//! Two different programs are started from this module, and they are not the
//! same kind of thing:
//!
//! * `hac-pool` is the free-IP work RELAY. It forwards work to a full node and
//!   holds no wallet, no share record and no money: whatever it helps find is
//!   minted to the host node's own reward address.
//! * `hbit-pool-server` is HBIT, the PAYOUT pool this project ships. It creates
//!   and holds a wallet, credits every miner's shares and pays them out of that
//!   wallet on a timer. What sits in that wallet belongs to other people.
//!
//! They are kept apart in this file for the same reason the UI keeps them apart:
//! one is a proxy, the other is somebody else's money.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
    /// Max concurrent stratum connections from one source IP (0 = unlimited).
    #[serde(default = "default_max_conns_per_ip")]
    pub max_conns_per_ip: u32,
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
fn default_max_conns_per_ip() -> u32 {
    128
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
            max_conns_per_ip: default_max_conns_per_ip(),
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
    cmd.arg("--max-conns-per-ip")
        .arg(s.max_conns_per_ip.to_string());
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

// ===========================================================================
// HBIT: the payout pool. Everything below this line concerns a program that
// holds a wallet full of other people's coins.
// ===========================================================================

/// Binary name of the payout pool server.
pub const HBIT_BIN: &str = "hbit-pool-server";
/// Where the panel keeps the pool's SETTINGS. The passphrase is never one of
/// them: see [`HbitPoolSettings`].
pub const HBIT_SETTINGS_FILE: &str = "hbit-pool.json";
/// The pool's own output, so a refusal or a crash is still readable after the
/// panel has been closed and reopened.
pub const HBIT_LOG_FILE: &str = "hbit-pool.log";
/// The previous run's output, kept so a crash is not erased by the restart that
/// follows it.
pub const HBIT_PREV_LOG_FILE: &str = "hbit-pool.prev.log";
/// The server refuses anything shorter, and so does the panel, before spawning.
pub const HBIT_MIN_PASSWORD: usize = 8;
/// Environment variable the pool reads its wallet passphrase from. The panel
/// sets it on the CHILD only; it is never exported, never saved and never put
/// on a command line.
pub const HBIT_PASSWORD_ENV: &str = "HBIT_WALLET_PASSWORD";
/// The pool's other passphrase source. The panel clears it on the child so an
/// unrelated variable left in the operator's environment cannot decide which
/// passphrase the pool uses.
pub const HBIT_PASSWORD_FILE_ENV: &str = "HBIT_WALLET_PASSWORD_FILE";

const HBIT_LOG_TAIL_LINES: usize = 14;
const HBIT_LOG_TAIL_BYTES: u64 = 32 * 1024;
/// How often the panel re-reads the pool's log while a section is on screen.
const HBIT_LOG_REFRESH: Duration = Duration::from_millis(900);
/// How often the panel checks whether anything is already holding the pool port.
const HBIT_PORT_PROBE_EVERY: Duration = Duration::from_secs(3);
const HBIT_PORT_PROBE_MS: u64 = 250;

/// Every payout-pool setting that is safe to write to disk.
///
/// The wallet passphrase is deliberately NOT a field here. This struct is the
/// only thing the panel serializes for the pool, so there is no field a later
/// edit could put a secret in and no file for one to leak into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HbitPoolSettings {
    /// Base URL of the operator's OWN full node RPC.
    #[serde(default = "hbit_default_node")]
    pub node: String,
    /// Key file the pool creates and then spends from.
    #[serde(default = "hbit_default_wallet_file")]
    pub wallet_file: String,
    /// Port miners connect to.
    #[serde(default = "hbit_default_port")]
    pub port: u16,
    /// false = 127.0.0.1 (this PC only). true = 0.0.0.0 (anyone who can reach
    /// this PC). Defaults to false: a pool that serves strangers is a decision,
    /// not a default.
    #[serde(default)]
    pub public_bind: bool,
    #[serde(default = "hbit_default_share_bits")]
    pub share_bits: u32,
    #[serde(default = "hbit_default_chain")]
    pub chain: String,
    #[serde(default = "hbit_default_settle_secs")]
    pub settle_secs: u64,
    /// The wallet file whose backup terms the operator has acknowledged. Keyed
    /// to the file on purpose: a different wallet is a different thing to back
    /// up, so pointing at one asks again.
    #[serde(default)]
    pub backup_ack_wallet: String,
}

