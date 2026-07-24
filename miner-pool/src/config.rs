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

    /// Max concurrent stratum connections from one source IP (0 = unlimited).
    /// Stops one peer pinning every connection slot; raise it for a large farm
    /// behind a single NAT.
    #[arg(
        long,
        env = "HAC_POOL_MAX_CONNS_PER_IP",
        default_value_t = 128
    )]
    pub max_conns_per_ip: usize,
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

    /// How old a mirrored job may be before workers are told upstream is stale.
    pub fn job_ttl(&self) -> std::time::Duration {
        crate::job::job_ttl(self.poll_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_bad_input_and_job_ttl_follows_poll_ms() {
        let mut a = PoolArgs::parse_from(["hac-pool"]);
        assert!(a.validate().is_ok());
        assert_eq!(a.job_ttl(), std::time::Duration::from_secs(15));
        a.poll_ms = 10_000;
        assert_eq!(a.job_ttl(), std::time::Duration::from_secs(40));
        a.poll_ms = 100;
        assert!(a.validate().is_err());
        a.poll_ms = 2000;
        a.upstream = "  ".into();
        assert!(a.validate().is_err());
    }
}
