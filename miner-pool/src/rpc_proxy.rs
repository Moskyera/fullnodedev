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
    /// How old a mirrored job may be before it counts as absent. Serving a frozen
    /// height during an upstream outage would burn every worker's hashrate.
    job_ttl: Duration,
}

pub async fn serve(
    addr: SocketAddr,
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
    job_ttl: Duration,
) -> Result<(), String> {
    let state = AppState {
        hub,
        upstream,
        pool_token,
        job_ttl,
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

/// Constant-time compare so a timing side-channel cannot leak the token byte by
/// byte. (Length is not treated as secret.)
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn auth_ok(state: &AppState, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    if state.pool_token.is_empty() {
        return true;
    }
    if let Some(v) = headers.get("x-api-token").and_then(|v| v.to_str().ok()) {
        if ct_eq(v.trim(), &state.pool_token) {
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
        if ct_eq(t, &state.pool_token) {
            return true;
        }
    }
    if let Some(v) = query.get("api_token") {
        if ct_eq(v, &state.pool_token) {
            return true;
        }
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
    match state.hub.current_fresh(state.job_ttl) {
        Some(job) => (StatusCode::OK, Json(job.raw)),
        // A job upstream has stopped refreshing is dead work: the network has
        // moved past that height, so report the outage instead of handing it out.
        None if state.hub.height() > 0 => (
            StatusCode::OK,
            Json(json!({"err": "upstream stale; work is not being refreshed"})),
        ),
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
        // Only a fresh job advertises new work. When it is stale the long-poller
        // is told so explicitly, instead of silently echoing a frozen height it
        // cannot distinguish from a quiet chain.
        let h = state.hub.height_fresh(state.job_ttl);
        if h > want {
            return (StatusCode::OK, Json(json!({"height": h})));
        }
        if tokio::time::Instant::now() >= deadline {
            if h == 0 && state.hub.height() > 0 {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "err": "upstream stale; work is not being refreshed",
                        "height": state.hub.height()
                    })),
                );
            }
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
            // Pass through upstream JSON if possible. A non-JSON body is an
            // unexpected/error response (HTML error page, proxy notice, truncated
            // reply), so it must NOT be reported as success (ret:0) to the miner:
            // treat it as a failure so the miner does not count a lost block as won.
            if let Ok(v) = serde_json::from_str::<JV>(&body) {
                (StatusCode::OK, Json(v))
            } else {
                (StatusCode::OK, Json(json!({"ret": 1, "msg": body})))
            }
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({"err": e, "ret": 1})),
        ),
    }
}