fn hbit_default_node() -> String {
    // The full node this package configures listens on 8080 ([server] listen in
    // hacash.config.ini). Loopback, so the default reaches nothing but this PC.
    "http://127.0.0.1:8080".into()
}
fn hbit_default_wallet_file() -> String {
    "pool-wallet.key".into()
}
fn hbit_default_port() -> u16 {
    9777
}
fn hbit_default_share_bits() -> u32 {
    24
}
fn hbit_default_chain() -> String {
    "mainnet".into()
}
fn hbit_default_settle_secs() -> u64 {
    300
}

impl Default for HbitPoolSettings {
    fn default() -> Self {
        Self {
            node: hbit_default_node(),
            wallet_file: hbit_default_wallet_file(),
            port: hbit_default_port(),
            public_bind: false,
            share_bits: hbit_default_share_bits(),
            chain: hbit_default_chain(),
            settle_secs: hbit_default_settle_secs(),
            backup_ack_wallet: String::new(),
        }
    }
}

/// Why the panel will not start the pool. Each one maps to a sentence in
/// i18n.rs: the reason a person reads must be in their own language, so this
/// module never invents the wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HbitRefusal {
    /// The full node address is empty or is not a plain base URL.
    Node,
    /// No wallet file named.
    WalletFile,
    /// Port, share difficulty or payout interval outside the allowed range.
    Numbers,
    /// Chain is neither mainnet nor a testnet label the server accepts.
    Chain,
    /// No passphrase typed.
    PasswordMissing,
    /// Passphrase shorter than [`HBIT_MIN_PASSWORD`].
    PasswordShort,
    /// The two passphrase fields differ and the wallet does not exist yet, so
    /// the typo would become the passphrase of a wallet nobody can open.
    PasswordMismatch,
    /// The backup warning has not been acknowledged for this wallet file.
    NotAcknowledged,
    /// Something is already listening on the pool port.
    PortBusy,
    /// This panel already runs one.
    AlreadyRunning,
}

/// What the panel could not do, as opposed to what it refuses to do.
#[derive(Clone, Debug)]
pub enum HbitStartError {
    /// A checked reason, worded in i18n.rs.
    Refused(HbitRefusal),
    /// `hbit-pool-server` is not next to the panel. Carries where it looked.
    Missing(String),
    /// The operating system said no. Carried verbatim: it is not ours to word.
    Os(String),
}

/// Normalize the full node field into the base URL the server expects.
///
/// Accepts `127.0.0.1:8080` and `http://127.0.0.1:8080/`; refuses anything with
/// a path, because the pool appends its own (`/query/...`, `/submit/block`).
pub fn hbit_node_url(raw: &str) -> Result<String, HbitRefusal> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return Err(HbitRefusal::Node);
    }
    let (scheme, rest) = match trimmed.split_once("://") {
        Some(("http", r)) => ("http", r),
        Some(("https", r)) => ("https", r),
        Some(_) => return Err(HbitRefusal::Node),
        None => ("http", trimmed),
    };
    if rest.is_empty() || rest.contains('/') || !rest.contains(':') {
        return Err(HbitRefusal::Node);
    }
    Ok(format!("{scheme}://{rest}"))
}

