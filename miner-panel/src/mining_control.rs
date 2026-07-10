//! Start/stop mining and worker process spawn.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::hacash_config::validate_wallet;
use crate::mining_kind::MiningKind;
use crate::platform;
use crate::MinerApp;

#[derive(Clone)]
pub(super) struct PendingStart {
    pub worker_path: PathBuf,
    pub deadline: Instant,
    /// When RPC first became reachable (fullnode needs ~30s warmup after that).
    pub rpc_ready_at: Option<Instant>,
}

impl MinerApp {
    pub(super) fn start_mining(&mut self) {
        let t = self.t();
        if self.mining {
            return;
        }
        if let Err(e) = validate_wallet(&self.wallet) {
            self.status_msg = if e == "empty" {
                t.wallet_required.to_string()
            } else {
                format!("{} {e}", t.wallet_invalid_prefix)
            };
            return;
        }
        if self.mining_kind == MiningKind::Hacd && self.bid_password.trim().is_empty() {
            self.status_msg = t.bid_password_required.to_string();
            return;
        }
        let worker_path = match self.mining_kind {
            MiningKind::Hac => self.poworker_path.clone(),
            MiningKind::Hacd => self.diaworker_path.clone(),
        };
        let not_found = match self.mining_kind {
            MiningKind::Hac => t.poworker_not_found,
            MiningKind::Hacd => t.diaworker_not_found,
        };
        if !worker_path.exists() {
            self.status_msg = format!("{}\n{}", not_found, worker_path.display());
            return;
        }
        if !self.save_config() {
            return;
        }
        if self.connect_mode == crate::connect::ConnectMode::Solo
            && !rpc_reachable(&self.connect)
        {
            let hacash = platform::find_fullnode(&self.work_dir);
            if !hacash.exists() {
                self.status_msg =
                    format!("{}\n{}", t.fullnode_exe_not_found, hacash.display());
                return;
            }
            self.status_msg = t.fullnode_starting.to_string();
            let _ = Command::new(&hacash)
                .current_dir(&self.work_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            self.pending_start = Some(PendingStart {
                worker_path,
                deadline: Instant::now() + Duration::from_secs(120),
                rpc_ready_at: None,
            });
            return;
        }
        self.launch_worker(worker_path);
    }

    pub(super) fn launch_worker(&mut self, worker_path: PathBuf) {
        let t = self.t();
        let mut cmd = Command::new(&worker_path);
        cmd.current_dir(&self.work_dir);
        match Self::spawn_worker_with_logs(&mut cmd) {
            Ok((child, rx)) => {
                self.log_rx = Some(rx);
                self.child = Some(child);
                self.mining = true;
                self.pending_start = None;
                self.status_msg = t.mining_active.to_string();
            }
            Err(e) => self.status_msg = format!("{} {e}", t.start_failed_prefix),
        }
    }

    pub(super) fn poll_pending_start(&mut self) {
        let Some(mut pending) = self.pending_start.clone() else {
            return;
        };
        let t = self.t();
        if rpc_reachable(&self.connect) {
            let ready_at = pending.rpc_ready_at.get_or_insert_with(Instant::now);
            if ready_at.elapsed() >= Duration::from_secs(35) {
                self.launch_worker(pending.worker_path);
            } else {
                self.pending_start = Some(pending);
                self.status_msg = t.fullnode_starting.to_string();
            }
            return;
        }
        pending.rpc_ready_at = None;
        self.pending_start = Some(pending);
        if Instant::now() >= self.pending_start.as_ref().unwrap().deadline {
            self.pending_start = None;
            self.status_msg = format!("{} {}", t.fullnode_not_ready, self.connect);
        }
    }

    pub(super) fn stop_mining(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.mining = false;
        self.log_rx = None;
        self.status_msg = self.t().mining_stopped.to_string();
    }

    pub(super) fn spawn_worker_with_logs(cmd: &mut Command) -> Result<(Child, Receiver<String>), String> {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            spawn_log_drainer(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_log_drainer(err, tx);
        }
        Ok((child, rx))
    }
}

fn spawn_log_drainer<R: Read + Send + 'static>(stream: R, tx: Sender<String>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
}

pub(super) fn rpc_reachable(connect: &str) -> bool {
    let Some(addr) = parse_rpc_addr(connect) else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
}

fn parse_rpc_addr(connect: &str) -> Option<SocketAddr> {
    let trimmed = connect.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = trimmed.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    format!("{host}:{port}").parse().ok()
}