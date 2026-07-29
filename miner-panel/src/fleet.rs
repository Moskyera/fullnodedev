//! Read-only LAN multi-miner dashboard and stats sharing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app::efficiency::MiningStatsSnapshot;
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::i18n::PanelLabels;
use crate::{hacash_config::atomic_write_private, theme};

const DEFAULT_PORT: u16 = 19_120;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 8 * 1024;
const MAX_PEERS: usize = 64;
const MAX_PEER_NAME_BYTES: usize = 80;
const TOKEN_RANDOM_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_RANDOM_BYTES * 2;
const SERVER_WORKERS: usize = 4;
const SERVER_QUEUE_CAPACITY: usize = 16;
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IN_FLIGHT_PER_SOURCE: u8 = 2;
const SOURCE_TOKEN_BURST: u8 = 4;
const SOURCE_TOKEN_REFILL: Duration = Duration::from_secs(1);
const SOURCE_IDLE_TTL: Duration = Duration::from_secs(60);
const MAX_SOURCE_ENTRIES: usize = 256;

const POLL_WORKERS: usize = 8;
const MAX_RESOLVED_ADDRESSES: usize = 4;
const MAX_DNS_RESOLVERS: usize = POLL_WORKERS;
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const PEER_WALL_TIMEOUT: Duration = Duration::from_millis(2_500);
const PEER_CONNECT_SLICE: Duration = Duration::from_millis(600);
/// A rig's snapshot is stale on exactly the terms the dashboard uses for the
/// local worker's, so "too old to quote" means one thing in this panel.
const MAX_STATS_AGE: Duration = crate::stats_poll::STATS_STALE_AFTER;
const MAX_STATS_FUTURE_SKEW: Duration = Duration::from_secs(30);
const MAX_STATS_STRING_BYTES: usize = 256;
const MAX_HASHRATE_HPS: f64 = 1.0e18;
const MAX_POWER_WATTS: f64 = 1.0e7;
const MAX_DAILY_VALUE: f64 = 1.0e12;

static DNS_RESOLVERS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetPeer {
    pub name: String,
    pub address: String,
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FleetConfig {
    #[serde(default)]
    share_enabled: bool,
    #[serde(default = "default_port")]
    share_port: u16,
    #[serde(default = "default_share_token")]
    share_token: String,
    #[serde(default)]
    peers: Vec<FleetPeer>,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            share_enabled: false,
            share_port: DEFAULT_PORT,
            share_token: default_share_token(),
            peers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct PeerResult {
    peer: FleetPeer,
    stats: Option<MiningStatsSnapshot>,
    error: String,
}

#[derive(Clone, Debug)]
struct PollBatch {
    generation: u64,
    results: Vec<PeerResult>,
}

#[derive(Clone, Copy, Debug)]
struct SourceState {
    in_flight: u8,
    tokens: u8,
    last_refill: Instant,
    last_seen: Instant,
}

#[derive(Default)]
struct SourceLimiter {
    sources: HashMap<IpAddr, SourceState>,
}

impl SourceLimiter {
    fn cleanup(&mut self, now: Instant) {
        self.sources.retain(|_, state| {
            state.in_flight > 0 || now.saturating_duration_since(state.last_seen) < SOURCE_IDLE_TTL
        });
    }

    fn try_acquire(&mut self, source: IpAddr, now: Instant) -> bool {
        let source = canonical_source_ip(source);
        self.cleanup(now);
        if !self.sources.contains_key(&source) && self.sources.len() >= MAX_SOURCE_ENTRIES {
            return false;
        }
        let state = self.sources.entry(source).or_insert(SourceState {
            in_flight: 0,
            tokens: SOURCE_TOKEN_BURST,
            last_refill: now,
            last_seen: now,
        });
        let refill = now
            .saturating_duration_since(state.last_refill)
            .as_secs()
            .min(u8::MAX as u64) as u8;
        if refill > 0 {
            state.tokens = state.tokens.saturating_add(refill).min(SOURCE_TOKEN_BURST);
            state.last_refill += SOURCE_TOKEN_REFILL * u32::from(refill);
        }
        state.last_seen = now;
        if state.in_flight >= MAX_IN_FLIGHT_PER_SOURCE || state.tokens == 0 {
            return false;
        }
        state.in_flight += 1;
        state.tokens -= 1;
        true
    }

    fn release(&mut self, source: IpAddr, now: Instant) {
        if let Some(state) = self.sources.get_mut(&canonical_source_ip(source)) {
            state.in_flight = state.in_flight.saturating_sub(1);
            state.last_seen = now;
        }
    }
}

fn canonical_source_ip(source: IpAddr) -> IpAddr {
    match source {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(address)),
        other => other,
    }
}

struct SourcePermit {
    source: IpAddr,
    limiter: Arc<Mutex<SourceLimiter>>,
}

impl SourcePermit {
    fn try_acquire(
        source: IpAddr,
        limiter: &Arc<Mutex<SourceLimiter>>,
        now: Instant,
    ) -> Option<Self> {
        let source = canonical_source_ip(source);
        let acquired = limiter.lock().ok()?.try_acquire(source, now);
        acquired.then(|| Self {
            source,
            limiter: Arc::clone(limiter),
        })
    }
}

impl Drop for SourcePermit {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            limiter.release(self.source, Instant::now());
        }
    }
}

struct AcceptedStream {
    stream: TcpStream,
    deadline: Instant,
    _permit: SourcePermit,
}

struct FleetServer {
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    worker_threads: Vec<JoinHandle<()>>,
}