/// `<ip>:<port>` for the pool's listen argument. Loopback unless the operator
/// has explicitly chosen to serve other machines.
pub fn hbit_listen(s: &HbitPoolSettings) -> String {
    let host = if s.public_bind {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    format!("{host}:{}", s.port)
}

/// What a miner types to reach this pool from this PC.
pub fn hbit_local_connect(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Where the wallet key file will actually land. A bare name means "next to the
/// panel"; an absolute path is left alone so the key can live on a backed-up or
/// encrypted volume.
pub fn hbit_wallet_path(work_dir: &Path, s: &HbitPoolSettings) -> PathBuf {
    let raw = s.wallet_file.trim();
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_dir.join(p)
    }
}

fn hbit_chain_ok(chain: &str) -> bool {
    if chain == "mainnet" || chain == "testnet" {
        return true;
    }
    // testnet:<difficulty_adjust_blocks>:<each_block_target_time>
    let Some(rest) = chain.strip_prefix("testnet:") else {
        return false;
    };
    let mut parts = rest.split(':');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    matches!(a.parse::<u64>(), Ok(n) if n > 0) && matches!(b.parse::<u64>(), Ok(n) if n > 0)
}

/// The pool's command line, in the order `hbit-pool-server` documents:
/// `<node> <wallet_file> <listen> <share_bits> <chain> [settle_secs]`.
///
/// The passphrase is NOT here and must never be. A command line is readable by
/// any process listing on the machine; an environment block is not.
pub fn hbit_args(s: &HbitPoolSettings) -> Result<Vec<String>, HbitRefusal> {
    let node = hbit_node_url(&s.node)?;
    let wallet = s.wallet_file.trim();
    if wallet.is_empty() {
        return Err(HbitRefusal::WalletFile);
    }
    if s.port < 1024
        || !(18..=40).contains(&s.share_bits)
        || !(60..=86_400).contains(&s.settle_secs)
    {
        return Err(HbitRefusal::Numbers);
    }
    let chain = s.chain.trim();
    if !hbit_chain_ok(chain) {
        return Err(HbitRefusal::Chain);
    }
    Ok(vec![
        node,
        wallet.to_string(),
        hbit_listen(s),
        s.share_bits.to_string(),
        chain.to_string(),
        s.settle_secs.to_string(),
    ])
}

/// Everything the start decision depends on, with no I/O of its own so the
/// decision can be tested exactly as it is made.
pub struct HbitStartCheck<'a> {
    pub settings: &'a HbitPoolSettings,
    pub password: &'a str,
    pub password_again: &'a str,
    /// False when the key file does not exist yet, which is the one moment a
    /// mistyped passphrase is unrecoverable.
    pub wallet_exists: bool,
    pub acknowledged: bool,
    pub already_running: bool,
    pub port_busy: bool,
}

/// Decide whether the pool may start, and with which arguments.
pub fn check_hbit_start(c: &HbitStartCheck<'_>) -> Result<Vec<String>, HbitRefusal> {
    if c.already_running {
        return Err(HbitRefusal::AlreadyRunning);
    }
    if c.port_busy {
        return Err(HbitRefusal::PortBusy);
    }
    let args = hbit_args(c.settings)?;
    if c.password.is_empty() {
        return Err(HbitRefusal::PasswordMissing);
    }
    if c.password.chars().count() < HBIT_MIN_PASSWORD {
        return Err(HbitRefusal::PasswordShort);
    }
    // Only when the wallet is about to be CREATED. On an existing wallet a wrong
    // passphrase simply fails to decrypt and costs nothing; on a new one it
    // becomes the passphrase, and nobody ever knows what was typed.
    if !c.wallet_exists && c.password != c.password_again {
        return Err(HbitRefusal::PasswordMismatch);
    }
    if !c.acknowledged {
        return Err(HbitRefusal::NotAcknowledged);
    }
    Ok(args)
}

pub fn hbit_settings_path(work_dir: &Path) -> PathBuf {
    work_dir.join(HBIT_SETTINGS_FILE)
}

