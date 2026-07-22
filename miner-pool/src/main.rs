//! hac-pool: public free-IP mining pool for Hacash.
//!
//! - HTTP: same miner RPC as fullnode so existing poworker/diaworker can connect.
//! - Stratum TCP: minimal JSON-RPC lines for third-party / multi-worker clients.
//! - Upstream: any fullnode with `[server]` miner API enabled.
//!
//! Requirements covered (community list):
//! 1+5 free IP pool + broadcast work, 3 official protocol, 4 workers unchanged.

mod config;
mod job;
mod rpc_proxy;
mod stratum;
mod upstream;

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tracing::{info, warn};

use crate::config::PoolArgs;
use crate::job::JobHub;
use crate::upstream::Upstream;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "miner_pool=info,hac_pool=info".into()),
        )
        .init();

    let args = PoolArgs::parse();
    if let Err(e) = args.validate() {
        eprintln!("config error: {e}");
        std::process::exit(2);
    }

    let hub = Arc::new(JobHub::new());
    let upstream = Upstream::new(
        args.upstream.clone(),
        args.upstream_token.clone(),
        hub.clone(),
    );

    // Background job refresh from fullnode
    let up = upstream.clone();
    tokio::spawn(async move {
        up.run_poll_loop(args.poll_ms).await;
    });

    let http_addr: SocketAddr = args
        .http_bind
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:3333".parse().unwrap());
    let stratum_addr: SocketAddr = args
        .stratum_bind
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:3334".parse().unwrap());

    info!(
        "hac-pool starting free-IP pool upstream={} http={} stratum={} token={}",
        args.upstream,
        http_addr,
        stratum_addr,
        if args.pool_token.is_empty() {
            "none (open)"
        } else {
            "required"
        }
    );
    if !http_addr.ip().is_loopback() && args.pool_token.is_empty() {
        warn!(
            "HTTP bind {} is public without --pool-token; anyone can use this pool",
            http_addr
        );
    }

    let hub_http = hub.clone();
    let token_http = args.pool_token.clone();
    let up_http = upstream.clone();
    let http = tokio::spawn(async move {
        if let Err(e) = rpc_proxy::serve(http_addr, hub_http, up_http, token_http).await {
            eprintln!("HTTP pool server error: {e}");
        }
    });

    let hub_st = hub.clone();
    let token_st = args.pool_token.clone();
    let up_st = upstream.clone();
    let stratum = tokio::spawn(async move {
        if let Err(e) = stratum::serve(stratum_addr, hub_st, up_st, token_st).await {
            eprintln!("Stratum server error: {e}");
        }
    });

    info!("poworker connect = {}", http_addr);
    info!("stratum connect  = {}", stratum_addr);
    info!("anyone can point workers at this host:port (free IP pool broadcast)");

    tokio::select! {
        _ = http => {},
        _ = stratum => {},
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal");
        }
    }
}