impl FleetServer {
    fn start(stats_path: PathBuf, port: u16, token: String) -> Result<Self, String> {
        validate_token(&token)
            .map_err(|error| format!("LAN sharing needs a secure access token: {error}"))?;
        let listeners = bind_lan_listeners(port)?;

        let stop = Arc::new(AtomicBool::new(false));
        let source_limiter = Arc::new(Mutex::new(SourceLimiter::default()));
        let (stream_tx, stream_rx) = mpsc::sync_channel::<AcceptedStream>(SERVER_QUEUE_CAPACITY);
        let stream_rx = Arc::new(Mutex::new(stream_rx));
        let mut worker_threads = Vec::with_capacity(SERVER_WORKERS);
        for _ in 0..SERVER_WORKERS {
            let worker_stop = Arc::clone(&stop);
            let worker_rx = Arc::clone(&stream_rx);
            let worker_stats_path = stats_path.clone();
            let worker_token = token.clone();
            worker_threads.push(thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    let received = match worker_rx.lock() {
                        Ok(receiver) => receiver.recv_timeout(Duration::from_millis(100)),
                        Err(_) => return,
                    };
                    match received {
                        Ok(accepted) => {
                            serve_stats_request(
                                accepted.stream,
                                &worker_stats_path,
                                &worker_token,
                                accepted.deadline,
                            );
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }));
        }

        let accept_stop = Arc::clone(&stop);
        let accept_thread = thread::spawn(move || {
            while !accept_stop.load(Ordering::Acquire) {
                let mut accepted_any = false;
                for listener in &listeners {
                    match listener.accept() {
                        Ok((stream, source)) => {
                            accepted_any = true;
                            if !is_lan_ip(source.ip()) {
                                continue;
                            }
                            let accepted_at = Instant::now();
                            let Some(permit) = SourcePermit::try_acquire(
                                source.ip(),
                                &source_limiter,
                                accepted_at,
                            ) else {
                                thread::sleep(Duration::from_millis(2));
                                continue;
                            };
                            let accepted = AcceptedStream {
                                stream,
                                deadline: accepted_at + SERVER_REQUEST_TIMEOUT,
                                _permit: permit,
                            };
                            match stream_tx.try_send(accepted) {
                                Ok(()) => {}
                                Err(TrySendError::Full(accepted)) => {
                                    drop(accepted);
                                    thread::sleep(Duration::from_millis(5));
                                }
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => thread::sleep(Duration::from_millis(20)),
                    }
                }
                if !accepted_any {
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });

        Ok(Self {
            stop,
            accept_thread: Some(accept_thread),
            worker_threads,
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        for handle in self.worker_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

fn bind_lan_listeners(port: u16) -> Result<Vec<TcpListener>, String> {
    let mut listeners = Vec::with_capacity(2);
    let ipv6_result = TcpListener::bind((Ipv6Addr::UNSPECIFIED, port));
    if let Ok(listener) = ipv6_result.as_ref() {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("Cannot configure IPv6 LAN stats port: {error}"))?;
    }
    if let Ok(listener) = ipv6_result {
        listeners.push(listener);
    }

    match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(listener) => {
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("Cannot configure IPv4 LAN stats port: {error}"))?;
            listeners.push(listener);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && !listeners.is_empty() => {
            // On dual-stack systems the IPv6 wildcard listener also accepts IPv4.
        }
        Err(error) if listeners.is_empty() => {
            return Err(format!("Cannot open LAN stats port {port}: {error}"));
        }
        Err(error) => {
            return Err(format!("Cannot open IPv4 LAN stats port {port}: {error}"));
        }
    }

    if listeners.is_empty() {
        Err(format!(
            "Cannot open LAN stats port {port} for IPv4 or IPv6"
        ))
    } else {
        Ok(listeners)
    }
}

pub struct FleetState {
    config_path: PathBuf,
    stats_path: PathBuf,
    config: FleetConfig,
    server: Option<FleetServer>,
    server_error: String,
    results: Vec<PeerResult>,
    poll_tx: Sender<PollBatch>,
    poll_rx: Receiver<PollBatch>,
    poll_running_generation: Option<u64>,
    poll_generation: u64,
    last_poll: Instant,
    name_input: String,
    address_input: String,
    token_input: String,
    add_error: String,
    /// Whether the worker on THIS machine is currently writing its stats file.
    ///
    /// The sidebar badge and the Master Panel's "workers online" card are the
    /// same arithmetic, and both count this machine, so both need the same
    /// answer about it. The local snapshot is only handed to the screen, and the
    /// badge is drawn before the screen, so the check lives in `poll` where both
    /// can read it.
    local_online: bool,
    local_probe_at: Instant,
}

impl FleetState {
    pub fn load(work_dir: &Path, stats_path: &Path) -> Self {
        let config_path = work_dir.join("miner-fleet.json");
        let (mut config, mut startup_messages) = match read_fleet_config(&config_path) {
            Ok(Some(config)) => (config, Vec::new()),
            Ok(None) => (FleetConfig::default(), Vec::new()),
            Err(error) => (FleetConfig::default(), vec![error]),
        };
        startup_messages.extend(sanitize_loaded_config(&mut config));

        if validate_token(&config.share_token).is_err() {
            match try_generate_token() {
                Ok(token) => config.share_token = token,
                Err(error) => {
                    config.share_enabled = false;
                    config.share_token.clear();
                    startup_messages.push(error);
                }
            }
        }
        let startup_error = startup_messages.join(" ");

        let (poll_tx, poll_rx) = mpsc::channel();
        let mut state = Self {
            config_path,
            stats_path: stats_path.to_path_buf(),
            config,
            server: None,
            server_error: String::new(),
            results: Vec::new(),
            poll_tx,
            poll_rx,
            poll_running_generation: None,
            poll_generation: 0,
            last_poll: Instant::now() - Duration::from_secs(30),
            name_input: String::new(),
            address_input: String::new(),
            token_input: String::new(),
            add_error: String::new(),
            local_online: false,
            local_probe_at: Instant::now() - LOCAL_PROBE_INTERVAL,
        };

        match state.save() {
            Ok(()) => {
                state.sync_server();
                if !startup_error.is_empty() {
                    if state.server_error.is_empty() {
                        state.server_error = startup_error;
                    } else {
                        state.server_error = format!("{startup_error} {}", state.server_error);
                    }
                }
            }
            Err(error) => state.server_error = error,
        }
        state
    }

    /// (answering, configured) counting this machine, the same arithmetic the
    /// Master Panel table prints. Exposed so the sidebar badge cannot drift
    /// away from the table it is a summary of.
    ///
    /// This machine counts as answering only when its worker is actually
    /// writing telemetry. An idle PC in the "online" count would be the one
    /// number on this screen that is decided by nothing.
    pub fn workers_online(&self) -> (usize, usize) {
        let answering = self.results.iter().filter(|r| r.stats.is_some()).count();
        (
            answering + usize::from(self.local_online),
            self.config.peers.len() + 1,
        )
    }

    pub fn poll(&mut self) {
        if self.local_probe_at.elapsed() >= LOCAL_PROBE_INTERVAL {
            self.local_probe_at = Instant::now();
            self.local_online = local_worker_reporting(&self.stats_path);
        }
        while let Ok(batch) = self.poll_rx.try_recv() {
            if self.poll_running_generation == Some(batch.generation) {
                self.poll_running_generation = None;
            }
            apply_poll_batch(self.poll_generation, &mut self.results, batch);
        }
        if self.poll_running_generation.is_some()
            || self.config.peers.is_empty()
            || self.last_poll.elapsed() < Duration::from_secs(5)
        {
            return;
        }

        self.last_poll = Instant::now();
        let generation = self.poll_generation;
        self.poll_running_generation = Some(generation);
        let peers = self.config.peers.clone();
        let tx = self.poll_tx.clone();
        thread::spawn(move || {
            let results = poll_peers_bounded(peers);
            let _ = tx.send(PollBatch {
                generation,
                results,
            });
        });
    }

    pub fn stop(&mut self) {
        self.stop_server_only();
        self.poll_generation = self.poll_generation.wrapping_add(1);
        self.results.clear();
        while let Ok(batch) = self.poll_rx.try_recv() {
            if self.poll_running_generation == Some(batch.generation) {
                self.poll_running_generation = None;
            }
        }
    }

    fn stop_server_only(&mut self) {
        if let Some(mut server) = self.server.take() {
            server.stop();
        }
    }

    fn invalidate_peer_set(&mut self) {
        self.poll_generation = self.poll_generation.wrapping_add(1);
        self.last_poll = Instant::now() - Duration::from_secs(30);
    }

    // -----------------------------------------------------------------------
    // The Master Panel screen.
    // -----------------------------------------------------------------------

    /// Every worker reporting to this panel, in the order the table prints them:
    /// this machine first, then each configured remote in the order it was
    /// added.
    ///
    /// A worker that is not answering carries dashes and the reason it is not
    /// answering. It never carries the last number it managed to send, because
    /// a stale hashrate on a dead rig is the one mistake this screen exists to
    /// prevent.
    fn worker_rows(&self, local: &MiningStatsSnapshot, l: &PanelLabels) -> Vec<WorkerRow> {
        let mut rows = Vec::with_capacity(self.config.peers.len() + 1);
        rows.push(WorkerRow {
            name: l.this_pc.to_string(),
            detail: if self.local_online {
                String::new()
            } else {
                l.state_idle.to_string()
            },
            state: if self.local_online {
                RowState::Online
            } else {
                RowState::Offline
            },
            hashrate: local.hashrate_hps,
            hac_day: local.hac_per_day,
            watts: local.watts,
            height: local.height,
        });

        for peer in &self.config.peers {
            let result = self.results.iter().find(|r| r.peer.address == peer.address);
            match result.and_then(|r| r.stats.as_ref()) {
                Some(stats) => rows.push(WorkerRow {
                    name: peer.name.clone(),
                    detail: String::new(),
                    state: RowState::Online,
                    hashrate: stats.hashrate_hps,
                    hac_day: stats.hac_per_day,
                    watts: stats.watts,
                    height: stats.height,
                }),
                None => {
                    // The three reasons a row has no numbers are not the same
                    // reason, so the row says which one it is.
                    let (state, detail) = match result {
                        Some(r) if !r.error.is_empty() => (RowState::Offline, r.error.clone()),
                        Some(_) => (RowState::Offline, l.state_offline.to_string()),
                        None => (RowState::Waiting, l.state_waiting.to_string()),
                    };
                    rows.push(WorkerRow {
                        name: peer.name.clone(),
                        detail,
                        state,
                        hashrate: 0.0,
                        hac_day: 0.0,
                        watts: 0.0,
                        height: 0,
                    });
                }
            }
        }
        rows
    }

    /// The Master Panel, laid out from the commissioned mockup: four overview
    /// cards, one table row per worker, the LAN warning under it, and the two
    /// cards that change the fleet, folded away until they are wanted.
    ///
    /// The four totals count only workers that are answering right now. Adding
    /// an unreachable rig's last known 4 GH/s into "total hashrate" would make
    /// the headline number grow when the fleet shrank.
    pub fn show_master(&mut self, ui: &mut egui::Ui, local: &MiningStatsSnapshot) {
        let l = crate::i18n::panel_labels(crate::i18n::active_lang());
        let rows = self.worker_rows(local, &l);
        let mut total_hashrate = 0.0;
        let mut total_hac = 0.0;
        let mut total_watts = 0.0;
        for row in rows.iter().filter(|row| row.state == RowState::Online) {
            total_hashrate += row.hashrate;
            total_hac += row.hac_day;
            total_watts += row.watts;
        }
        let (answering, configured) = self.workers_online();
        let busy = self.poll_running_generation.is_some();

        page_header(
            ui,
            l.master_title,
            l.master_sub,
            if busy { Some(l.polling) } else { None },
            l.read_only,
        );
        ui.add_space(16.0);

        let width = ui.available_width();
        let card_w = ((width - CARD_GAP * 3.0) / 4.0).floor().max(60.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = CARD_GAP;
            overview_card(ui, card_w, l.kpi_hashrate, &format_hashrate(total_hashrate));
            overview_card(
                ui,
                card_w,
                l.kpi_workers,
                &format!("{answering} / {configured}"),
            );
            overview_card(ui, card_w, l.kpi_power, &format!("{total_watts:.0} W"));
            overview_card(ui, card_w, l.kpi_hac_day, &format!("{total_hac:.4}"));
        });

        ui.add_space(CARD_GAP);
        worker_table(ui, &l, &rows);

        ui.add_space(CARD_GAP);
        warning_strip(ui, l.lan_warning);

        ui.add_space(CARD_GAP);
        self.manage_card(ui, &l);
        ui.add_space(CARD_GAP);
        self.share_card(ui, &l, "fleet_share_master");
        ui.add_space(4.0);
    }

    /// Add and remove remote workers. Folded shut by default: the screen is a
    /// dashboard first, and this is the thing you do to it once.
    fn manage_card(&mut self, ui: &mut egui::Ui, l: &PanelLabels) {
        collapsible_card(
            ui,
            "fleet_manage_miners",
            l.manage_title,
            l.manage_sub,
            |ui| {
                ui.label(
                    egui::RichText::new(l.manage_hint)
                        .size(11.5)
                        .color(theme::colors::TEXT_MUTED),
                );
                ui.add_space(12.0);

                let width = ui.available_width();
                // Wide enough that "Add miner" stays on one line in the longest
                // of the nine languages rather than wrapping into a two-line
                // button beside single-line inputs.
                let button_w = 186.0;
                let col = ((width - CARD_GAP * 3.0 - button_w) / 3.0)
                    .floor()
                    .max(90.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = CARD_GAP;
                    theme::field_col(ui, col, l.field_name, |ui, w| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.name_input)
                                .hint_text("Rig 02")
                                .desired_width(w),
                        );
                    });
                    theme::field_col(ui, col, l.field_address, |ui, w| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.address_input)
                                .hint_text("192.168.1.42:19120")
                                .desired_width(w),
                        );
                    });
                    theme::field_col(ui, col, l.field_token, |ui, w| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token_input)
                                .password(true)
                                .desired_width(w),
                        );
                    });
                    ui.vertical(|ui| {
                        // Down onto the baseline of the three inputs, past the
                        // captions above them.
                        ui.add_space(18.0);
                        if theme::btn_secondary(ui, l.btn_add_miner).clicked() {
                            self.add_peer();
                        }
                    });
                });

                if !self.add_error.is_empty() {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(&self.add_error)
                            .size(11.5)
                            .color(theme::colors::RED),
                    );
                }

                ui.add_space(12.0);
                if self.config.peers.is_empty() {
                    ui.label(
                        egui::RichText::new(l.no_peers)
                            .size(11.5)
                            .color(theme::colors::TEXT_DIM),
                    );
                    return;
                }

                let mut remove = None;
                for (idx, peer) in self.config.peers.iter().enumerate() {
                    if idx > 0 {
                        ui.add_space(9.0);
                        hairline(ui);
                        ui.add_space(9.0);
                    }
                    let result = self.results.iter().find(|r| r.peer.address == peer.address);
                    let (state, note) = match result {
                        Some(r) if r.stats.is_some() => (RowState::Online, String::new()),
                        Some(r) if !r.error.is_empty() => (RowState::Offline, r.error.clone()),
                        Some(_) => (RowState::Offline, l.state_offline.to_string()),
                        None => (RowState::Waiting, l.state_waiting.to_string()),
                    };
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let (dot, _) =
                            ui.allocate_exact_size(egui::Vec2::splat(8.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.5, state.dot());
                        ui.label(
                            egui::RichText::new(&peer.name)
                                .size(12.5)
                                .strong()
                                .color(theme::colors::TEXT),
                        );
                        ui.label(
                            egui::RichText::new(&peer.address)
                                .size(11.5)
                                .color(theme::colors::TEXT_DIM),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(l.btn_remove).clicked() {
                                remove = Some(idx);
                            }
                            if !note.is_empty() {
                                ui.label(
                                    egui::RichText::new(elide(&note, 64))
                                        .size(11.0)
                                        .color(theme::colors::TEXT_DIM),
                                );
                            }
                        });
                    });
                }
                if let Some(idx) = remove {
                    self.remove_peer(idx);
                }
            },
        );
    }

    /// Whether other panels may read this miner, on which port, and with which
    /// token. Drawn on the Master Panel and again on Setup, so the caller names
    /// the fold state; two cards sharing one id would open and close together.
    fn share_card(&mut self, ui: &mut egui::Ui, l: &PanelLabels, id: &str) {
        collapsible_card(ui, id, l.share_title, l.share_sub, |ui| {
            let mut restart = ui
                .checkbox(&mut self.config.share_enabled, l.share_enable)
                .changed();
            ui.add_space(10.0);

            let enabled = self.config.share_enabled;
            ui.add_enabled_ui(enabled, |ui| {
                let width = ui.available_width();
                let col = ((width - CARD_GAP) * 0.5).floor().max(120.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = CARD_GAP;
                    theme::field_col(ui, col, l.share_port, |ui, _w| {
                        restart |= ui
                            .add(
                                egui::DragValue::new(&mut self.config.share_port)
                                    .range(1024..=65535),
                            )
                            .changed();
                    });
                    theme::field_col(ui, col, l.share_token, |ui, w| {
                        ui.horizontal(|ui| {
                            // Shown masked and disabled on purpose: this is a
                            // secret to copy, never a field to retype.
                            let mut masked = self.config.share_token.clone();
                            ui.add_enabled(
                                false,
                                egui::TextEdit::singleline(&mut masked)
                                    .password(true)
                                    .desired_width((w - 170.0).max(70.0)),
                            );
                            if ui.small_button(l.btn_copy).clicked() {
                                ui.ctx().copy_text(self.config.share_token.clone());
                            }
                            if ui.small_button(l.btn_new_token).clicked() {
                                match try_generate_token() {
                                    Ok(token) => {
                                        self.config.share_token = token;
                                        self.server_error.clear();
                                        restart = true;
                                    }
                                    Err(error) => self.server_error = error,
                                }
                            }
                        });
                    });
                });
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(l.share_hint_display(self.config.share_port))
                        .size(11.0)
                        .color(theme::colors::TEXT_MUTED),
                );
            });

            ui.add_space(10.0);
            warning_strip(ui, l.share_warning);

            if !self.server_error.is_empty() {
                ui.add_space(9.0);
                ui.label(
                    egui::RichText::new(&self.server_error)
                        .size(11.5)
                        .color(theme::colors::RED),
                );
            } else if self.config.share_enabled {
                ui.add_space(9.0);
                ui.label(
                    egui::RichText::new(l.share_active)
                        .size(11.5)
                        .strong()
                        .color(theme::colors::ACCENT),
                );
            }

            if restart {
                self.restart_sharing();
            }
        });
    }

    /// Save the sharing settings, then bring the endpoint into line with them.
    /// A save that fails takes the endpoint down: a running server whose
    /// settings were not written would come back different after a restart.
    fn restart_sharing(&mut self) {
        if self.config.share_token.trim().is_empty() {
            match try_generate_token() {
                Ok(token) => self.config.share_token = token,
                Err(error) => {
                    self.server_error = error;
                    self.stop_server_only();
                    return;
                }
            }
        }
        match self.save() {
            Ok(()) => self.sync_server(),
            Err(error) => {
                self.stop_server_only();
                self.server_error = error;
            }
        }
    }

    /// The Setup screen's copy of the LAN sharing card.
    pub fn show_settings(&mut self, ui: &mut egui::Ui) {
        let l = crate::i18n::panel_labels(crate::i18n::active_lang());
        self.share_card(ui, &l, "fleet_share_setup");
    }

    fn remove_peer(&mut self, idx: usize) {
        if idx >= self.config.peers.len() {
            return;
        }
        let removed = self.config.peers.remove(idx);
        match self.save() {
            Ok(()) => {
                let kept: HashSet<String> = self
                    .config
                    .peers
                    .iter()
                    .map(|peer| peer.address.clone())
                    .collect();
                self.results
                    .retain(|result| kept.contains(&result.peer.address));
                self.invalidate_peer_set();
            }
            Err(error) => {
                // The file is the truth. If it would not take the removal, the
                // peer is still in the fleet and the table must keep saying so.
                self.config.peers.insert(idx, removed);
                self.add_error = error;
            }
        }
    }

    fn add_peer(&mut self) {
        self.add_error.clear();
        if self.config.peers.len() >= MAX_PEERS {
            self.add_error = format!("A dashboard can monitor up to {MAX_PEERS} miners.");
            return;
        }

        let address = match normalize_peer_address(&self.address_input) {
            Ok(address) => address,
            Err(error) => {
                self.add_error = error;
                return;
            }
        };
        let token = self.token_input.trim().to_string();
        if let Err(error) = validate_token(&token) {
            self.add_error = format!("Remote access token is invalid: {error}");
            return;
        }
        if self.config.peers.iter().any(|peer| peer.address == address) {
            self.add_error = "This miner is already in the fleet.".to_string();
            return;
        }

        let name = if self.name_input.trim().is_empty() {
            address.clone()
        } else {
            self.name_input.trim().to_string()
        };
        if let Err(error) = validate_peer_name(&name) {
            self.add_error = error;
            return;
        }

        self.config.peers.push(FleetPeer {
            name,
            address,
            token,
        });
        if let Err(error) = self.save() {
            self.config.peers.pop();
            self.add_error = error;
            return;
        }

        self.invalidate_peer_set();
        self.name_input.clear();
        self.address_input.clear();
        self.token_input.clear();
    }

    fn save(&self) -> Result<(), String> {
        save_fleet_config(&self.config_path, &self.config)
    }

    fn sync_server(&mut self) {
        self.stop_server_only();
        self.server_error.clear();
        if !self.config.share_enabled {
            return;
        }
        match FleetServer::start(
            self.stats_path.clone(),
            self.config.share_port,
            self.config.share_token.clone(),
        ) {
            Ok(server) => self.server = Some(server),
            Err(error) => self.server_error = error,
        }
    }
}