pub fn load_hbit_settings(work_dir: &Path) -> HbitPoolSettings {
    fs::read_to_string(hbit_settings_path(work_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_hbit_settings(work_dir: &Path, s: &HbitPoolSettings) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(hbit_settings_path(work_dir), raw).map_err(|e| e.to_string())
}

/// Overwrite a passphrase buffer before letting go of it. Rust cannot promise
/// the bytes were never copied elsewhere, but the copy this panel owns does not
/// outlive the spawn that needed it.
fn scrub(s: &mut String) {
    let n = s.len();
    s.clear();
    for _ in 0..n {
        s.push('\0');
    }
    s.clear();
    s.shrink_to_fit();
}

fn hbit_log_tail(path: &Path) -> Vec<String> {
    let Ok(mut f) = fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(HBIT_LOG_TAIL_BYTES);
    if from > 0 && f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: VecDeque<String> = text
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if from > 0 {
        // The seek almost certainly landed mid-line; that fragment is not a line.
        lines.pop_front();
    }
    while lines.len() > HBIT_LOG_TAIL_LINES {
        lines.pop_front();
    }
    lines.into()
}

fn exit_code_text(status: ExitStatus) -> String {
    match status.code() {
        Some(c) => c.to_string(),
        None => "?".to_string(),
    }
}

/// Live state of the payout pool.
///
/// Not `Serialize`, and never will be: the passphrase lives in here, so the type
/// itself is what stops it reaching a file.
pub struct HbitPoolRuntime {
    loaded: bool,
    work_dir: PathBuf,
    pub settings: HbitPoolSettings,
    /// Typed in the UI, held for one spawn, scrubbed after it.
    pub password: String,
    /// Only asked for when the wallet is about to be created.
    pub password_again: String,
    /// The backup box, this session. The durable record is
    /// `settings.backup_ack_wallet`.
    pub ack_checked: bool,
    /// Last refusal, for the UI to word.
    pub refusal: Option<HbitRefusal>,
    /// Last thing the operating system said about starting, verbatim.
    pub error: Option<String>,
    /// Last thing the file system said about saving the settings, verbatim.
    pub save_error: Option<String>,
    /// Exit code of the run that ended, when it ended on its own.
    pub last_exit: Option<String>,
    pub log: Vec<String>,
    child: Option<Child>,
    log_read_at: Option<Instant>,
    port_probe_at: Option<Instant>,
    port_busy: bool,
}

impl HbitPoolRuntime {
    fn new() -> Self {
        Self {
            loaded: false,
            work_dir: PathBuf::new(),
            settings: HbitPoolSettings::default(),
            password: String::new(),
            password_again: String::new(),
            ack_checked: false,
            refusal: None,
            error: None,
            save_error: None,
            last_exit: None,
            log: Vec::new(),
            child: None,
            log_read_at: None,
            port_probe_at: None,
            port_busy: false,
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.work_dir.join(HBIT_LOG_FILE)
    }

    pub fn wallet_path(&self) -> PathBuf {
        hbit_wallet_path(&self.work_dir, &self.settings)
    }

    pub fn wallet_exists(&self) -> bool {
        self.wallet_path().is_file()
    }

    /// Path the panel looked in for the server binary.
    pub fn binary_path(&self) -> PathBuf {
        platform::find_worker(&self.work_dir, HBIT_BIN)
    }

    pub fn binary_present(&self) -> bool {
        self.binary_path().is_file()
    }

    /// The backup terms are acknowledged for the wallet file currently named.
    pub fn acknowledged(&self) -> bool {
        let w = self.settings.wallet_file.trim();
        !w.is_empty() && self.settings.backup_ack_wallet == w
    }

    /// True while this panel's own pool process is alive. Also notices an exit.
    pub fn running(&mut self) -> bool {
        let Some(c) = self.child.as_mut() else {
            return false;
        };
        match c.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.child = None;
                self.last_exit = Some(exit_code_text(status));
                self.log_read_at = None; // read the last words at once
                false
            }
            Err(e) => {
                self.child = None;
                self.last_exit = None;
                self.error = Some(e.to_string());
                false
            }
        }
    }

    pub fn save(&mut self) {
        self.save_error = save_hbit_settings(&self.work_dir, &self.settings).err();
    }

    /// Re-read the pool's own output, at most a few times a second.
    pub fn refresh_log(&mut self) {
        let due = self
            .log_read_at
            .is_none_or(|at| at.elapsed() >= HBIT_LOG_REFRESH);
        if !due {
            return;
        }
        self.log_read_at = Some(Instant::now());
        self.log = hbit_log_tail(&self.log_path());
    }

    /// Is anything already answering on the pool port? Cheap on loopback: a
    /// closed port refuses immediately.
    fn probe_port(&self, port: u16) -> bool {
        crate::connect::probe_reachable(&hbit_local_connect(port), HBIT_PORT_PROBE_MS).is_ok()
    }

    /// Throttled version for drawing. `start` never trusts this.
    pub fn port_busy(&mut self) -> bool {
        if self.child.is_some() {
            return false;
        }
        let due = self
            .port_probe_at
            .is_none_or(|at| at.elapsed() >= HBIT_PORT_PROBE_EVERY);
        if due {
            self.port_probe_at = Some(Instant::now());
            self.port_busy = self.probe_port(self.settings.port);
        }
        self.port_busy
    }

    /// Start the pool. On success the acknowledgement is recorded against the
    /// wallet file it was given for, and the passphrase is scrubbed.
    pub fn start(&mut self) -> Result<(), HbitStartError> {
        self.refusal = None;
        self.error = None;
        // `last_exit` is NOT cleared here. If the previous run ended on its own,
        // why it ended is the most useful thing on the screen right up to the
        // moment a new one is actually running.
        let already_running = self.running();
        // Authoritative at the moment it matters, not the throttled value the
        // UI has been drawing.
        let port_busy = !already_running && self.probe_port(self.settings.port);
        self.port_busy = port_busy;
        self.port_probe_at = Some(Instant::now());

        let wallet_exists = self.wallet_exists();
        let acknowledged = self.acknowledged() || self.ack_checked;
        let args = check_hbit_start(&HbitStartCheck {
            settings: &self.settings,
            password: &self.password,
            password_again: &self.password_again,
            wallet_exists,
            acknowledged,
            already_running,
            port_busy,
        })
        .map_err(|r| {
            self.refusal = Some(r);
            HbitStartError::Refused(r)
        })?;

        let bin = self.binary_path();
        if !bin.is_file() {
            // Nothing stored: the section redraws its own "not here, looked in
            // ..." card from `binary_present()` on the very next frame.
            return Err(HbitStartError::Missing(bin.display().to_string()));
        }

        let child = self.spawn(&bin, &args).map_err(|e| {
            self.error = Some(e.clone());
            HbitStartError::Os(e)
        })?;

        self.child = Some(child);
        self.last_exit = None;
        self.settings.backup_ack_wallet = self.settings.wallet_file.trim().to_string();
        self.save();
        // The pool has it now. Nothing keeps a copy here.
        scrub(&mut self.password);
        scrub(&mut self.password_again);
        self.ack_checked = false;
        self.log_read_at = None;
        Ok(())
    }

    fn spawn(&self, bin: &Path, args: &[String]) -> Result<Child, String> {
        // Keep one previous run, so a crash is not erased by the restart that
        // follows it. Both steps are best effort: an unmovable old log is not a
        // reason to refuse to run a pool.
        let log = self.log_path();
        let prev = self.work_dir.join(HBIT_PREV_LOG_FILE);
        if log.is_file() {
            let _ = fs::remove_file(&prev);
            let _ = fs::rename(&log, &prev);
        }
        let open = || {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .map_err(|e| format!("{}: {e}", log.display()))
        };
        // Two handles, not a pipe. A pipe nobody drains blocks the pool, and a
        // pipe whose reader has exited makes every line it prints fail. A file
        // keeps the pool independent of whether this panel is still open.
        let out = open()?;
        let err = open()?;

        let mut cmd = Command::new(bin);
        cmd.current_dir(&self.work_dir);
        cmd.args(args);
        // The passphrase travels in the child's ENVIRONMENT. Not the command
        // line, which any process listing shows; not a file, which outlives the
        // run; not this panel's own environment, which children would inherit.
        cmd.env(HBIT_PASSWORD_ENV, &self.password);
        // Whatever the operator left in their environment must not get a vote on
        // which passphrase opens this wallet.
        cmd.env_remove(HBIT_PASSWORD_FILE_ENV);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(out));
        cmd.stderr(Stdio::from(err));
        platform::configure_background_command(&mut cmd);
        cmd.spawn().map_err(|e| e.to_string())
    }

    /// Stop the pool this panel started. Safe at any moment: the server records
    /// a payout on disk before it submits it, so a kill can never pay twice.
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.last_exit = None;
        self.refusal = None;
        self.log_read_at = None;
        self.port_probe_at = None;
        self.port_busy = false;
    }
}

