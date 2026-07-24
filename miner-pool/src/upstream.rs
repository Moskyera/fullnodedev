use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value as JV;
use tracing::{debug, info, warn};

use crate::job::JobHub;

/// Request timeout for ordinary upstream calls (job polling).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Submits get their own, longer budget: a busy fullnode validates the whole
/// block before answering, and that happens at exactly the moment a solution
/// arrives. Tripping the poll timeout there would discard a found block.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Attempts for a found-block submit. A block solution is the highest value
/// event in mining, so a transient transport failure or a 5xx from upstream
/// must never be allowed to discard it on the first try.
const SUBMIT_ATTEMPTS: u32 = 5;
/// Backoff before the second submit attempt; doubled on each further retry
/// (250ms, 500ms, 1s, 2s) so the caller still answers the miner promptly.
const SUBMIT_BACKOFF: Duration = Duration::from_millis(250);

/// A failed submit attempt. `retryable` separates transport/5xx failures, where
/// a retry can still land the block, from a definitive upstream refusal that
/// re-sending cannot fix.
struct SubmitError {
    retryable: bool,
    msg: String,
}

/// Every client we actually run must carry a request timeout: a timeout-less
/// client turns a stalled upstream into an unbounded hang that wedges job
/// refresh and submits alike. The fallback therefore keeps the timeout and only
/// drops `no_proxy`, and an unbuildable client refuses to start rather than
/// silently degrading.
fn build_client() -> Client {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .no_proxy()
        .build()
        .unwrap_or_else(|e| {
            warn!("reqwest client build failed: {e}; retrying without no_proxy");
            Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("failed to build HTTP client")
        })
}

#[derive(Clone)]
pub struct Upstream {
    base: String,
    token: String,
    client: Client,
    hub: Arc<JobHub>,
}

impl Upstream {
    pub fn new(host_port: String, token: String, hub: Arc<JobHub>) -> Self {
        // Accept http:// or https:// prefixes and a trailing slash; we rebuild the
        // URL as http://{base}/... so a bare host:port is what we keep.
        let base = host_port
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Self {
            base,
            token: token.trim().to_string(),
            client: build_client(),
            hub,
        }
    }

    fn apply_token(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            req
        } else {
            req.header("x-api-token", &self.token)
        }
    }

    pub async fn fetch_pending(&self) -> Result<JV, String> {
        let url = format!(
            "http://{}/query/miner/pending?stuff=true&t={}",
            self.base,
            now_ms()
        );
        let req = self.apply_token(self.client.get(&url));
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("upstream HTTP {}", resp.status()));
        }
        resp.json::<JV>().await.map_err(|e| e.to_string())
    }

    pub async fn submit_success(
        &self,
        height: u64,
        block_nonce: &str,
        coinbase_nonce: &str,
    ) -> Result<String, String> {
        // These come from an untrusted stratum client and are interpolated into
        // the upstream query string. A valid nonce is hex; rejecting anything else
        // both preserves correctness and closes a query-injection vector (a `&` or
        // space could otherwise inject/alter upstream query parameters).
        if !is_hex_nonce(block_nonce) || !is_hex_nonce(coinbase_nonce) {
            return Err(format!(
                "invalid nonce (hex only): block_nonce={block_nonce:?} coinbase_nonce={coinbase_nonce:?}"
            ));
        }
        // Losing a found block to one network hiccup is an unrecoverable loss of a
        // full block reward, so transport and 5xx failures are retried. This is
        // safe: the fullnode matches a submit by height and can only ever include
        // one block per height, so a re-submit after a lost reply is at worst a
        // no-op ("height not found") and can never pay twice. An HTTP 2xx body is
        // the node's verdict and is returned verbatim without retrying, so a
        // genuine stale-block rejection is never re-hammered.
        let mut backoff = SUBMIT_BACKOFF;
        let mut last_err = String::new();
        for attempt in 1..=SUBMIT_ATTEMPTS {
            let url = format!(
                "http://{}/submit/miner/success?height={height}&block_nonce={block_nonce}&coinbase_nonce={coinbase_nonce}&t={}",
                self.base,
                now_ms()
            );
            let req = self
                .apply_token(self.client.get(&url))
                .timeout(SUBMIT_TIMEOUT);
            match self.send_submit(req).await {
                Ok(body) => return Ok(body),
                Err(e) => {
                    last_err = e.msg;
                    if !e.retryable {
                        break;
                    }
                    warn!(
                        "submit height={height} attempt {attempt}/{SUBMIT_ATTEMPTS} failed: {last_err}"
                    );
                    if attempt < SUBMIT_ATTEMPTS {
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2);
                    }
                }
            }
        }
        Err(format!("submit height={height} failed: {last_err}"))
    }

    /// One submit attempt. Ok is returned only for an HTTP 2xx body, because that
    /// body is the node's authoritative accept/reject verdict and callers key
    /// acceptance on it. A non-2xx body is a transport/server failure and must
    /// never reach a miner as if it were a consensus verdict.
    async fn send_submit(&self, req: reqwest::RequestBuilder) -> Result<String, SubmitError> {
        let resp = req.send().await.map_err(|e| SubmitError {
            retryable: true,
            msg: e.to_string(),
        })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| SubmitError {
            retryable: true,
            msg: e.to_string(),
        })?;
        if status.is_success() {
            return Ok(body);
        }
        // 5xx / 408 / 429 are the upstream being unavailable or overloaded, which
        // a retry can still get past; other statuses (auth, bad request) will not
        // change on a re-send.
        let retryable = status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        Err(SubmitError {
            retryable,
            msg: format!(
                "upstream HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            ),
        })
    }

    pub async fn run_poll_loop(&self, poll_ms: u64) {
        let mut last_h = 0u64;
        let mut stale_logged = false;
        let ttl = crate::job::job_ttl(poll_ms);
        loop {
            // Belt and braces around the client-level timeout: job refresh must
            // never be wedged by a single stalled request.
            let fetched = match tokio::time::timeout(
                HTTP_TIMEOUT + Duration::from_secs(5),
                self.fetch_pending(),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err("upstream fetch timed out".to_string()),
            };
            match fetched {
                Ok(raw) => {
                    let height = raw
                        .get("height")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let has_intro = raw.get("block_intro").and_then(|v| v.as_str()).is_some();
                    if height > 0 && has_intro {
                        if height != last_h {
                            info!("upstream job height={height}");
                            last_h = height;
                        } else {
                            debug!("upstream job refresh height={height}");
                        }
                        self.hub.update(height, raw);
                    } else {
                        let err = raw
                            .get("err")
                            .and_then(|v| v.as_str())
                            .unwrap_or("no block_intro");
                        warn!("upstream pending incomplete: {err}");
                    }
                }
                Err(e) => warn!("upstream fetch failed: {e}"),
            }
            // Make the outage visible once, and again on recovery, so an operator
            // can tell a wedged refresher from a quiet chain.
            let stale = self.hub.current_fresh(ttl).is_none() && self.hub.height() > 0;
            if stale && !stale_logged {
                warn!("upstream job is stale (no refresh for >{}s); workers are being told to wait", ttl.as_secs());
                stale_logged = true;
            } else if !stale && stale_logged {
                info!("upstream job refresh recovered");
                stale_logged = false;
            }
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        }
    }
}

