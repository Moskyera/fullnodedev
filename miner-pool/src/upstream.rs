use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value as JV;
use tracing::{debug, info, warn};

use crate::job::JobHub;

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
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .no_proxy()
                .build()
                .unwrap_or_else(|_| Client::new()),
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
        let url = format!(
            "http://{}/submit/miner/success?height={height}&block_nonce={block_nonce}&coinbase_nonce={coinbase_nonce}&t={}",
            self.base,
            now_ms()
        );
        let req = self.apply_token(self.client.get(&url));
        let resp = req.send().await.map_err(|e| e.to_string())?;
        resp.text().await.map_err(|e| e.to_string())
    }

    pub async fn run_poll_loop(&self, poll_ms: u64) {
        let mut last_h = 0u64;
        loop {
            match self.fetch_pending().await {
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