impl Drop for FleetState {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// The Master Panel's own drawing.
//
// The sizes and colours below are measured off the commissioned mockup, not
// picked: a 30px header band filled like a card, 34px rows sitting straight on
// the page, and one hairline between them. That is what lets a fleet table be
// read at a glance, and it is why the rows are not striped and why the only
// amber on the screen is the status dot and the words at the top right.
// ---------------------------------------------------------------------------

const CARD_GAP: f32 = 12.0;
const TABLE_ROUND: f32 = 12.0;
const TABLE_HEAD_H: f32 = 30.0;
const TABLE_ROW_H: f32 = 34.0;
/// A row that has to explain itself is taller by one small line.
const TABLE_ROW_DETAIL_H: f32 = 46.0;
/// The left edge of the four value columns, as a fraction of the table's width.
const TABLE_COLS: [f32; 4] = [0.303, 0.503, 0.683, 0.843];
const TABLE_DOT_X: f32 = 13.0;
const TABLE_NAME_X: f32 = 35.0;
const HAIRLINE: egui::Color32 = egui::Color32::from_rgb(24, 24, 24);
/// The warning strip: a near-black amber wash, not a colour field. It has to be
/// readable every time the screen is opened without shouting every time.
const WARN_BG: egui::Color32 = egui::Color32::from_rgb(14, 11, 5);
const WARN_BORDER: egui::Color32 = egui::Color32::from_rgb(41, 32, 16);
const WARN_TEXT: egui::Color32 = egui::Color32::from_rgb(151, 139, 119);
/// The cell of a worker that is not reporting. An en dash, written as an escape
/// so the character itself never has to survive a copy and paste.
const NO_VALUE: &str = "\u{2013}";
const LOCAL_PROBE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowState {
    Online,
    Waiting,
    Offline,
}

impl RowState {
    /// Amber for a worker that is answering, dim amber while the first poll is
    /// still out, grey for one that is not there. Never green: this panel can
    /// say a worker replied, not that it is healthy.
    fn dot(self) -> egui::Color32 {
        match self {
            RowState::Online => theme::colors::ACCENT,
            RowState::Waiting => theme::colors::GOLD_DIM,
            RowState::Offline => theme::colors::TEXT_DIM,
        }
    }
}

struct WorkerRow {
    name: String,
    /// Second line under the name, drawn only when there is something to say:
    /// why this worker has no numbers. Empty for a worker that is answering.
    detail: String,
    state: RowState,
    hashrate: f64,
    hac_day: f64,
    watts: f64,
    height: u64,
}

impl WorkerRow {
    fn height_px(&self) -> f32 {
        if self.detail.is_empty() {
            TABLE_ROW_H
        } else {
            TABLE_ROW_DETAIL_H
        }
    }

