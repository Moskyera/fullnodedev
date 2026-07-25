//! Minimal Stratum-style JSON-RPC over TCP (line-delimited).
//!
//! Methods:
//! - mining.subscribe
//! - mining.authorize (password = pool token if set)
//! - mining.notify is pushed when job changes
//! - mining.submit → forwarded to fullnode
//! - mining.get_job (helper returning full Hacash pending JSON)
//!
//! Hardened for a public, unauthenticated TCP port: connections are capped, each
//! line is length-bounded (no unbounded buffering / OOM), idle sockets are
//! dropped, a single malformed line replies with an error instead of killing the
//! session, and one accept() error never tears down the whole listener.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as JV, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::job::JobHub;
use crate::upstream::Upstream;

/// Max concurrent stratum connections. Bounds sockets/tasks/memory so a flood
/// cannot exhaust file descriptors or RAM.
const MAX_CONNS: usize = 1024;
/// Max bytes in a single JSON-RPC line. A well-formed request is a few hundred
/// bytes; anything past this is garbage or an OOM attempt, so drop the peer.
const MAX_LINE: usize = 64 * 1024;
/// Drop a connection that sends nothing for this long (slow-loris / dead peer).
/// Generous so a legitimately slow miner between shares is not disconnected.
/// Measured from the last byte actually received from the client: outbound job
/// pushes must not renew it, or a silent peer would hold its slot forever on an
/// active chain.
const READ_IDLE: Duration = Duration::from_secs(600);

/// Tracks concurrent connections per source IP so a single peer cannot pin every
/// slot of the global cap and lock legitimate miners out. `max` of 0 disables the
/// per-IP limit.
struct IpLimiter {
    inner: Mutex<HashMap<IpAddr, usize>>,
    max: usize,
}

/// Releases the per-IP slot when the connection ends.
struct IpGuard {
    limiter: Arc<IpLimiter>,
    ip: IpAddr,
    counted: bool,
}

impl IpLimiter {
    fn new(max: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max,
        }
    }

    fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpGuard> {
        if self.max == 0 {
            return Some(IpGuard {
                limiter: self.clone(),
                ip,
                counted: false,
            });
        }
        // Poison-tolerant: a poisoned counter must never stop the pool accepting
        // miners. The guarded value is a plain counter map.
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let n = g.entry(ip).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(IpGuard {
            limiter: self.clone(),
            ip,
            counted: true,
        })
    }
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        let mut g = self.limiter.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.remove(&self.ip);
            }
        }
    }
}

/// Constant-time string compare for the pool token, so a timing side-channel does
/// not leak it byte by byte. (Length is not secret.)
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

/// Aborts a spawned task when dropped, so a per-connection push task cannot leak
/// past the connection it serves.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Incrementally frames newline-delimited lines from raw reads with a hard cap on
/// a single line's length.
struct LineFramer {
    buf: Vec<u8>,
    max: usize,
}
impl LineFramer {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }
    /// Pull one complete line if buffered; Err if a single line exceeds the cap.
    fn take_line(&mut self) -> std::io::Result<Option<String>> {
        if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&line).trim().to_string()));
        }
        if self.buf.len() > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stratum line exceeds length cap",
            ));
        }
        Ok(None)
    }
}