/// A valid PoW nonce is a non-empty, reasonably short hex string. Used to reject
/// injection attempts before interpolating into the upstream URL.
fn is_hex_nonce(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal HTTP/1.1 server replying with the scripted (status, body) pairs in
    /// order (the last entry repeats). Each connection is closed after one reply,
    /// so every submit attempt is an observable request. Returns the bind
    /// host:port and the request counter.
    async fn fake_upstream(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = hits_srv.fetch_add(1, Ordering::SeqCst);
                let (code, body) = script[n.min(script.len() - 1)];
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, hits)
    }

    fn upstream_at(addr: String) -> Upstream {
        Upstream::new(addr, String::new(), Arc::new(JobHub::new()))
    }

    #[tokio::test]
    async fn submit_retries_transient_5xx_and_returns_the_node_verdict() {
        // A found block must survive a transient upstream failure: the reward is
        // lost forever if the single attempt is discarded.
        let (addr, hits) = fake_upstream(vec![
            (503, "{\"ret\":1,\"err\":\"upstream busy\"}"),
            (502, "{\"ret\":1,\"err\":\"bad gateway\"}"),
            (200, "{\"ret\":0}"),
        ])
        .await;
        let up = upstream_at(addr);
        let body = up.submit_success(100, "aabb", "00").await.unwrap();
        assert_eq!(body, "{\"ret\":0}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn submit_never_reports_a_non_2xx_body_as_a_verdict() {
        // A 401 body that happens to say ret:0 is an error page, not consensus
        // acceptance, and re-sending cannot fix it.
        let (addr, hits) = fake_upstream(vec![(401, "{\"ret\":0}")]).await;
        let up = upstream_at(addr);
        let err = up.submit_success(100, "aabb", "00").await.unwrap_err();
        assert!(err.contains("upstream HTTP 401"), "unexpected error: {err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn submit_does_not_re_hammer_a_genuine_rejection() {
        // ret:1 with HTTP 200 is the node's real verdict (e.g. stale height) and
        // must reach the miner verbatim after exactly one attempt.
        let (addr, hits) = fake_upstream(vec![(200, "{\"ret\":1,\"err\":\"height passed\"}")]).await;
        let up = upstream_at(addr);
        let body = up.submit_success(100, "aabb", "00").await.unwrap();
        assert_eq!(body, "{\"ret\":1,\"err\":\"height passed\"}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn submit_rejects_a_non_hex_nonce_without_touching_upstream() {
        let (addr, hits) = fake_upstream(vec![(200, "{\"ret\":0}")]).await;
        let up = upstream_at(addr);
        assert!(up.submit_success(100, "aa&x=1", "00").await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