    fn cells(&self) -> [String; 4] {
        if self.state != RowState::Online {
            return [
                NO_VALUE.to_string(),
                NO_VALUE.to_string(),
                NO_VALUE.to_string(),
                NO_VALUE.to_string(),
            ];
        }
        [
            format_hashrate(self.hashrate),
            format!("{:.4}", self.hac_day),
            format!("{:.0} W", self.watts),
            if self.height > 0 {
                crate::dashboard::group_thousands(self.height)
            } else {
                NO_VALUE.to_string()
            },
        ]
    }
}

/// True when the worker on this machine is currently writing its stats file.
///
/// The file is removed when mining stops, so its absence is a stop, and a file
/// that stopped moving is a worker that stopped answering. This is the same
/// staleness rule a peer's snapshot is held to, applied to the one worker that
/// does not arrive over the network.
fn local_worker_reporting(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age <= MAX_STATS_AGE,
        // A file stamped in the future is a clock that moved, not a dead rig.
        Err(_) => true,
    }
}

fn prop(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Proportional)
}

/// Cut a string to a printable length. The table paints straight onto its own
/// rectangle, so an over-long worker name would run across the next column
/// rather than wrap.
fn elide(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

/// The page heading: the screen's name, one quiet line about it, and the note
/// at the far right that says what this screen is allowed to do.
fn page_header(ui: &mut egui::Ui, title: &str, sub: &str, busy: Option<&str>, badge: &str) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(25.0)
                    .strong()
                    .color(theme::colors::TEXT),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(sub)
                    .size(12.0)
                    .color(theme::colors::TEXT_MUTED),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(
                egui::RichText::new(badge.to_uppercase())
                    .size(10.5)
                    .color(theme::colors::ACCENT),
            );
            if let Some(busy) = busy {
                ui.label(
                    egui::RichText::new(busy)
                        .size(10.5)
                        .color(theme::colors::TEXT_DIM),
                );
            }
        });
    });
}

/// One of the four figures across the top of the Master Panel.
fn overview_card(ui: &mut egui::Ui, width: f32, label: &str, value: &str) {
    egui::Frame::none()
        .fill(theme::colors::BG_INPUT)
        .stroke(egui::Stroke::new(1.0, theme::colors::BORDER_SOFT))
        .rounding(egui::Rounding::same(12.0))
        .inner_margin(egui::Margin::symmetric(15.0, 12.0))
        .show(ui, |ui| {
            ui.set_width((width - 30.0).max(1.0));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.label(
                    egui::RichText::new(label.to_uppercase())
                        .size(10.5)
                        .color(theme::colors::TEXT_DIM),
                );
                ui.label(
                    egui::RichText::new(value)
                        .size(19.0)
                        .strong()
                        .color(theme::colors::TEXT),
                );
            });
        });
}

/// The worker table.
///
/// Painted directly rather than assembled from widgets: the mockup's table is a
/// filled header band over unfilled rows with one hairline between them, and a
/// grid of labels cannot produce that without a fill behind every cell.
fn worker_table(ui: &mut egui::Ui, l: &PanelLabels, rows: &[WorkerRow]) {
    let width = ui.available_width();
    let height = TABLE_HEAD_H + rows.iter().map(WorkerRow::height_px).sum::<f32>();
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(width, height), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    let painter = ui.painter();
    let head = egui::Rect::from_min_size(rect.min, egui::Vec2::new(width, TABLE_HEAD_H));
    painter.rect_filled(
        head,
        egui::Rounding {
            nw: TABLE_ROUND,
            ne: TABLE_ROUND,
            sw: 0.0,
            se: 0.0,
        },
        theme::colors::BG_CARD,
    );

    let headings = [
        l.col_worker,
        l.col_hashrate,
        l.col_hac_day,
        l.col_power,
        l.col_height,
    ];
    painter.text(
        egui::pos2(rect.left() + TABLE_NAME_X, head.center().y),
        egui::Align2::LEFT_CENTER,
        headings[0].to_uppercase(),
        prop(10.5),
        theme::colors::TEXT_DIM,
    );
    for (i, fraction) in TABLE_COLS.iter().enumerate() {
        painter.text(
            egui::pos2(rect.left() + width * fraction, head.center().y),
            egui::Align2::LEFT_CENTER,
            headings[i + 1].to_uppercase(),
            prop(10.5),
            theme::colors::TEXT_DIM,
        );
    }

    // The name column runs to the first value column, less a gutter.
    let name_budget = ((width * TABLE_COLS[0] - TABLE_NAME_X - 14.0) / 6.6).max(8.0) as usize;
    let detail_budget = ((width - TABLE_NAME_X - 20.0) / 5.6).max(12.0) as usize;

    let mut y = rect.top() + TABLE_HEAD_H;
    for row in rows {
        let row_h = row.height_px();
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::Vec2::new(width, 1.0)),
            egui::Rounding::ZERO,
            HAIRLINE,
        );
        let line_y = if row.detail.is_empty() {
            y + row_h * 0.5
        } else {
            y + 16.0
        };
        painter.circle_filled(
            egui::pos2(rect.left() + TABLE_DOT_X, line_y),
            3.5,
            row.state.dot(),
        );
        painter.text(
            egui::pos2(rect.left() + TABLE_NAME_X, line_y),
            egui::Align2::LEFT_CENTER,
            elide(&row.name, name_budget),
            prop(12.5),
            if row.state == RowState::Online {
                theme::colors::TEXT
            } else {
                theme::colors::TEXT_MUTED
            },
        );
        if !row.detail.is_empty() {
            painter.text(
                egui::pos2(rect.left() + TABLE_NAME_X, y + row_h - 14.0),
                egui::Align2::LEFT_CENTER,
                elide(&row.detail, detail_budget),
                prop(10.5),
                theme::colors::TEXT_DIM,
            );
        }
        let cells = row.cells();
        let cell_color = if row.state == RowState::Online {
            theme::colors::TEXT_MUTED
        } else {
            theme::colors::TEXT_DIM
        };
        for (i, fraction) in TABLE_COLS.iter().enumerate() {
            painter.text(
                egui::pos2(rect.left() + width * fraction, line_y),
                egui::Align2::LEFT_CENTER,
                &cells[i],
                prop(12.0),
                cell_color,
            );
        }
        y += row_h;
    }

    painter.rect_stroke(
        rect,
        egui::Rounding::same(TABLE_ROUND),
        egui::Stroke::new(1.0, theme::colors::BORDER_SOFT),
    );
}

