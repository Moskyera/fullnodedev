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
const MAX_STATS_AGE: Duration = Duration::from_secs(30);
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

    pub fn poll(&mut self) {
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

    pub fn show_settings(&mut self, ui: &mut egui::Ui) {
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new("MINER FLEET • LAN SHARING")
                    .strong()
                    .size(12.0)
                    .color(theme::colors::ACCENT),
            );
            ui.label(
                egui::RichText::new(
                    "Share read-only statistics with another dashboard on your local network.",
                )
                .color(theme::colors::TEXT_MUTED)
                .size(11.5),
            );
            ui.add_space(10.0);

            let enabled_changed = ui
                .checkbox(
                    &mut self.config.share_enabled,
                    "Allow this miner to be monitored over LAN",
                )
                .changed();
            let mut restart = enabled_changed;
            ui.add_enabled_ui(self.config.share_enabled, |ui| {
                egui::Grid::new("fleet_share_grid")
                    .num_columns(2)
                    .spacing([18.0, 8.0])
                    .show(ui, |ui| {
                        theme::field_label(ui, "Port:");
                        restart |= ui
                            .add(
                                egui::DragValue::new(&mut self.config.share_port)
                                    .range(1024..=65535),
                            )
                            .changed();
                        ui.end_row();

                        theme::field_label(ui, "Access token:");
                        ui.horizontal(|ui| {
                            let mut masked_token = self.config.share_token.clone();
                            ui.add_enabled(
                                false,
                                egui::TextEdit::singleline(&mut masked_token)
                                    .password(true)
                                    .desired_width(240.0),
                            );
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(self.config.share_token.clone());
                            }
                            if ui.small_button("New token").clicked() {
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
                        ui.end_row();
                    });
                ui.label(
                    egui::RichText::new(format!(
                        "Add this miner on another panel as <this-PC-IP>:{} and copy the token.",
                        self.config.share_port
                    ))
                    .color(theme::colors::TEXT_MUTED)
                    .size(11.0),
                );
                ui.label(
                    egui::RichText::new(
                        "Trusted LAN only: the stats connection is not encrypted. Never expose this port to the internet.",
                    )
                    .color(theme::colors::GOLD)
                    .size(11.0),
                );
            });

            if restart {
                let token_ready = if self.config.share_token.trim().is_empty() {
                    match try_generate_token() {
                        Ok(token) => {
                            self.config.share_token = token;
                            true
                        }
                        Err(error) => {
                            self.server_error = error;
                            false
                        }
                    }
                } else {
                    true
                };
                if token_ready {
                    match self.save() {
                        Ok(()) => self.sync_server(),
                        Err(error) => {
                            self.stop_server_only();
                            self.server_error = error;
                        }
                    }
                } else {
                    self.stop_server_only();
                }
            }
            if !self.server_error.is_empty() {
                ui.label(
                    egui::RichText::new(&self.server_error)
                        .color(theme::colors::RED)
                        .size(11.5),
                );
            } else if self.config.share_enabled {
                ui.label(
                    egui::RichText::new("Read-only LAN endpoint is active")
                        .color(theme::colors::ACCENT)
                        .strong()
                        .size(11.5),
                );
            }
        });
    }

    pub fn show_dashboard(&mut self, ui: &mut egui::Ui, local: &MiningStatsSnapshot) {
        let online: Vec<&PeerResult> = self
            .results
            .iter()
            .filter(|result| result.stats.is_some())
            .collect();
        let mut total_hashrate = local.hashrate_hps;
        let mut total_watts = local.watts;
        let mut total_cost = local.daily_cost_eur;
        for result in &online {
            if let Some(stats) = &result.stats {
                total_hashrate += stats.hashrate_hps;
                total_watts += stats.watts;
                total_cost += stats.daily_cost_eur;
            }
        }
        let total_hashrate_display = format_hashrate(total_hashrate);
        let online_display = format!("{} / {}", online.len() + 1, self.config.peers.len() + 1);

        theme::section_card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("MINER FLEET • MULTI-MINER")
                            .strong()
                            .size(12.0)
                            .color(theme::colors::ACCENT),
                    );
                    ui.label(
                        egui::RichText::new("Local miner plus all reachable LAN miners")
                            .color(theme::colors::TEXT_MUTED)
                            .size(11.5),
                    );
                });
                if self.poll_running_generation.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spinner();
                    });
                }
            });
            ui.add_space(10.0);
            egui::Grid::new("fleet_totals")
                .num_columns(4)
                .spacing([12.0, 10.0])
                .min_col_width(180.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        "Total hashrate",
                        &total_hashrate_display,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::GOLD,
                        "Online panels",
                        &online_display,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        "Total power",
                        &format!("{total_watts:.0} W"),
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::GOLD,
                        "Fleet cost / day",
                        &format!("€{total_cost:.2}"),
                    );
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.collapsing("Manage miners", |ui| {
                ui.label(
                    egui::RichText::new(
                        "On every remote panel enable LAN sharing, then enter its address and token here.",
                    )
                    .color(theme::colors::TEXT_MUTED)
                    .size(11.0),
                );
                ui.add_space(6.0);
                egui::Grid::new("fleet_add_grid")
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .show(ui, |ui| {
                        theme::field_label(ui, "Miner name:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.name_input)
                                .hint_text("Rig 2")
                                .desired_width(300.0),
                        );
                        ui.end_row();
                        theme::field_label(ui, "LAN address:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.address_input)
                                .hint_text("192.168.1.42:19120")
                                .desired_width(300.0),
                        );
                        ui.end_row();
                        theme::field_label(ui, "Access token:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token_input)
                                .password(true)
                                .desired_width(300.0),
                        );
                        ui.end_row();
                    });
                if theme::btn_secondary(ui, "Add miner").clicked() {
                    self.add_peer();
                }
                if !self.add_error.is_empty() {
                    ui.label(
                        egui::RichText::new(&self.add_error)
                            .color(theme::colors::RED)
                            .size(11.5),
                    );
                }

                let mut remove = None;
                for (idx, peer) in self.config.peers.iter().enumerate() {
                    let result = self.results.iter().find(|r| r.peer.address == peer.address);
                    let (status, color) = match result {
                        Some(r) if r.stats.is_some() => ("Online".to_string(), theme::colors::ACCENT),
                        Some(r) if !r.error.is_empty() => (r.error.clone(), theme::colors::RED),
                        _ => ("Waiting…".to_string(), theme::colors::TEXT_MUTED),
                    };
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} • {}", peer.name, peer.address))
                                .color(theme::colors::TEXT)
                                .strong(),
                        );
                        ui.label(egui::RichText::new(status).color(color).size(11.5));
                        if ui.small_button("Remove").clicked() {
                            remove = Some(idx);
                        }
                    });
                }
                if let Some(idx) = remove {
                    let removed = self.config.peers.remove(idx);
                    match self.save() {
                        Ok(()) => {
                            self.results.retain(|result| {
                                self.config
                                    .peers
                                    .iter()
                                    .any(|peer| peer.address == result.peer.address)
                            });
                            self.invalidate_peer_set();
                        }
                        Err(error) => {
                            self.config.peers.insert(idx, removed);
                            self.add_error = error;
                        }
                    }
                }
            });
        });
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
        || stats.oom_work_groups > 100_000_000
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
