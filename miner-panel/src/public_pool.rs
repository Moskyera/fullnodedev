//! Host a public free-IP pool (hac-pool) from the panel — all-in-one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use serde::{Deserialize, Serialize};

use crate::platform;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicPoolSettings {
    /// User wants the panel to manage a local public pool process.
    #[serde(default)]
    pub host_enabled: bool,
    /// When pool is running, point local mining at 127.0.0.1:http_port.
    #[serde(default = "default_true")]
    pub mine_through_pool: bool,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_stratum_port")]
    pub stratum_port: u16,
    /// Upstream fullnode miner RPC (usually local solo fullnode).
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// Empty = open free pool.
    #[serde(default)]
    pub token: String,
}

fn default_true() -> bool {
    true
}
fn default_http_port() -> u16 {
    3333
}
fn default_stratum_port() -> u16 {
    3334
}
fn default_upstream() -> String {
    "127.0.0.1:8080".into()
}

impl Default for PublicPoolSettings {
    fn default() -> Self {
        Self {
            host_enabled: false,
            mine_through_pool: true,
            http_port: default_http_port(),
            stratum_port: default_stratum_port(),
            upstream: default_upstream(),
            token: String::new(),
        }
    }
}

pub fn settings_path(work_dir: &Path) -> PathBuf {
    work_dir.join("public-pool.json")
}

pub fn load_settings(work_dir: &Path) -> PublicPoolSettings {
    let path = settings_path(work_dir);
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_settings(work_dir: &Path, s: &PublicPoolSettings) -> Result<(), String> {
    let path = settings_path(work_dir);
    let raw = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(path, raw).map_err(|e| e.to_string())
}

pub fn find_hac_pool(work_dir: &Path) -> PathBuf {
    platform::find_worker(work_dir, "hac-pool")
}

pub fn local_pool_connect(http_port: u16) -> String {
    format!("127.0.0.1:{http_port}")
}

/// Spawn hac-pool; returns child on success.
pub fn start_pool(work_dir: &Path, s: &PublicPoolSettings) -> Result<Child, String> {
    let bin = find_hac_pool(work_dir);
    if !bin.is_file() {
        return Err(format!(
            "hac-pool not found at {}. Build: cargo build --release -p miner-pool",
            bin.display()
        ));
    }
    if s.http_port == 0 || s.stratum_port == 0 {
        return Err("pool ports must be non-zero".into());
    }
    if s.http_port == s.stratum_port {
        return Err("HTTP and Stratum ports must differ".into());
    }
    let upstream = s.upstream.trim();
    if upstream.is_empty() {
        return Err("upstream fullnode host:port is required".into());
    }

    let mut cmd = Command::new(&bin);
    cmd.current_dir(work_dir);
    cmd.arg("--upstream").arg(upstream);
    cmd.arg("--http-bind").arg(format!("0.0.0.0:{}", s.http_port));
    cmd.arg("--stratum-bind")
        .arg(format!("0.0.0.0:{}", s.stratum_port));
    if !s.token.trim().is_empty() {
        cmd.arg("--pool-token").arg(s.token.trim());
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    platform::configure_background_command(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("failed to start hac-pool: {e}"))
}

pub fn stop_pool(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Returns true if process still running.
pub fn poll_pool(child: &mut Option<Child>) -> bool {
    let Some(c) = child.as_mut() else {
        return false;
    };
    match c.try_wait() {
        Ok(None) => true,
        Ok(Some(_)) | Err(_) => {
            *child = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_connect_format() {
        assert_eq!(local_pool_connect(3333), "127.0.0.1:3333");
    }
}