/// The one thing on this screen an operator must not skip.
fn warning_strip(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(WARN_BG)
        .stroke(egui::Stroke::new(1.0, WARN_BORDER))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(16.0, 11.0))
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.set_width(width);
            ui.label(egui::RichText::new(text).size(11.5).color(WARN_TEXT));
        });
}

fn hairline(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::new(ui.available_width(), 1.0),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, egui::Rounding::ZERO, HAIRLINE);
}

/// The open / closed marker, drawn rather than typed: the bundled font has no
/// caret glyph, and a text arrow at this size reads as a punctuation mistake.
fn chevron(painter: &egui::Painter, center: egui::Pos2, open: bool, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.4, color);
    let (dx, dy) = (4.5_f32, 2.6_f32);
    let (near, far) = if open { (dy, -dy) } else { (-dy, dy) };
    painter.line_segment(
        [
            egui::pos2(center.x - dx, center.y + near),
            egui::pos2(center.x, center.y + far),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y + far),
            egui::pos2(center.x + dx, center.y + near),
        ],
        stroke,
    );
}

/// A card whose body folds away: title, one quiet line beside it, a caret at
/// the right, and the content under a hairline once it is open.
///
/// `id` names the fold state, so the same card drawn on two screens can be open
/// on one and shut on the other.
fn collapsible_card(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    sub: &str,
    content: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.make_persistent_id(id);
    let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    egui::Frame::none()
        .fill(theme::colors::BG_CARD)
        .stroke(egui::Stroke::new(1.0, theme::colors::BORDER_SOFT))
        .rounding(egui::Rounding::same(14.0))
        .inner_margin(egui::Margin::symmetric(18.0, 15.0))
        .show(ui, |ui| {
            let width = ui.available_width();
            ui.set_width(width);
            let (rect, response) =
                ui.allocate_exact_size(egui::Vec2::new(width, 22.0), egui::Sense::click());
            if ui.is_rect_visible(rect) {
                let hovered = response.hovered();
                let title_color = if hovered {
                    theme::colors::ACCENT
                } else {
                    theme::colors::TEXT
                };
                let painter = ui.painter();
                let galley = painter.layout_no_wrap(title.to_owned(), prop(14.5), title_color);
                let title_w = galley.rect.width();
                painter.galley(
                    egui::pos2(rect.left(), rect.center().y - galley.rect.height() * 0.5),
                    galley,
                    title_color,
                );
                if !sub.is_empty() {
                    painter.text(
                        egui::pos2(rect.left() + title_w + 12.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        sub,
                        prop(11.5),
                        theme::colors::TEXT_DIM,
                    );
                }
                chevron(
                    painter,
                    egui::pos2(rect.right() - 9.0, rect.center().y),
                    open,
                    if hovered {
                        theme::colors::TEXT
                    } else {
                        theme::colors::TEXT_MUTED
                    },
                );
            }
            if response.clicked() {
                open = !open;
                ui.data_mut(|d| d.insert_temp(id, open));
            }
            if open {
                ui.add_space(13.0);
                hairline(ui);
                ui.add_space(13.0);
                content(ui);
            }
        });
}

fn format_hashrate(hashrate: f64) -> String {
    if hashrate >= 1_000_000_000.0 {
        format!("{:.2} GH/s", hashrate / 1_000_000_000.0)
    } else if hashrate >= 1_000_000.0 {
        format!("{:.2} MH/s", hashrate / 1_000_000.0)
    } else if hashrate >= 1_000.0 {
        format!("{:.2} kH/s", hashrate / 1_000.0)
    } else {
        format!("{hashrate:.0} H/s")
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_share_token() -> String {
    try_generate_token().unwrap_or_default()
}

fn try_generate_token() -> Result<String, String> {
    let mut random = [0u8; TOKEN_RANDOM_BYTES];
    getrandom::fill(&mut random)
        .map_err(|error| format!("Secure OS random generator is unavailable: {error}"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn validate_token(token: &str) -> Result<(), String> {
    let bytes = token.as_bytes();
    if bytes.len() != TOKEN_HEX_BYTES {
        return Err(format!(
            "use the generated {TOKEN_HEX_BYTES}-character token"
        ));
    }
    if !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err("use hexadecimal characters only".to_string());
    }
    Ok(())
}

fn token_matches(expected: &str, supplied: Option<&str>) -> bool {
    let Some(supplied) = supplied else {
        return false;
    };
    let expected = expected.as_bytes();
    let supplied = supplied.as_bytes();
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .iter()
        .zip(supplied)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn validate_peer_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Enter a miner name.".to_string());
    }
    if name.len() > MAX_PEER_NAME_BYTES {
        return Err(format!(
            "Miner name must be at most {MAX_PEER_NAME_BYTES} bytes."
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("Miner name cannot contain control characters.".to_string());
    }
    Ok(())
}

fn normalize_peer_address(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    let invalid = || "Use only host:port, for example 192.168.1.42:19120.".to_string();
    if value.is_empty() {
        return Err("Enter the remote miner LAN address.".to_string());
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
        || value.contains("://")
        || value.contains('/')
        || value.contains('\\')
        || value.contains('?')
        || value.contains('#')
        || value.contains('@')
    {
        return Err(invalid());
    }

    if let Some(rest) = value.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']').ok_or_else(invalid)?;
        let port_text = remainder.strip_prefix(':').ok_or_else(invalid)?;
        let address = host
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| "The IPv6 address is invalid.".to_string())?;
        if !is_lan_ip(IpAddr::V6(address)) {
            return Err(
                "Only private, link-local, or loopback LAN addresses are allowed.".to_string(),
            );
        }
        let port = parse_peer_port(port_text)?;
        return Ok(format!("[{address}]:{port}"));
    }

    let (host, port_text) = value
        .rsplit_once(':')
        .ok_or_else(|| "The address needs a valid port.".to_string())?;
    if host.is_empty() || host.contains(':') {
        return Err("IPv6 addresses must use brackets, for example [::1]:19120.".to_string());
    }
    let port = parse_peer_port(port_text)?;

    if let Ok(address) = host.parse::<std::net::Ipv4Addr>() {
        if !is_lan_ip(IpAddr::V4(address)) {
            return Err(
                "Only private, link-local, or loopback LAN addresses are allowed.".to_string(),
            );
        }
        return Ok(format!("{address}:{port}"));
    }
    validate_dns_host(host)?;
    Ok(format!("{}:{port}", host.to_ascii_lowercase()))
}

fn parse_peer_port(raw: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| "The address needs a valid port.".to_string())
}

fn validate_dns_host(host: &str) -> Result<(), String> {
    if host.len() > 253 || !host.is_ascii() {
        return Err("The hostname is invalid.".to_string());
    }
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("The hostname is invalid.".to_string());
        }
    }
    Ok(())
}

fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            address.is_private() || address.is_loopback() || address.is_link_local()
        }
        IpAddr::V6(address) => {
            if address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
            {
                true
            } else if let Some(mapped) = address.to_ipv4_mapped() {
                mapped.is_private() || mapped.is_loopback() || mapped.is_link_local()
            } else {
                false
            }
        }
    }
}

fn read_fleet_config(path: &Path) -> Result<Option<FleetConfig>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read miner fleet settings: {error}")),
    };
    let mut body = String::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_string(&mut body)
        .map_err(|error| format!("Cannot read miner fleet settings: {error}"))?;
    if body.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "Miner fleet settings exceed the {MAX_CONFIG_BYTES}-byte safety limit; defaults were loaded."
        ));
    }
    let config = serde_json::from_str::<FleetConfig>(&body).map_err(|error| {
        format!("Miner fleet settings are invalid; defaults were loaded: {error}")
    })?;
    Ok(Some(config))
}

fn sanitize_loaded_config(config: &mut FleetConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    if config.share_port < 1024 {
        config.share_port = DEFAULT_PORT;
        warnings.push(format!(
            "Invalid fleet sharing port was replaced with {DEFAULT_PORT}."
        ));
    }

    config.share_token = config.share_token.trim().to_string();
    if validate_token(&config.share_token).is_err() {
        warnings.push("Invalid fleet sharing token was replaced.".to_string());
    }

    let original_count = config.peers.len();
    let mut peers = Vec::with_capacity(original_count.min(MAX_PEERS));
    let mut seen = HashSet::new();
    for mut peer in std::mem::take(&mut config.peers)
        .into_iter()
        .take(MAX_PEERS)
    {
        let address = match normalize_peer_address(&peer.address) {
            Ok(address) => address,
            Err(_) => continue,
        };
        peer.token = peer.token.trim().to_string();
        if validate_token(&peer.token).is_err() {
            continue;
        }
        peer.name = if peer.name.trim().is_empty() {
            address.clone()
        } else {
            peer.name.trim().to_string()
        };
        if validate_peer_name(&peer.name).is_err() || !seen.insert(address.clone()) {
            continue;
        }
        peer.address = address;
        peers.push(peer);
    }

    let dropped = original_count.saturating_sub(peers.len());
    if dropped > 0 {
        warnings.push(format!(
            "Removed {dropped} invalid, duplicate, or excess fleet miner entries."
        ));
    }
    config.peers = peers;
    warnings
}