pub async fn serve(
    addr: SocketAddr,
    hub: Arc<JobHub>,
    upstream: Upstream,
    pool_token: String,
    job_ttl: Duration,
    max_conns_per_ip: usize,
) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("stratum bind {addr}: {e}"))?;
    info!("Stratum listening on {addr}");
    let sem = Arc::new(Semaphore::new(MAX_CONNS));
    let ip_limiter = Arc::new(IpLimiter::new(max_conns_per_ip));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            // A single accept() error (e.g. EMFILE under load) must not kill the
            // whole listener — log and keep serving.
            Err(e) => {
                warn!("stratum accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // Refuse past the connection cap instead of unbounded spawning.
        let Ok(permit) = sem.clone().try_acquire_owned() else {
            warn!("stratum at capacity ({MAX_CONNS}); dropping {peer}");
            drop(sock);
            continue;
        };
        // Refuse past the per-IP cap so one peer cannot monopolise every slot.
        let Some(ip_guard) = ip_limiter.try_acquire(peer.ip()) else {
            warn!("stratum per-IP cap ({max_conns_per_ip}) reached; dropping {peer}");
            drop(sock);
            continue;
        };
        let hub = hub.clone();
        let upstream = upstream.clone();
        let token = pool_token.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            let _ip_guard = ip_guard; // ditto for the per-IP slot
            if let Err(e) = handle_client(sock, peer, hub, upstream, token, job_ttl).await {
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
    job_ttl: Duration,
) -> Result<(), String> {
    let (mut reader, mut writer) = sock.into_split();
    let mut authorized = pool_token.is_empty();
    let mut last_job = String::new();
    let mut worker = peer.to_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);

    let hub_push = hub.clone();
    let push_tx = tx.clone();
    // Aborted automatically when this connection ends, so the push task never
    // lingers past its client.
    let _push_guard = AbortOnDrop(tokio::spawn(async move {
        let mut last = String::new();
        loop {
            // Only push work upstream is still refreshing: a frozen height would
            // burn the miner's hashrate for the whole outage.
            if let Some(job) = hub_push.current_fresh(job_ttl) {
                if job.job_id != last {
                    last = job.job_id.clone();
                    if push_tx.send(notify_line(&job)).await.is_err() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }));

    let mut framer = LineFramer::new(MAX_LINE);
    let mut read_buf = [0u8; 8 * 1024];
    // Inbound-idle clock. Advanced only by bytes actually received from the
    // client, never by an outbound job push, so a peer that merely drains the
    // socket is still reaped after READ_IDLE.
    let mut last_read = tokio::time::Instant::now();

    loop {
        // Drain any complete lines already buffered before reading more.
        match framer.take_line() {
            Ok(Some(line)) => {
                if line.is_empty() {
                    continue;
                }
                if !process_line(
                    &line,
                    &hub,
                    &upstream,
                    &pool_token,
                    &tx,
                    &mut authorized,
                    &mut worker,
                    &mut writer,
                    job_ttl,
                )
                .await?
                {
                    break; // writer closed
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_string()), // oversized line: drop peer
        }

        // Remaining idle budget since the last byte from the client. Recomputed
        // every iteration so a push that restarts the loop cannot reset it.
        let idle_left = READ_IDLE
            .checked_sub(last_read.elapsed())
            .unwrap_or(Duration::ZERO);
        if idle_left.is_zero() {
            break; // idle timeout
        }

        tokio::select! {
            r = tokio::time::timeout(idle_left, reader.read(&mut read_buf)) => {
                let n = match r {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => return Err(e.to_string()),
                    Err(_) => break, // idle timeout
                };
                if n == 0 {
                    break; // EOF
                }
                last_read = tokio::time::Instant::now();
                framer.buf.extend_from_slice(&read_buf[..n]);
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

/// Build a mining.notify line carrying the real Hacash job fields a client needs
/// to mine from the push alone: the exact `target_hash` (not the absent `target`)
/// and the `coinbase_body` used to set the miner nonce and recompute the merkle
/// root.
fn notify_line(job: &crate::job::MiningJob) -> String {
    let intro = job
        .raw
        .get("block_intro")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let target = job.raw.get("target_hash").cloned().unwrap_or(JV::Null);
    let coinbase = job.raw.get("coinbase_body").cloned().unwrap_or(JV::Null);
    json!({
        "id": null,
        "method": "mining.notify",
        "params": [job.job_id, job.height, intro, target, coinbase]
    })
    .to_string()
}

/// Handle one request line. Returns Ok(false) if the writer closed (end session),
/// Ok(true) to keep going. A malformed line replies with a JSON-RPC error and
/// keeps the session instead of dropping the connection.
#[allow(clippy::too_many_arguments)]
async fn process_line(
    line: &str,
    hub: &Arc<JobHub>,
    upstream: &Upstream,
    pool_token: &str,
    tx: &tokio::sync::mpsc::Sender<String>,
    authorized: &mut bool,
    worker: &mut String,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    job_ttl: Duration,
) -> Result<bool, String> {
    let req: JV = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // Content error, not a transport error: reply and keep the session.
            let reply = json!({"id": JV::Null, "result": JV::Null, "error": [20, format!("bad json: {e}"), null]});
            return write_line(writer, &reply.to_string()).await;
        }
    };
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
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("worker");
            *worker = user.to_string();
            if pool_token.is_empty() || ct_eq(pass, pool_token) {
                *authorized = true;
                if let Some(job) = hub.current_fresh(job_ttl) {
                    // Enqueue only. The deduped push channel owns `last_job`;
                    // pre-seeding it here would make the reader drop this very
                    // notify (and the push task's copy of it), leaving a
                    // just-authorized miner idle until the next height change.
                    let _ = tx.send(notify_line(&job)).await;
                }
                json!({"id": id, "result": true, "error": null})
            } else {
                json!({"id": id, "result": false, "error": [24, "unauthorized", null]})
            }
        }
        "mining.get_job" => {
            if !*authorized {
                json!({"id": id, "result": null, "error": [24, "unauthorized", null]})
            } else if let Some(job) = hub.current_fresh(job_ttl) {
                json!({"id": id, "result": job.raw, "error": null})
            } else {
                json!({"id": id, "result": null, "error": [20, "no job", null]})
            }
        }
        "mining.submit" => {
            if !*authorized {
                json!({"id": id, "result": false, "error": [24, "unauthorized", null]})
            } else {
                let arr = params.as_array().cloned().unwrap_or_default();
                let job_id = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                let block_nonce = arr.get(2).and_then(|v| v.as_str()).unwrap_or("");
                let coinbase_nonce = arr.get(3).and_then(|v| v.as_str()).unwrap_or("00");
                let height = crate::job::job_height(job_id)
                    .or_else(|| hub.current().map(|j| j.height))
                    .unwrap_or(0);
                match upstream
                    .submit_success(height, block_nonce, coinbase_nonce)
                    .await
                {
                    Ok(body) => {
                        info!("stratum submit from {worker} height={height}: {body}");
                        // Acceptance is strictly ret==0 from a parseable JSON reply.
                        let ok = serde_json::from_str::<JV>(&body)
                            .ok()
                            .and_then(|v| v["ret"].as_i64())
                            == Some(0);
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

    write_line(writer, &reply.to_string()).await
}

async fn write_line(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> Result<bool, String> {
    let mut out = String::with_capacity(line.len() + 1);
    out.push_str(line);
    out.push('\n');
    match writer.write_all(out.as_bytes()).await {
        Ok(()) => Ok(true),
        Err(_) => Ok(false), // writer closed: end the session
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn test_hub(height: u64) -> Arc<JobHub> {
        let hub = Arc::new(JobHub::new());
        hub.update(
            height,
            json!({
                "height": height,
                "block_intro": "aa",
                "target_hash": "ff",
                "coinbase_body": "bb"
            }),
        );
        hub
    }

    /// Spawn a pool on an ephemeral port and return a connected client socket.
    async fn connect_to_pool(hub: Arc<JobHub>, job_ttl: Duration) -> TcpStream {
        let upstream = Upstream::new("127.0.0.1:1".to_string(), String::new(), hub.clone());
        // Reserve an ephemeral port, then let serve() bind it.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        tokio::spawn(serve(addr, hub, upstream, String::new(), job_ttl, 0));
        for _ in 0..100 {
            if let Ok(s) = TcpStream::connect(addr).await {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("stratum never came up");
    }

    /// Collect mining.notify job ids for `window`, returning what the client
    /// actually received on the wire.
    async fn drain_notifies(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
        window: Duration,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while tokio::time::Instant::now() < deadline {
            let next = tokio::time::timeout(Duration::from_millis(200), lines.next_line()).await;
            let Ok(Ok(Some(line))) = next else {
                continue;
            };
            let v: JV = serde_json::from_str(&line).expect("server sent valid json");
            if v.get("method").and_then(|m| m.as_str()) == Some("mining.notify") {
                seen.push(v["params"][0].as_str().unwrap_or("").to_string());
            }
        }
        seen
    }

    /// A miner that authorizes just after a height change must be given the new
    /// job exactly once. Pre-fix the authorize handler pre-seeded `last_job` with
    /// the id it was about to enqueue, so the reader deduped away both its own
    /// notify and the push task's copy, and the miner sat idle until the NEXT
    /// height change.
    #[tokio::test]
    async fn authorize_after_a_height_change_delivers_exactly_one_notify() {
        let hub = test_hub(100);
        let sock = connect_to_pool(hub.clone(), Duration::from_secs(60)).await;
        let (r, mut w) = sock.into_split();
        let mut lines = BufReader::new(r).lines();

        // Wait for the connect-time push, which leaves the push task sleeping out
        // its 500ms cycle with last=h100.
        let first = drain_notifies(&mut lines, Duration::from_millis(400)).await;
        assert_eq!(first, vec!["h100_aa".to_string()]);

        // New block arrives, then the miner authorizes before the push task wakes.
        hub.update(
            101,
            json!({"height": 101, "block_intro": "aa", "target_hash": "ff", "coinbase_body": "bb"}),
        );
        w.write_all(b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"w\",\"\"]}\n")
            .await
            .unwrap();

        // Long enough for the push task's copy to arrive as well, so a duplicate
        // would also be caught.
        let after = drain_notifies(&mut lines, Duration::from_secs(2)).await;
        assert_eq!(after, vec!["h101_aa".to_string()]);
    }

    /// A miner that pipelines subscribe+authorize in one segment must still get
    /// its first job exactly once.
    #[tokio::test]
    async fn pipelined_subscribe_and_authorize_delivers_exactly_one_notify() {
        let hub = test_hub(100);
        let sock = connect_to_pool(hub, Duration::from_secs(60)).await;
        let (r, mut w) = sock.into_split();
        w.write_all(
            b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[]}\n\
              {\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"w\",\"\"]}\n",
        )
        .await
        .unwrap();

        let mut lines = BufReader::new(r).lines();
        let seen = drain_notifies(&mut lines, Duration::from_secs(2)).await;
        assert_eq!(seen, vec!["h100_aa".to_string()]);
    }

    /// A job upstream has stopped refreshing must not be handed out as work.
    #[tokio::test]
    async fn stale_job_is_not_pushed_to_a_new_miner() {
        let hub = test_hub(100);
        // A ttl of 1ms means the job is already stale by the time anyone connects.
        let sock = connect_to_pool(hub, Duration::from_millis(1)).await;
        let (r, mut w) = sock.into_split();
        w.write_all(b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"w\",\"\"]}\n")
            .await
            .unwrap();

        let mut lines = BufReader::new(r).lines();
        let seen = drain_notifies(&mut lines, Duration::from_secs(2)).await;
        assert!(seen.is_empty(), "stale work must not be pushed: {seen:?}");
    }

    #[test]
    fn ip_limiter_caps_and_releases_per_source_ip() {
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        let other: IpAddr = "10.0.0.8".parse().unwrap();
        let lim = Arc::new(IpLimiter::new(2));
        let a = lim.try_acquire(ip).expect("first slot");
        let b = lim.try_acquire(ip).expect("second slot");
        assert!(lim.try_acquire(ip).is_none(), "third slot must be refused");
        // A different peer is unaffected by one IP hitting its cap.
        assert!(lim.try_acquire(other).is_some());
        drop(a);
        assert!(lim.try_acquire(ip).is_some(), "slot freed on drop");
        drop(b);

        // 0 disables the cap entirely.
        let open = Arc::new(IpLimiter::new(0));
        for _ in 0..100 {
            assert!(open.try_acquire(ip).is_some());
        }
    }
}
