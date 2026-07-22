//! Minimal Stratum-style JSON-RPC over TCP (line-delimited).
//!
//! Methods:
//! - mining.subscribe
//! - mining.authorize (password = pool token if set)
//! - mining.notify is pushed when job changes
//! - mining.submit → forwarded to fullnode
//! - mining.get_job (helper returning full Hacash pending JSON)

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{Value as JV, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::job::JobHub;
use crate::upstream::Upstream;

pub async fn serve(
    addr: SocketAddr,
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("stratum bind {addr}: {e}"))?;
    info!("Stratum listening on {addr}");
    loop {
        let (sock, peer) = listener.accept().await.map_err(|e| e.to_string())?;
        let hub = hub.clone();
        let upstream = upstream.clone();
        let token = pool_token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(sock, peer, hub, upstream, token).await {
                warn!("stratum {peer}: {e}");
            }
        });
    }
}

async fn handle_client(
    sock: TcpStream,
    peer: SocketAddr,
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
) -> Result<(), String> {
    let (reader, mut writer) = sock.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut authorized = pool_token.is_empty();
    let mut last_job = String::new();
    let mut worker = peer.to_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);

    let hub_push = hub.clone();
    let push_tx = tx.clone();
    tokio::spawn(async move {
        let mut last = String::new();
        loop {
            if let Some(job) = hub_push.current() {
                if job.job_id != last {
                    last = job.job_id.clone();
                    let intro = job
                        .raw
                        .get("block_intro")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let notify = json!({
                        "id": null,
                        "method": "mining.notify",
                        "params": [
                            job.job_id,
                            job.height,
                            intro,
                            job.raw.get("target").cloned().unwrap_or(JV::Null)
                        ]
                    });
                    if push_tx.send(notify.to_string()).await.is_err() {
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    loop {
        tokio::select! {
            maybe_line = lines.next_line() => {
                let line = match maybe_line {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => return Err(e.to_string()),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let req: JV = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                let id = req.get("id").cloned().unwrap_or(JV::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let params = req.get("params").cloned().unwrap_or(JV::Array(vec![]));

                let reply = match method {
                    "mining.subscribe" => {
                        json!({
                            "id": id,
                            "result": [ [["mining.notify", "hacash"]], "00", 4 ],
                            "error": null
                        })
                    }
                    "mining.authorize" => {
                        let pass = params
                            .as_array()
                            .and_then(|a| a.get(1))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let user = params
                            .as_array()
                            .and_then(|a| a.get(0))
                            .and_then(|v| v.as_str())
                            .unwrap_or("worker");
                        worker = user.to_string();
                        if pool_token.is_empty() || pass == pool_token {
                            authorized = true;
                            if let Some(job) = hub.current() {
                                last_job = job.job_id.clone();
                                let intro = job
                                    .raw
                                    .get("block_intro")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let notify = json!({
                                    "id": null,
                                    "method": "mining.notify",
                                    "params": [job.job_id, job.height, intro, JV::Null]
                                });
                                let _ = tx.send(notify.to_string()).await;
                            }
                            json!({"id": id, "result": true, "error": null})
                        } else {
                            json!({"id": id, "result": false, "error": [24, "unauthorized", null]})
                        }
                    }
                    "mining.get_job" => {
                        if !authorized {
                            json!({"id": id, "result": null, "error": [24, "unauthorized", null]})
                        } else if let Some(job) = hub.current() {
                            json!({"id": id, "result": job.raw, "error": null})
                        } else {
                            json!({"id": id, "result": null, "error": [20, "no job", null]})
                        }
                    }
                    "mining.submit" => {
                        if !authorized {
                            json!({"id": id, "result": false, "error": [24, "unauthorized", null]})
                        } else {
                            // params: [worker, job_id, block_nonce, coinbase_nonce]
                            let arr = params.as_array().cloned().unwrap_or_default();
                            let job_id = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                            let block_nonce = arr.get(2).and_then(|v| v.as_str()).unwrap_or("");
                            let coinbase_nonce = arr
                                .get(3)
                                .and_then(|v| v.as_str())
                                .unwrap_or("00");
                            let height = job_id
                                .strip_prefix('h')
                                .and_then(|s| s.parse::<u64>().ok())
                                .or_else(|| hub.current().map(|j| j.height))
                                .unwrap_or(0);
                            match upstream
                                .submit_success(height, block_nonce, coinbase_nonce)
                                .await
                            {
                                Ok(body) => {
                                    info!("stratum submit from {worker} height={height}: {body}");
                                    let ok = !body.contains("\"ret\":1");
                                    json!({"id": id, "result": ok, "error": null})
                                }
                                Err(e) => {
                                    json!({"id": id, "result": false, "error": [20, e, null]})
                                }
                            }
                        }
                    }
                    "" => json!({"id": id, "result": null, "error": [20, "missing method", null]}),
                    other => json!({
                        "id": id,
                        "result": null,
                        "error": [20, format!("unknown method {other}"), null]
                    }),
                };

                let mut out = reply.to_string();
                out.push('\n');
                writer
                    .write_all(out.as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Some(push) = rx.recv() => {
                if !authorized && !pool_token.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<JV>(&push) {
                    if let Some(jid) = v
                        .get("params")
                        .and_then(|p| p.as_array())
                        .and_then(|a| a.first())
                        .and_then(|x| x.as_str())
                    {
                        if jid == last_job {
                            continue;
                        }
                        last_job = jid.to_string();
                    }
                }
                let mut out = push;
                out.push('\n');
                if writer.write_all(out.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}