static HBIT: LazyLock<Mutex<HbitPoolRuntime>> =
    LazyLock::new(|| Mutex::new(HbitPoolRuntime::new()));

/// The one payout-pool state.
///
/// A `static` because there is exactly one pool per machine and one child
/// process to own. A second copy of this state would be a second pool, and two
/// pools on one wallet pay the same shares twice.
pub fn hbit_state(work_dir: &Path) -> MutexGuard<'static, HbitPoolRuntime> {
    let mut g = HBIT.lock().unwrap_or_else(|e| e.into_inner());
    if !g.loaded {
        g.work_dir = work_dir.to_path_buf();
        g.settings = load_hbit_settings(work_dir);
        g.loaded = true;
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_connect_format() {
        assert_eq!(local_pool_connect(3333), "127.0.0.1:3333");
    }

    #[test]
    fn old_settings_file_without_max_conns_still_loads() {
        // public-pool.json written by an earlier build has no max_conns_per_ip;
        // it must keep loading and fall back to the hac-pool default of 128.
        let raw = r#"{"host_enabled":true,"http_port":3333,"stratum_port":3334,
            "upstream":"127.0.0.1:8080","token":""}"#;
        let s: PublicPoolSettings = serde_json::from_str(raw).expect("old settings must parse");
        assert_eq!(s.max_conns_per_ip, 128);
        assert_eq!(s.max_conns_per_ip, PublicPoolSettings::default().max_conns_per_ip);
    }

    /* ---- HBIT: the pool that holds money ---- */

    const PASS: &str = "a passphrase i wrote down";

    fn ok_check<'a>(s: &'a HbitPoolSettings) -> HbitStartCheck<'a> {
        HbitStartCheck {
            settings: s,
            password: PASS,
            password_again: PASS,
            wallet_exists: false,
            acknowledged: true,
            already_running: false,
            port_busy: false,
        }
    }

    #[test]
    fn hbit_defaults_ship_nobody_elses_address_and_reach_nobody() {
        let s = HbitPoolSettings::default();
        // Loopback in, loopback out. Nothing here names a machine, a person or
        // an address, and nothing here serves a stranger until asked to.
        assert_eq!(s.node, "http://127.0.0.1:8080");
        assert!(!s.public_bind);
        assert_eq!(hbit_listen(&s), "127.0.0.1:9777");
        assert_eq!(s.wallet_file, "pool-wallet.key");
        // No wallet is acknowledged before a person acknowledges one.
        assert!(s.backup_ack_wallet.is_empty());
    }

    #[test]
    fn hbit_settings_file_can_never_carry_the_passphrase() {
        // The saved shape is the whole defence: there is no field to put a
        // secret in, so no code path can write one to disk.
        let raw = serde_json::to_string(&HbitPoolSettings::default()).unwrap();
        let j: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Sorted, because that is the order serde_json hands an object back in.
        let keys: Vec<&str> = j.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "backup_ack_wallet",
                "chain",
                "node",
                "port",
                "public_bind",
                "settle_secs",
                "share_bits",
                "wallet_file",
            ]
        );
        for bad in ["password", "passphrase", "secret", "key"] {
            assert!(!keys.contains(&bad), "settings must not hold `{bad}`");
        }
    }

    #[test]
    fn hbit_args_are_the_documented_order_and_hold_no_secret() {
        let mut s = HbitPoolSettings::default();
        s.public_bind = true;
        let args = hbit_args(&s).expect("defaults must be startable");
        assert_eq!(
            args,
            vec![
                "http://127.0.0.1:8080",
                "pool-wallet.key",
                "0.0.0.0:9777",
                "24",
                "mainnet",
                "300",
            ]
        );
        // The command line is world readable. The passphrase is not on it.
        assert!(!args.iter().any(|a| a.contains(PASS)));
    }

    #[test]
    fn hbit_start_check_returns_the_same_args_it_would_run() {
        let s = HbitPoolSettings::default();
        let args = check_hbit_start(&ok_check(&s)).expect("a complete request must pass");
        assert_eq!(args, hbit_args(&s).unwrap());
        assert!(!args.iter().any(|a| a.contains(PASS)));
    }

    #[test]
    fn hbit_node_field_is_normalized_and_paths_are_refused() {
        assert_eq!(
            hbit_node_url(" 127.0.0.1:8080 ").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            hbit_node_url("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            hbit_node_url("https://node.example:8080").unwrap(),
            "https://node.example:8080"
        );
        // The pool appends its own paths, so a base URL with one would build
        // requests that go nowhere.
        assert_eq!(hbit_node_url("http://x:8080/api"), Err(HbitRefusal::Node));
        assert_eq!(hbit_node_url(""), Err(HbitRefusal::Node));
        assert_eq!(hbit_node_url("ftp://x:8080"), Err(HbitRefusal::Node));
        assert_eq!(hbit_node_url("http://no-port"), Err(HbitRefusal::Node));
    }

    #[test]
    fn hbit_refuses_a_new_wallet_when_the_two_passphrases_differ() {
        let s = HbitPoolSettings::default();
        let mut c = ok_check(&s);
        c.password_again = "a passphrase i typed wrong";
        assert_eq!(
            check_hbit_start(&c),
            Err(HbitRefusal::PasswordMismatch),
            "a typo on a wallet that does not exist yet is unrecoverable"
        );
        // An existing wallet cannot be harmed by a wrong passphrase: it simply
        // does not open, so the second field is not asked for.
        c.wallet_exists = true;
        assert!(check_hbit_start(&c).is_ok());
    }

    #[test]
    fn hbit_refuses_an_unprotected_or_weak_wallet_key() {
        let s = HbitPoolSettings::default();
        let mut c = ok_check(&s);
        c.password = "";
        c.password_again = "";
        assert_eq!(check_hbit_start(&c), Err(HbitRefusal::PasswordMissing));
        c.password = "1234567";
        c.password_again = "1234567";
        assert_eq!(check_hbit_start(&c), Err(HbitRefusal::PasswordShort));
    }

    #[test]
    fn hbit_refuses_until_the_backup_warning_is_acknowledged() {
        let s = HbitPoolSettings::default();
        let mut c = ok_check(&s);
        c.acknowledged = false;
        assert_eq!(check_hbit_start(&c), Err(HbitRefusal::NotAcknowledged));
    }

    #[test]
    fn hbit_refuses_a_second_pool_on_the_same_wallet() {
        let s = HbitPoolSettings::default();
        let mut c = ok_check(&s);
        c.already_running = true;
        assert_eq!(check_hbit_start(&c), Err(HbitRefusal::AlreadyRunning));
        c.already_running = false;
        c.port_busy = true;
        assert_eq!(check_hbit_start(&c), Err(HbitRefusal::PortBusy));
    }

    #[test]
    fn hbit_refuses_numbers_and_chains_the_server_would_reject() {
        let base = HbitPoolSettings::default();

        let mut s = base.clone();
        s.share_bits = 41;
        assert_eq!(hbit_args(&s), Err(HbitRefusal::Numbers));
        s.share_bits = 17;
        assert_eq!(hbit_args(&s), Err(HbitRefusal::Numbers));

        let mut s = base.clone();
        s.settle_secs = 5;
        assert_eq!(hbit_args(&s), Err(HbitRefusal::Numbers));

        let mut s = base.clone();
        s.port = 80;
        assert_eq!(hbit_args(&s), Err(HbitRefusal::Numbers));

        let mut s = base.clone();
        s.wallet_file = "   ".into();
        assert_eq!(hbit_args(&s), Err(HbitRefusal::WalletFile));

        let mut s = base.clone();
        for good in ["mainnet", "testnet", "testnet:288:10"] {
            s.chain = good.into();
            assert!(
                hbit_args(&s).is_ok(),
                "`{good}` is a chain the server takes"
            );
        }
        for bad in [
            "",
            "main",
            "testnet:",
            "testnet:288",
            "testnet:0:10",
            "testnet:a:b",
        ] {
            s.chain = bad.into();
            assert_eq!(
                hbit_args(&s),
                Err(HbitRefusal::Chain),
                "`{bad}` must be refused"
            );
        }
    }

    #[test]
    fn hbit_settings_file_from_an_older_build_still_loads() {
        // A file written before a field existed must keep working and fall back
        // to the same defaults the program would have used anyway.
        let s: HbitPoolSettings =
            serde_json::from_str(r#"{"node":"http://127.0.0.1:8080"}"#).expect("must parse");
        assert_eq!(s.port, HbitPoolSettings::default().port);
        assert_eq!(s.share_bits, 24);
        assert_eq!(s.settle_secs, 300);
        assert!(!s.public_bind);
        assert!(s.backup_ack_wallet.is_empty());
    }

    #[test]
    fn hbit_acknowledgement_is_tied_to_one_wallet_file() {
        let mut r = HbitPoolRuntime::new();
        r.settings.wallet_file = "pool-wallet.key".into();
        r.settings.backup_ack_wallet = "pool-wallet.key".into();
        assert!(r.acknowledged());
        // A different wallet is a different thing to back up, so the warning is
        // owed again.
        r.settings.wallet_file = "other-wallet.key".into();
        assert!(!r.acknowledged());
    }

    #[test]
    fn scrubbing_a_passphrase_leaves_nothing_behind() {
        let mut p = String::from(PASS);
        scrub(&mut p);
        assert!(p.is_empty());
        assert_eq!(p.capacity(), 0);
    }
}