fn save_fleet_config(path: &Path, config: &FleetConfig) -> Result<(), String> {
    let mut raw = serde_json::to_string_pretty(config)
        .map_err(|error| format!("Cannot serialize miner fleet settings: {error}"))?;
    raw.push('\n');
    atomic_write_private(path, &raw)
        .map_err(|error| format!("Cannot save private miner fleet settings: {error}"))
}

fn apply_poll_batch(
    current_generation: u64,
    current_results: &mut Vec<PeerResult>,
    batch: PollBatch,
) -> bool {
    if batch.generation != current_generation {
        return false;
    }
    *current_results = batch.results;
    true
}

fn poll_peers_bounded(peers: Vec<FleetPeer>) -> Vec<PeerResult> {
    if peers.is_empty() {
        return Vec::new();
    }

    let peer_count = peers.len();
    let fallback_peers = peers.clone();
    let jobs = Arc::new(Mutex::new(
        peers.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let (result_tx, result_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(POLL_WORKERS.min(peer_count));
    for _ in 0..POLL_WORKERS.min(peer_count) {
        let jobs = Arc::clone(&jobs);
        let result_tx = result_tx.clone();
        workers.push(thread::spawn(move || {
            loop {
                let job = match jobs.lock() {
                    Ok(mut jobs) => jobs.pop_front(),
                    Err(_) => None,
                };
                let Some((index, peer)) = job else {
                    break;
                };
                if result_tx.send((index, poll_peer(peer))).is_err() {
                    break;
                }
            }
        }));
    }
    drop(result_tx);

    let mut ordered = vec![None; peer_count];
    for (index, result) in result_rx {
        if index < ordered.len() {
            ordered[index] = Some(result);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }

    ordered
        .into_iter()
        .zip(fallback_peers)
        .map(|(result, peer)| {
            result.unwrap_or(PeerResult {
                peer,
                stats: None,
                error: "Poll worker failed".to_string(),
            })
        })
        .collect()
}

fn poll_peer(mut peer: FleetPeer) -> PeerResult {
    let normalized = match normalize_peer_address(&peer.address) {
        Ok(address) => address,
        Err(error) => {
            return PeerResult {
                peer,
                stats: None,
                error,
            };
        }
    };
    peer.address = normalized;
    if let Err(error) = validate_token(&peer.token) {
        return PeerResult {
            peer,
            stats: None,
            error: format!("Invalid token: {error}"),
        };
    }

    match fetch_peer_stats(&peer) {
        Ok(stats) => PeerResult {
            peer,
            stats: Some(stats),
            error: String::new(),
        },
        Err(error) => PeerResult {
            peer,
            stats: None,
            error,
        },
    }
}

fn fetch_peer_stats(peer: &FleetPeer) -> Result<MiningStatsSnapshot, String> {
    fetch_peer_stats_with_budget(peer, PEER_WALL_TIMEOUT)
}

fn fetch_peer_stats_with_budget(
    peer: &FleetPeer,
    wall_timeout: Duration,
) -> Result<MiningStatsSnapshot, String> {
    let deadline = Instant::now() + wall_timeout;
    let address = normalize_peer_address(&peer.address)?;
    validate_token(&peer.token).map_err(|error| format!("Invalid token: {error}"))?;
    let addrs = resolve_peer_addresses(&address, deadline)?;
    let mut stream = None;
    for addr in addrs {
        let timeout = remaining_until(deadline)?.min(PEER_CONNECT_SLICE);
        if let Ok(connected) = TcpStream::connect_timeout(&addr, timeout) {
            stream = Some(connected);
            break;
        }
    }
    let mut stream = stream.ok_or_else(|| "Offline".to_string())?;
    stream
        .set_nonblocking(true)
        .map_err(|_| "Cannot configure connection".to_string())?;
    let request = format!(
        "GET /api/v1/stats HTTP/1.1\r\nHost: {address}\r\nX-Hacash-Token: {}\r\nConnection: close\r\n\r\n",
        peer.token
    );
    write_before_deadline(&mut stream, request.as_bytes(), deadline)?;

    let response_bytes = read_before_deadline(&mut stream, deadline)?;
    let response =
        std::str::from_utf8(&response_bytes).map_err(|_| "Invalid response".to_string())?;
    if !response.starts_with("HTTP/1.1 200") {
        return Err(if response.starts_with("HTTP/1.1 401") {
            "Wrong token".to_string()
        } else {
            "Stats unavailable".to_string()
        });
    }
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| "Invalid response".to_string())?;
    let stats = serde_json::from_str(body).map_err(|_| "Invalid stats".to_string())?;
    validate_stats_snapshot(&stats)?;
    Ok(stats)
}

struct DnsResolverSlot;

impl Drop for DnsResolverSlot {
    fn drop(&mut self) {
        DNS_RESOLVERS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

fn resolve_peer_addresses(address: &str, deadline: Instant) -> Result<Vec<SocketAddr>, String> {
    if let Ok(socket_address) = address.parse::<SocketAddr>() {
        return if is_lan_ip(socket_address.ip()) {
            Ok(vec![socket_address])
        } else {
            Err("Only LAN addresses are allowed".to_string())
        };
    }

    DNS_RESOLVERS_IN_FLIGHT
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_DNS_RESOLVERS).then_some(current + 1)
        })
        .map_err(|_| "DNS resolver is busy".to_string())?;

    let owned_address = address.to_string();
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("hacash-fleet-dns".to_string())
        .spawn(move || {
            let _slot = DnsResolverSlot;
            let result = owned_address
                .to_socket_addrs()
                .map(filter_lan_addresses)
                .map_err(|_| ());
            let _ = tx.send(result);
        });
    if let Err(error) = spawned {
        DNS_RESOLVERS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        return Err(format!("Cannot start DNS resolver: {error}"));
    }

    let addresses = rx
        .recv_timeout(remaining_until(deadline)?)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => "Timed out".to_string(),
            mpsc::RecvTimeoutError::Disconnected => "Invalid address".to_string(),
        })?
        .map_err(|_| "Invalid address".to_string())?;
    if addresses.is_empty() {
        Err("Hostname did not resolve to a private LAN address".to_string())
    } else {
        Ok(addresses)
    }
}

fn filter_lan_addresses(addresses: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    addresses
        .into_iter()
        .filter(|address| is_lan_ip(address.ip()))
        .filter(|address| seen.insert(*address))
        .take(MAX_RESOLVED_ADDRESSES)
        .collect()
}

fn remaining_until(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "Timed out".to_string())
}

fn wait_for_socket(deadline: Instant) -> Result<(), String> {
    thread::sleep(remaining_until(deadline)?.min(Duration::from_millis(5)));
    Ok(())
}

