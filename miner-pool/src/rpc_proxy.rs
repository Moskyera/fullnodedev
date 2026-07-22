//! Fullnode-compatible miner HTTP RPC so existing poworker can use the pool.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value as JV, json};
use tracing::info;

use crate::job::JobHub;
use crate::upstream::Upstream;

#[derive(Clone)]
struct AppState {
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
}

pub async fn serve(
    addr: SocketAddr,
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
) -> Result<(), String> {
    let state = AppState {
        hub,
        upstream,
        pool_token,
    };
    let app = Router::new()
        .route("/_server_", get(|| async { "Hacash Pool (miner RPC)" }))
        .route("/query/miner/pending", get(pending))
        .route("/query/miner/notice", get(notice))
        .route("/submit/miner/success", get(submit))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    info!("HTTP miner RPC listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| e.to_string())
}

fn auth_ok(state: &AppState, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    if state.pool_token.is_empty() {
        return true;
    }
    if let Some(v) = headers.get("x-api-token").and_then(|v| v.to_str().ok()) {
        if v.trim() == state.pool_token {
            return true;
        }
    }
    if let Some(v) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let t = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
            .unwrap_or(v)
            .trim();
        if t == state.pool_token {
            return true;
        }
    }
    if query.get("api_token").map(|s| s.as_str()) == Some(state.pool_token.as_str()) {
        return true;
    }
    false
}

async fn pending(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !auth_ok(&state, &headers, &q) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"err": "unauthorized"})),
        );
    }
    match state.hub.current() {
        Some(job) => (StatusCode::OK, Json(job.raw)),
        None => (
            StatusCode::OK,
            Json(json!({"err": "no job yet; wait for upstream fullnode"})),
        ),
    }
}

async fn notice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !auth_ok(&state, &headers, &q) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"err": "unauthorized"})),
        );
    }
    let want = q
        .get("height")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let wait_s = q
        .get("wait")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(45)
        .min(120);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_s.max(1));
    loop {
        let h = state.hub.height();
        if h > want {
            return (StatusCode::OK, Json(json!({"height": h})));
        }
        if tokio::time::Instant::now() >= deadline {
            return (StatusCode::OK, Json(json!({"height": h})));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !auth_ok(&state, &headers, &q) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"err": "unauthorized", "ret": 1})),
        );
    }
    let height = q
        .get("height")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let block_nonce = q.get("block_nonce").cloned().unwrap_or_default();
    let coinbase_nonce = q.get("coinbase_nonce").cloned().unwrap_or_default();
    if height == 0 || block_nonce.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({"err": "missing height or block_nonce", "ret": 1})),
        );
    }
    match state
        .upstream
        .submit_success(height, &block_nonce, &coinbase_nonce)
        .await
    {
        Ok(body) => {
            // Pass through upstream JSON if possible
            if let Ok(v) = serde_json::from_str::<JV>(&body) {
                (StatusCode::OK, Json(v))
            } else {
                (StatusCode::OK, Json(json!({"ret": 0, "msg": body})))
            }
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({"err": e, "ret": 1})),
        ),
    }
}
