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
    axum::serve(listener, app).await.map_err(|e| e.to_string())
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

/// Cap on the slice of an upstream body echoed back to the miner as `msg`. The
/// body is already bounded upstream; this keeps the reply itself small and
/// readable in a worker log.
const MAX_MSG_CHARS: usize = 512;

/// Char-safe clip. The body is whatever the upstream sent, so it may be non-ASCII
/// and must never be byte-sliced.
fn clip_msg(body: &str) -> String {
    let mut out: String = body.chars().take(MAX_MSG_CHARS).collect();
    if body.chars().nth(MAX_MSG_CHARS).is_some() {
        out.push_str("...(truncated)");
    }
    out
}

/// Turn an upstream 2xx body into the reply the miner sees.
///
/// Only a body that actually carries a numeric `ret` is the node's verdict, so
/// only that is passed through verbatim. Everything else - an HTML error page, a
/// proxy notice, a truncated reply, or valid JSON with no `ret` at all - is
/// reported as a definite failure. Both consumers key acceptance on `ret == 0`,
/// and poworker treats a MISSING `ret` as transient and retries up to 5 times,
/// each of which re-enters `Upstream::submit_success` with its own 5 attempts: up
/// to 25 upstream submits for one solution, at the worst possible moment.
/// Answering `ret:1` makes the rejection definite and stops that burst.
///
/// This never turns a rejection into a success: the fallback is always `ret:1`.
fn normalise_submit_body(body: &str) -> JV {
    if let Ok(v) = serde_json::from_str::<JV>(body) {
        if v.get("ret").and_then(|r| r.as_i64()).is_some() {
            return v;
        }
    }
    json!({"ret": 1, "msg": clip_msg(body)})
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
        Ok(body) => (StatusCode::OK, Json(normalise_submit_body(&body))),
        Err(e) => (StatusCode::OK, Json(json!({"err": e, "ret": 1}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_node_verdict_is_passed_through_untouched() {
        // ret:0 is an accepted block and ret:1 a definite rejection. Neither may be
        // rewritten: the miner counts its blocks off exactly this.
        let ok = normalise_submit_body("{\"ret\":0,\"height\":100,\"mining\":\"success\"}");
        assert_eq!(ok["ret"].as_i64(), Some(0));
        assert_eq!(ok["mining"].as_str(), Some("success"));
        let no = normalise_submit_body("{\"ret\":1,\"err\":\"height passed\"}");
        assert_eq!(no["ret"].as_i64(), Some(1));
        assert_eq!(no["err"].as_str(), Some("height passed"));
    }

    #[test]
    fn a_2xx_body_without_a_ret_is_reported_as_a_definite_failure() {
        // Valid JSON with no `ret` is not a node verdict. Relaying it verbatim made
        // poworker treat it as transient and retry 5 times, each retry costing
        // another 5 upstream submits. It must come back as ret:1 so the miner stops.
        for body in [
            "{\"err\":\"pending block not ready\"}",
            "{}",
            "[]",
            "\"maintenance\"",
            "<html>502 Bad Gateway</html>",
            "",
        ] {
            let v = normalise_submit_body(body);
            assert_eq!(
                v["ret"].as_i64(),
                Some(1),
                "body {body:?} was not normalised"
            );
            assert!(v.get("msg").is_some(), "body {body:?} lost its detail");
        }
        // A non-numeric `ret` is not a verdict either: both consumers read it with
        // as_i64, so a string would land in the same retry loop.
        assert_eq!(
            normalise_submit_body("{\"ret\":\"0\"}")["ret"].as_i64(),
            Some(1)
        );
    }

    #[test]
    fn an_oversized_upstream_body_is_clipped_before_it_is_echoed() {
        let body = "x".repeat(MAX_MSG_CHARS * 4);
        let v = normalise_submit_body(&body);
        let msg = v["msg"].as_str().unwrap();
        assert!(
            msg.chars().count() <= MAX_MSG_CHARS + "...(truncated)".chars().count(),
            "msg not clipped: {} chars",
            msg.chars().count()
        );
        // Char-safe: an upstream body is not guaranteed ASCII, and clipping it must
        // not panic on a multi-byte codepoint.
        let wide = "\u{03b1}".repeat(MAX_MSG_CHARS * 2);
        assert_eq!(normalise_submit_body(&wide)["ret"].as_i64(), Some(1));
    }
}