fn write_before_deadline(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), String> {
    while !bytes.is_empty() {
        remaining_until(deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err("Request failed".to_string()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_socket(deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err("Request failed".to_string()),
        }
    }
    Ok(())
}

fn read_before_deadline(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>, String> {
    let mut response = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        remaining_until(deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&chunk[..read]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err("Stats response is too large".to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_for_socket(deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err("Invalid response".to_string()),
        }
    }
    Ok(response)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestHeaderReadError {
    Timeout,
    TooLarge,
    Invalid,
}

trait RequestHeaderReader: Read {
    fn set_header_read_timeout(&self, timeout: Duration) -> std::io::Result<()>;
}

impl RequestHeaderReader for TcpStream {
    fn set_header_read_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }
}

fn remaining_request_time(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

fn read_request_header_with_clock<R, F>(
    reader: &mut R,
    deadline: Instant,
    mut now: F,
) -> Result<Vec<u8>, RequestHeaderReadError>
where
    R: RequestHeaderReader,
    F: FnMut() -> Instant,
{
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        if request.len() >= MAX_REQUEST_HEADER_BYTES {
            return Err(RequestHeaderReadError::TooLarge);
        }

        let remaining =
            remaining_request_time(deadline, now()).ok_or(RequestHeaderReadError::Timeout)?;
        reader
            .set_header_read_timeout(remaining)
            .map_err(|_| RequestHeaderReadError::Invalid)?;

        match reader.read(&mut chunk) {
            Ok(0) => return Err(RequestHeaderReadError::Invalid),
            Ok(read) => {
                request.extend_from_slice(&chunk[..read]);
                if request.len() > MAX_REQUEST_HEADER_BYTES {
                    return Err(RequestHeaderReadError::TooLarge);
                }

                if remaining_request_time(deadline, now()).is_none() {
                    return Err(RequestHeaderReadError::Timeout);
                }

                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    return Ok(request);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(RequestHeaderReadError::Timeout);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(RequestHeaderReadError::Invalid),
        }
    }
}

fn serve_stats_request(mut stream: TcpStream, stats_path: &Path, token: &str, deadline: Instant) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));

    let request_bytes = match read_request_header_with_clock(&mut stream, deadline, Instant::now) {
        Ok(request) => request,
        Err(_) => {
            write_http_response(
                &mut stream,
                401,
                "application/json",
                r#"{"error":"unauthorized"}"#,
                deadline,
            );
            return;
        }
    };
    let Ok(request) = std::str::from_utf8(&request_bytes) else {
        write_http_response(
            &mut stream,
            401,
            "application/json",
            r#"{"error":"unauthorized"}"#,
            deadline,
        );
        return;
    };

    let mut lines = request.split("\r\n");
    let valid_route = matches!(
        lines.next(),
        Some("GET /api/v1/stats HTTP/1.1" | "GET /api/v1/stats HTTP/1.0")
    );
    let mut supplied_token = None;
    let mut duplicate_token = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("X-Hacash-Token") {
            if supplied_token.is_some() {
                duplicate_token = true;
            } else {
                supplied_token = Some(value.trim());
            }
        }
    }

    if !valid_route || duplicate_token || !token_matches(token, supplied_token) {
        write_http_response(
            &mut stream,
            401,
            "application/json",
            r#"{"error":"unauthorized"}"#,
            deadline,
        );
        return;
    }

    match read_stats_body(stats_path, deadline) {
        Ok(body) => write_http_response(&mut stream, 200, "application/json", &body, deadline),
        Err(_) => write_http_response(
            &mut stream,
            503,
            "application/json",
            r#"{"error":"stats unavailable"}"#,
            deadline,
        ),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn string_is_sane(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn validate_stats_snapshot(stats: &MiningStatsSnapshot) -> Result<(), String> {
    let now = now_unix_ms();
    let max_age_ms = MAX_STATS_AGE.as_millis() as u64;
    let future_skew_ms = MAX_STATS_FUTURE_SKEW.as_millis() as u64;
    if stats.updated_unix_ms == 0 || now.saturating_sub(stats.updated_unix_ms) > max_age_ms {
        return Err("Stale stats".to_string());
    }
    if stats.updated_unix_ms > now.saturating_add(future_skew_ms) {
        return Err("Invalid stats timestamp".to_string());
    }

    let hash_values = [
        stats.hashrate_hps,
        stats.gpu_hashrate_hps,
        stats.cpu_hashrate_hps,
    ];
    if hash_values
        .into_iter()
        .any(|value| !value.is_finite() || !(0.0..=MAX_HASHRATE_HPS).contains(&value))
        || !stats.watts.is_finite()
        || !(0.0..=MAX_POWER_WATTS).contains(&stats.watts)
        || !stats.kh_per_j.is_finite()
        || !(0.0..=MAX_HASHRATE_HPS).contains(&stats.kh_per_j)
        || !stats.hac_per_day.is_finite()
        || !(0.0..=MAX_DAILY_VALUE).contains(&stats.hac_per_day)
        || !stats.network_pct.is_finite()
        || !(0.0..=100.0).contains(&stats.network_pct)
        || !stats.daily_cost_eur.is_finite()
        || !(0.0..=MAX_DAILY_VALUE).contains(&stats.daily_cost_eur)
        || !stats.daily_revenue_eur.is_finite()
        || !(0.0..=MAX_DAILY_VALUE).contains(&stats.daily_revenue_eur)
        || !stats.daily_net_eur.is_finite()
        || stats.daily_net_eur.abs() > MAX_DAILY_VALUE
    {
        return Err("Invalid numeric stats".to_string());
    }

    if stats.configured_work_groups > 100_000_000
        || stats.oom_allowed_work_groups > 100_000_000
        || stats.thermal_cap_work_groups > 100_000_000
        || stats.effective_work_groups > 100_000_000
        || stats.active_cpu_threads > 1_000_000
    {
        return Err("Invalid mining limits".to_string());
    }

    let strings = [
        (&stats.status, 64),
        (&stats.hashrate_display, MAX_STATS_STRING_BYTES),
        (&stats.gpu_profile, MAX_STATS_STRING_BYTES),
        (&stats.gpu_hashrate_display, MAX_STATS_STRING_BYTES),
        (&stats.mining_kind, 64),
        (&stats.diamond_best, MAX_STATS_STRING_BYTES),
    ];
    if strings
        .into_iter()
        .any(|(value, max_bytes)| !string_is_sane(value, max_bytes))
    {
        return Err("Invalid stats text".to_string());
    }
    Ok(())
}

fn read_stats_body(path: &Path, deadline: Instant) -> Result<String, String> {
    remaining_request_time(deadline, Instant::now()).ok_or_else(|| "timeout".to_string())?;
    let metadata = fs::symlink_metadata(path).map_err(|_| "stats unavailable".to_string())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_RESPONSE_BYTES as u64
    {
        return Err("stats unavailable".to_string());
    }
    let file = fs::File::open(path).map_err(|_| "stats unavailable".to_string())?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| "stats unavailable".to_string())?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_RESPONSE_BYTES as u64 {
        return Err("stats unavailable".to_string());
    }
    let mut body = String::new();
    file.take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_string(&mut body)
        .map_err(|_| "stats unavailable".to_string())?;
    remaining_request_time(deadline, Instant::now()).ok_or_else(|| "timeout".to_string())?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("stats unavailable".to_string());
    }
    let stats: MiningStatsSnapshot =
        serde_json::from_str(&body).map_err(|_| "stats unavailable".to_string())?;
    validate_stats_snapshot(&stats).map_err(|_| "stats unavailable".to_string())?;
    let sanitized = serde_json::to_string(&stats).map_err(|_| "stats unavailable".to_string())?;
    if sanitized.len() > MAX_RESPONSE_BYTES {
        return Err("stats unavailable".to_string());
    }
    Ok(sanitized)
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    deadline: Instant,
) {
    if remaining_request_time(deadline, Instant::now()).is_none() {
        return;
    }
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        _ => "Service Unavailable",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    if stream.set_nonblocking(true).is_ok() {
        let _ = write_before_deadline(stream, response.as_bytes(), deadline);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const WRONG_TOKEN: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    struct ScriptedHeaderReader {
        chunks: VecDeque<Vec<u8>>,
        timeouts: std::cell::RefCell<Vec<Duration>>,
    }

    impl Read for ScriptedHeaderReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            assert!(chunk.len() <= buffer.len());
            buffer[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    impl RequestHeaderReader for ScriptedHeaderReader {
        fn set_header_read_timeout(&self, timeout: Duration) -> std::io::Result<()> {
            self.timeouts.borrow_mut().push(timeout);
            Ok(())
        }
    }

    fn test_result(address: &str) -> PeerResult {
        PeerResult {
            peer: FleetPeer {
                name: address.to_string(),
                address: address.to_string(),
                token: TEST_TOKEN.to_string(),
            },
            stats: None,
            error: String::new(),
        }
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let suffix = try_generate_token().unwrap();
        std::env::temp_dir().join(format!(
            "hacash-fleet-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn peer_address_accepts_canonical_lan_forms() {
        assert_eq!(
            normalize_peer_address(" 192.168.1.42:19120 ").unwrap(),
            "192.168.1.42:19120"
        );
        assert_eq!(
            normalize_peer_address("Rig-01.LOCAL:19120").unwrap(),
            "rig-01.local:19120"
        );
        assert_eq!(
            normalize_peer_address("[fd00::1]:19120").unwrap(),
            "[fd00::1]:19120"
        );
        assert_eq!(
            normalize_peer_address("[::1]:19120").unwrap(),
            "[::1]:19120"
        );
    }

    #[test]
    fn peer_address_rejects_injection_and_invalid_hosts() {
        for invalid in [
            "http://host:19120",
            "host",
            "host:0",
            ":19120",
            "bad_host:19120",
            "-bad.local:19120",
            "bad-.local:19120",
            "host:19120/path",
            "2001:db8::1:19120",
            "host:19120@elsewhere",
            "host:19120\r\nX-Hacash-Token: injected",
            "8.8.8.8:19120",
            "[2001:db8::1]:19120",
        ] {
            assert!(
                normalize_peer_address(invalid).is_err(),
                "address should be rejected: {invalid:?}"
            );
        }
    }

    #[test]
    fn fleet_config_defaults_to_private_and_tokenized() {
        let first = FleetConfig::default();
        let second = FleetConfig::default();
        assert!(!first.share_enabled);
        assert_eq!(first.share_port, DEFAULT_PORT);
        assert_eq!(first.share_token.len(), TOKEN_HEX_BYTES);
        assert!(validate_token(&first.share_token).is_ok());
        assert_ne!(first.share_token, second.share_token);
        assert!(token_matches(&first.share_token, Some(&first.share_token)));
        assert!(!token_matches(
            &first.share_token,
            Some(&second.share_token)
        ));
        assert!(!token_matches(&first.share_token, None));
        assert!(validate_token(&"g".repeat(TOKEN_HEX_BYTES)).is_err());
    }

    #[test]
    fn source_limiter_caps_concurrency_rate_and_ipv4_mapped_aliases() {
        let base = Instant::now();
        let source: IpAddr = "192.168.10.20".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.10.20".parse().unwrap();
        let mut limiter = SourceLimiter::default();

        assert!(limiter.try_acquire(source, base));
        assert!(limiter.try_acquire(source, base));
        assert!(!limiter.try_acquire(source, base));
        limiter.release(source, base);
        limiter.release(source, base);
        assert!(limiter.try_acquire(source, base));
        limiter.release(source, base);
        assert!(limiter.try_acquire(source, base));
        limiter.release(source, base);
        assert!(!limiter.try_acquire(mapped, base));
        assert!(limiter.try_acquire(mapped, base + SOURCE_TOKEN_REFILL));
    }

    #[test]
    fn stale_poll_batch_cannot_restore_a_removed_peer() {
        let mut results = vec![test_result("kept.local:19120")];
        let stale = PollBatch {
            generation: 4,
            results: vec![test_result("removed.local:19120")],
        };
        assert!(!apply_poll_batch(5, &mut results, stale));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].peer.address, "kept.local:19120");

        let fresh = PollBatch {
            generation: 5,
            results: vec![test_result("fresh.local:19120")],
        };
        assert!(apply_poll_batch(5, &mut results, fresh));
        assert_eq!(results[0].peer.address, "fresh.local:19120");
    }

    #[test]
    fn fleet_config_save_is_atomic_and_private() {
        let directory = temp_test_dir("config");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("miner-fleet.json");
        let mut config = FleetConfig::default();
        config.share_port = 19_121;
        save_fleet_config(&path, &config).unwrap();
        config.share_port = 19_122;
        save_fleet_config(&path, &config).unwrap();

        let saved: FleetConfig = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.share_port, 19_122);
        let leftovers = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn resolved_addresses_are_lan_only_deduplicated_and_capped() {
        let addresses = [
            "8.8.8.8:19120".parse().unwrap(),
            "10.0.0.1:19120".parse().unwrap(),
            "10.0.0.1:19120".parse().unwrap(),
            "192.168.1.2:19120".parse().unwrap(),
            "172.16.0.3:19120".parse().unwrap(),
            "127.0.0.1:19120".parse().unwrap(),
            "169.254.1.1:19120".parse().unwrap(),
        ];
        let filtered = filter_lan_addresses(addresses);
        assert_eq!(filtered.len(), MAX_RESOLVED_ADDRESSES);
        assert!(filtered.iter().all(|address| is_lan_ip(address.ip())));
        assert_eq!(
            filtered
                .iter()
                .filter(|address| address.ip() == "10.0.0.1".parse::<IpAddr>().unwrap())
                .count(),
            1
        );
    }

    #[test]
    fn loaded_config_is_normalized_and_invalid_peers_are_removed() {
        let mut config = FleetConfig::default();
        config.share_port = 80;
        config.share_token = " invalid ".to_string();
        config.peers = vec![
            FleetPeer {
                name: " Rig One ".to_string(),
                address: " 192.168.1.9:19120 ".to_string(),
                token: TEST_TOKEN.to_string(),
            },
            FleetPeer {
                name: "duplicate".to_string(),
                address: "192.168.1.9:19120".to_string(),
                token: TEST_TOKEN.to_string(),
            },
            FleetPeer {
                name: "public".to_string(),
                address: "8.8.8.8:19120".to_string(),
                token: TEST_TOKEN.to_string(),
            },
            FleetPeer {
                name: "weak token".to_string(),
                address: "192.168.1.10:19120".to_string(),
                token: "weak".to_string(),
            },
        ];

        let warnings = sanitize_loaded_config(&mut config);
        assert_eq!(config.share_port, DEFAULT_PORT);
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].name, "Rig One");
        assert_eq!(config.peers[0].address, "192.168.1.9:19120");
        assert!(warnings.iter().any(|warning| warning.contains("port")));
        assert!(warnings.iter().any(|warning| warning.contains("3 invalid")));
    }

    #[test]
    fn fleet_config_read_has_a_hard_size_limit() {
        let directory = temp_test_dir("oversize-config");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("miner-fleet.json");
        fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();

        let error = read_fleet_config(&path).unwrap_err();
        assert!(error.contains("safety limit"));

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn stats_validation_rejects_stale_and_unreasonable_snapshots() {
        let mut stats = MiningStatsSnapshot {
            status: "mining".to_string(),
            updated_unix_ms: now_unix_ms(),
            ..Default::default()
        };
        assert!(validate_stats_snapshot(&stats).is_ok());

        stats.updated_unix_ms = now_unix_ms().saturating_sub(31_000);
        assert_eq!(validate_stats_snapshot(&stats).unwrap_err(), "Stale stats");

        stats.updated_unix_ms = now_unix_ms();
        stats.watts = f64::INFINITY;
        assert_eq!(
            validate_stats_snapshot(&stats).unwrap_err(),
            "Invalid numeric stats"
        );

        stats.watts = 0.0;
        stats.status = "mining\nforged".to_string();
        assert_eq!(
            validate_stats_snapshot(&stats).unwrap_err(),
            "Invalid stats text"
        );
    }

    #[test]
    fn stats_body_is_regular_bounded_and_reserialized() {
        let directory = temp_test_dir("sanitize-stats");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("stats.json");
        let stats = MiningStatsSnapshot {
            status: "mining".to_string(),
            updated_unix_ms: now_unix_ms(),
            ..Default::default()
        };
        let mut value = serde_json::to_value(&stats).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("injected".to_string(), serde_json::json!("hidden"));
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let body = read_stats_body(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(!body.contains("injected"));
        assert!(serde_json::from_str::<MiningStatsSnapshot>(&body).is_ok());

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn peer_fetch_respects_one_wall_clock_budget() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(400));
        });
        let peer = FleetPeer {
            name: "slow".to_string(),
            address: format!("127.0.0.1:{port}"),
            token: TEST_TOKEN.to_string(),
        };

        let started = Instant::now();
        assert!(fetch_peer_stats_with_budget(&peer, Duration::from_millis(150)).is_err());
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(350), "elapsed: {elapsed:?}");
        server.join().unwrap();
    }

    #[test]
    fn request_header_deadline_does_not_reset_between_reads() {
        let base = Instant::now();
        let deadline = base + Duration::from_secs(1);
        let mut reader = ScriptedHeaderReader {
            chunks: VecDeque::from([b"G".to_vec(), b"E".to_vec(), b"T".to_vec()]),
            timeouts: std::cell::RefCell::new(Vec::new()),
        };
        let mut times = [
            base,
            base + Duration::from_millis(100),
            base + Duration::from_millis(800),
            base + Duration::from_millis(900),
            deadline,
        ]
        .into_iter();

        assert_eq!(
            read_request_header_with_clock(&mut reader, deadline, || times.next().unwrap()),
            Err(RequestHeaderReadError::Timeout)
        );
        assert_eq!(
            *reader.timeouts.borrow(),
            [Duration::from_secs(1), Duration::from_millis(200)]
        );
        assert_eq!(reader.chunks.len(), 1);
    }

    #[test]
    fn request_header_completed_at_deadline_is_rejected() {
        let base = Instant::now();
        let deadline = base + Duration::from_secs(1);
        let mut reader = ScriptedHeaderReader {
            chunks: VecDeque::from([
                b"GET /api/v1/stats HTTP/1.1\r\nX-Hacash-Token: token\r\n\r\n".to_vec(),
            ]),
            timeouts: std::cell::RefCell::new(Vec::new()),
        };
        let mut times = [base, deadline].into_iter();

        assert_eq!(
            read_request_header_with_clock(&mut reader, deadline, || times.next().unwrap()),
            Err(RequestHeaderReadError::Timeout)
        );
    }

    #[test]
    fn lan_stats_endpoint_is_bounded_concurrent_and_token_protected() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let directory = temp_test_dir("stats");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("stats.json");
        let stats = MiningStatsSnapshot {
            status: "mining".to_string(),
            hashrate_hps: 42_000.0,
            updated_unix_ms: now_unix_ms(),
            ..Default::default()
        };
        fs::write(&path, serde_json::to_string(&stats).unwrap()).unwrap();
        let mut server = FleetServer::start(path.clone(), port, TEST_TOKEN.to_string()).unwrap();
        assert_eq!(server.worker_threads.len(), SERVER_WORKERS);

        let mut slow_clients = Vec::new();
        for _ in 0..usize::from(MAX_IN_FLIGHT_PER_SOURCE) {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .write_all(b"GET /api/v1/stats HTTP/1.1\r\nHost: localhost\r\n")
                .unwrap();
            slow_clients.push(stream);
        }
        thread::sleep(Duration::from_millis(150));
        drop(slow_clients);
        thread::sleep(Duration::from_millis(150));

        let peer = FleetPeer {
            name: "test".to_string(),
            address: format!("127.0.0.1:{port}"),
            token: TEST_TOKEN.to_string(),
        };
        let fetched = fetch_peer_stats(&peer).unwrap();
        assert_eq!(fetched.status, "mining");
        assert_eq!(fetched.hashrate_hps, 42_000.0);

        let wrong = FleetPeer {
            token: WRONG_TOKEN.to_string(),
            ..peer
        };
        assert_eq!(fetch_peer_stats(&wrong).unwrap_err(), "Wrong token");

        server.stop();
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn lan_server_refuses_weak_tokens() {
        let directory = temp_test_dir("weak-token");
        let error = FleetServer::start(directory.join("stats.json"), 0, "predictable".to_string())
            .err()
            .expect("weak token must be rejected");
        assert!(error.contains("secure access token"));
    }
}
