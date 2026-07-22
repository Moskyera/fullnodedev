use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "hac-pool",
    about = "Hacash public free-IP mining pool (HTTP miner RPC + Stratum)",
    version
)]
pub struct PoolArgs {
    /// Upstream fullnode miner RPC host:port (e.g. 127.0.0.1:8080)
    #[arg(long, env = "HAC_POOL_UPSTREAM", default_value = "127.0.0.1:8080")]
    pub upstream: String,

    /// Optional X-Api-Token for upstream fullnode
    #[arg(long, env = "HAC_POOL_UPSTREAM_TOKEN", default_value = "")]
    pub upstream_token: String,

    /// HTTP miner-RPC listen address (free IP: 0.0.0.0:3333)
    #[arg(long, env = "HAC_POOL_HTTP_BIND", default_value = "0.0.0.0:3333")]
    pub http_bind: String,

    /// Stratum TCP listen address
    #[arg(long, env = "HAC_POOL_STRATUM_BIND", default_value = "0.0.0.0:3334")]
    pub stratum_bind: String,

    /// Optional pool token (X-Api-Token / stratum password). Empty = open free pool.
    #[arg(long, env = "HAC_POOL_TOKEN", default_value = "")]
    pub pool_token: String,

    /// How often to refresh pending work from upstream (ms)
    #[arg(long, env = "HAC_POOL_POLL_MS", default_value_t = 2000)]
    pub poll_ms: u64,
}

impl PoolArgs {
    pub fn validate(&self) -> Result<(), String> {
        if self.upstream.trim().is_empty() {
            return Err("upstream must not be empty".into());
        }
        if self.poll_ms < 200 {
            return Err("poll_ms must be >= 200".into());
        }
        Ok(())
    }
}
