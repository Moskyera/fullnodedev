mod assets;
mod config;
mod connect;
mod currency;
mod fonts;
mod hacash_config;
mod help_options;
mod i18n;
mod mining_kind;
mod presets;
mod theme;

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use app::efficiency::{EfficiencyMode, MiningStatsSnapshot};
use config::{
    apply_loaded_ini, load_panel_ini, write_diaworker_config, write_poworker_benchmark_config,
    write_poworker_config, PanelSettings,
};
use presets::tuning_for_profile;
use connect::{pool_presets, ConnectMode, SOLO_DEFAULT};
use eframe::egui;
use hacash_config::{
    find_hacash_config, read_diamond_miner, read_reward_wallet, validate_wallet,
    write_diamond_miner, write_hac_miner_only, DiamondMinerSettings,
};
use currency::{load_currency, save_currency, Currency};
use i18n::{load_lang, save_lang, strings, Lang, Strings};
use mining_kind::{load_mining_kind, save_mining_kind, MiningKind};
use presets::{cpu_presets, gpu_presets, CpuPreset, GpuPreset};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 700.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HAC Miner Panel",
        options,
        Box::new(|cc| Ok(Box::new(MinerApp::new(cc)))),
    )
}

#[derive(Clone)]
struct PendingStart {
    worker_path: PathBuf,
    deadline: Instant,
}

struct MinerApp {
    work_dir: PathBuf,
    config_path: PathBuf,
    dia_config_path: PathBuf,
    hacash_config_path: PathBuf,
    stats_path: PathBuf,
    poworker_path: PathBuf,
    diaworker_path: PathBuf,
    wallet: String,
    mining_kind: MiningKind,
    bid_password: String,
    bid_min: String,
    bid_max: String,
    bid_step: String,
    cpu_presets: Vec<CpuPreset>,
    gpu_presets: Vec<GpuPreset>,
    lang: Lang,
    currency: Currency,
    cpu_idx: usize,
    gpu_idx: usize,
    mode_idx: usize,
    power_cost: f32,
    hac_price: f32,
    platform_id: u32,
    device_id: u32,
    connect: String,
    connect_mode: ConnectMode,
    pool_preset_idx: usize,
    max_temp_c: u32,
    pause_unprofitable: bool,
    work_groups: u32,
    unit_size: u32,
    benchmarking: bool,
    benchmark_child: Option<Child>,
    pending_start: Option<PendingStart>,
    mining: bool,
    child: Option<Child>,
    stats: MiningStatsSnapshot,
    status_msg: String,
    log_rx: Option<Receiver<String>>,
    tab: usize,
    logo_texture: Option<egui::TextureHandle>,
}

impl MinerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        fonts::setup_fonts(&cc.egui_ctx);
        theme::setup_theme(&cc.egui_ctx);
        let work_dir = exe_dir();
        let logo_texture = assets::load_logo(&cc.egui_ctx, &work_dir);
        let config_path = work_dir.join("poworker.config.ini");
        let dia_config_path = work_dir.join("diaworker.config.ini");
        let hacash_config_path = find_hacash_config(&work_dir);
        let mining_kind = load_mining_kind(&work_dir);
        let diamond = read_diamond_miner(&hacash_config_path);
        let wallet = match mining_kind {
            MiningKind::Hacd if !diamond.reward.is_empty() => diamond.reward,
            _ => read_reward_wallet(&hacash_config_path),
        };
        let stats_path = work_dir.join("miner-stats.json");
        let poworker_path = find_worker_exe(&work_dir, "poworker.exe");
        let diaworker_path = find_worker_exe(&work_dir, "diaworker.exe");
        let cpus = cpu_presets();
        let gpus = gpu_presets();
        let lang = load_lang(&work_dir);
        let currency = load_currency(&work_dir, lang);
        let t = strings(lang);
        let mut cpu_idx = 2usize;
        let mut gpu_idx = 5usize;
        let mut mode_idx = 1usize;
        let mut power_cost = currency.default_power_cost();
        let mut hac_price = 0.0f32;
        let mut platform_id = 0u32;
        let mut device_id = 0u32;
        let mut connect = SOLO_DEFAULT.to_string();
        let mut max_temp_c = 83u32;
        let mut pause_unprofitable = false;
        let (mut work_groups, mut unit_size) =
            tuning_for_profile(&gpus[gpu_idx].profile);
        let ini_path = work_dir.join("poworker.config.ini");
        apply_loaded_ini(
            &load_panel_ini(&ini_path),
            &cpus,
            &gpus,
            &mut cpu_idx,
            &mut gpu_idx,
            &mut mode_idx,
            &mut work_groups,
            &mut unit_size,
            &mut platform_id,
            &mut device_id,
            &mut connect,
            &mut power_cost,
            &mut hac_price,
            &mut max_temp_c,
            &mut pause_unprofitable,
        );
        Self {
            work_dir,
            config_path,
            dia_config_path,
            hacash_config_path,
            stats_path,
            poworker_path,
            diaworker_path,
            wallet,
            mining_kind,
            bid_password: diamond.bid_password,
            bid_min: diamond.bid_min,
            bid_max: diamond.bid_max,
            bid_step: diamond.bid_step,
            cpu_presets: cpus,
            gpu_presets: gpus,
            lang,
            currency,
            cpu_idx,
            gpu_idx,
            mode_idx,
            power_cost,
            hac_price,
            platform_id,
            device_id,
            connect,
            connect_mode: ConnectMode::Solo,
            pool_preset_idx: 0,
            max_temp_c,
            pause_unprofitable,
            work_groups,
            unit_size,
            benchmarking: false,
            benchmark_child: None,
            pending_start: None,
            mining: false,
            child: None,
            stats: MiningStatsSnapshot::default(),
            status_msg: t.ready_status.to_string(),
            log_rx: None,
            tab: 0,
            logo_texture,
        }
    }

    fn miner_badge_state(&self) -> theme::MinerBadgeState {
        theme::miner_badge_state(self.mining, self.stats.paused_unprofitable)
    }

    fn miner_status_label(&self) -> &str {
        let t = self.t();
        match self.miner_badge_state() {
            theme::MinerBadgeState::Mining => t.mining_status,
            theme::MinerBadgeState::Paused => t.paused_unprofitable,
            theme::MinerBadgeState::Stopped => t.stopped_status,
        }
    }

    fn apply_gpu_preset_tuning(&mut self) {
        if self.gpu_presets[self.gpu_idx].slug == "none" {
            return;
        }
        let (wg, us) = tuning_for_profile(&self.gpu_presets[self.gpu_idx].profile);
        self.work_groups = wg;
        self.unit_size = us;
    }

    fn t(&self) -> Strings {
        strings(self.lang)
    }

    fn cpu_label(&self, idx: usize) -> &str {
        if idx + 1 == self.cpu_presets.len() {
            self.t().cpu_only
        } else {
            self.cpu_presets[idx].label
        }
    }

    fn gpu_label(&self, idx: usize) -> &str {
        if idx + 1 == self.gpu_presets.len() {
            self.t().no_gpu
        } else {
            self.gpu_presets[idx].label
        }
    }

    fn mode_label(&self, idx: usize) -> &str {
        let t = self.t();
        match idx {
            0 => t.mode_eco,
            2 => t.mode_max,
            _ => t.mode_profit,
        }
    }

    fn convert_money(&mut self, from: Currency, to: Currency) {
        if from == to {
            return;
        }
        self.power_cost =
            Currency::convert(self.power_cost as f64, from, to) as f32;
        let (min, max) = to.power_cost_range();
        self.power_cost = self.power_cost.clamp(min, max);
    }

    fn set_currency(&mut self, currency: Currency) {
        if self.currency == currency {
            return;
        }
        let from = self.currency;
        self.convert_money(from, currency);
        self.currency = currency;
        save_currency(&self.work_dir, currency);
    }

    fn set_lang(&mut self, lang: Lang) {
        if self.lang == lang {
            return;
        }
        let new_currency = Currency::default_for_lang(lang);
        self.convert_money(self.currency, new_currency);
        self.lang = lang;
        self.currency = new_currency;
        save_lang(&self.work_dir, lang);
        save_currency(&self.work_dir, new_currency);
        let t = strings(lang);
        if self.mining {
            self.status_msg = t.mining_active.to_string();
        } else {
            self.status_msg = t.ready_status.to_string();
        }
    }

    fn panel_settings(&self) -> PanelSettings {
        PanelSettings {
            cpu: self.cpu_presets[self.cpu_idx].clone(),
            gpu: self.gpu_presets[self.gpu_idx].clone(),
            mode: match self.mode_idx {
                0 => EfficiencyMode::Eco,
                2 => EfficiencyMode::Max,
                _ => EfficiencyMode::Profit,
            },
            power_cost_kwh: self.power_cost as f64,
            hac_price: self.hac_price as f64,
            platform_id: self.platform_id,
            device_id: self.device_id,
            connect: self.connect.clone(),
            stats_file: self.stats_path.to_string_lossy().to_string(),
            opencl_dir: opencl_dir_for(&self.work_dir),
            max_temp_c: self.max_temp_c,
            pause_if_unprofitable: self.pause_unprofitable,
            benchmark_seconds: 0,
            idle_start_hour: 255,
            idle_end_hour: 255,
            benchmark_fine_sweep: true,
            thermal_gpu_index: self.device_id,
            work_groups: self.work_groups,
            unit_size: self.unit_size,
        }
    }

    fn set_connect_mode(&mut self, mode: ConnectMode) {
        self.connect_mode = mode;
        if mode == ConnectMode::Solo {
            self.connect = SOLO_DEFAULT.to_string();
        }
    }

    fn apply_pool_preset(&mut self, idx: usize) {
        self.pool_preset_idx = idx;
        let pools = pool_presets();
        if let Some(p) = pools.get(idx) {
            if !p.host.is_empty() {
                self.connect = p.host.to_string();
            }
        }
    }

    fn run_benchmark(&mut self) {
        let t = self.t();
        if self.mining || self.benchmarking {
            return;
        }
        if self.gpu_presets[self.gpu_idx].slug == "none" {
            self.status_msg = t.no_gpu.to_string();
            return;
        }
        if !self.poworker_path.exists() {
            self.status_msg = format!("{}\n{}", t.poworker_not_found, self.poworker_path.display());
            return;
        }
        let s = self.panel_settings();
        if write_poworker_benchmark_config(&self.config_path, &s, 90).is_err() {
            self.status_msg = t.save_error_prefix.to_string();
            return;
        }
        let mut cmd = Command::new(&self.poworker_path);
        cmd.current_dir(&self.work_dir);
        match Self::spawn_worker_with_logs(&mut cmd) {
            Ok((child, _rx)) => {
                self.benchmark_child = Some(child);
                self.benchmarking = true;
                self.status_msg = t.benchmark_running.to_string();
            }
            Err(e) => self.status_msg = format!("{} {e}", t.start_failed_prefix),
        }
    }

    fn poll_benchmark(&mut self) {
        if !self.benchmarking {
            return;
        }
        let t = self.t();
        if let Some(child) = &mut self.benchmark_child {
            if let Ok(Some(_)) = child.try_wait() {
                self.benchmark_child = None;
                self.benchmarking = false;
                apply_loaded_ini(
                    &load_panel_ini(&self.config_path),
                    &self.cpu_presets,
                    &self.gpu_presets,
                    &mut self.cpu_idx,
                    &mut self.gpu_idx,
                    &mut self.mode_idx,
                    &mut self.work_groups,
                    &mut self.unit_size,
                    &mut self.platform_id,
                    &mut self.device_id,
                    &mut self.connect,
                    &mut self.power_cost,
                    &mut self.hac_price,
                    &mut self.max_temp_c,
                    &mut self.pause_unprofitable,
                );
                self.status_msg = t.benchmark_done.to_string();
            }
        }
    }

    fn diamond_settings(&self) -> DiamondMinerSettings {
        DiamondMinerSettings {
            reward: self.wallet.clone(),
            bid_password: self.bid_password.clone(),
            bid_min: self.bid_min.clone(),
            bid_max: self.bid_max.clone(),
            bid_step: self.bid_step.clone(),
        }
    }

    fn save_config(&mut self) -> bool {
        let t = self.t();
        if validate_wallet(&self.wallet).is_err() {
            self.status_msg = t.wallet_required.to_string();
            return false;
        }
        if self.mining_kind == MiningKind::Hacd && self.bid_password.trim().is_empty() {
            self.status_msg = t.bid_password_required.to_string();
            return false;
        }
        let s = self.panel_settings();
        save_mining_kind(&self.work_dir, self.mining_kind);
        match self.mining_kind {
            MiningKind::Hac => {
                if let Err(e) = write_poworker_config(&self.config_path, &s) {
                    self.status_msg = format!("{} {e}", t.save_error_prefix);
                    return false;
                }
                if let Err(e) = write_hac_miner_only(&self.hacash_config_path, &self.wallet) {
                    self.status_msg = format!("{} {e}", t.save_error_prefix);
                    return false;
                }
                self.status_msg = format!(
                    "{} {} + {}",
                    t.saved_prefix,
                    self.config_path.display(),
                    self.hacash_config_path.display()
                );
            }
            MiningKind::Hacd => {
                if let Err(e) = write_diaworker_config(&self.dia_config_path, &s) {
                    self.status_msg = format!("{} {e}", t.save_error_prefix);
                    return false;
                }
                if let Err(e) =
                    write_diamond_miner(&self.hacash_config_path, &self.wallet, &self.diamond_settings())
                {
                    self.status_msg = format!("{} {e}", t.save_error_prefix);
                    return false;
                }
                self.status_msg = format!(
                    "{} {} + {}",
                    t.saved_prefix,
                    self.dia_config_path.display(),
                    self.hacash_config_path.display()
                );
            }
        }
        true
    }

    fn spawn_worker_with_logs(cmd: &mut Command) -> Result<(Child, Receiver<String>), String> {
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

    fn start_mining(&mut self) {
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
        if self.connect_mode == ConnectMode::Solo && !rpc_reachable(&self.connect) {
            let hacash = find_hacash_exe(&self.work_dir);
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
                deadline: Instant::now() + Duration::from_secs(45),
            });
            return;
        }
        self.launch_worker(worker_path);
    }

    fn launch_worker(&mut self, worker_path: PathBuf) {
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

    fn poll_pending_start(&mut self) {
        let Some(pending) = self.pending_start.clone() else {
            return;
        };
        let t = self.t();
        if rpc_reachable(&self.connect) {
            self.launch_worker(pending.worker_path);
            return;
        }
        if Instant::now() >= pending.deadline {
            self.pending_start = None;
            self.status_msg = format!("{} {}", t.fullnode_not_ready, self.connect);
        }
    }

    fn stop_mining(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.mining = false;
        self.log_rx = None;
        self.status_msg = self.t().mining_stopped.to_string();
    }

    fn poll_stats(&mut self) {
        self.poll_pending_start();
        self.poll_benchmark();
        let t = self.t();
        if let Ok(data) = std::fs::read_to_string(&self.stats_path) {
            if let Ok(s) = serde_json::from_str::<MiningStatsSnapshot>(&data) {
                self.stats = s;
            }
        }
        if !self.mining {
            return;
        }
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
                if line.contains("MINING SUCCESS") {
                    self.status_msg = t.block_found.to_string();
                } else if line.contains("cannot get block data") {
                    self.status_msg = t.fullnode_not_ready.to_string();
                } else if line.contains("OpenCL error")
                    || line.contains("GPU batch failed")
                    || line.contains("CL_OUT_OF")
                {
                    self.status_msg = format!("{} {line}", t.worker_error_prefix);
                }
            }
        }
        if let Some(child) = &mut self.child {
            if let Ok(Some(_)) = child.try_wait() {
                self.mining = false;
                self.child = None;
                self.status_msg = t.miner_exited.to_string();
            }
        }
    }
}

impl eframe::App for MinerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_stats();
        ctx.request_repaint_after(Duration::from_millis(500));

        let t = self.t();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.window_title.to_string()));

        egui::TopBottomPanel::top("header")
            .frame(theme::header_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(tex) = self.logo_texture.as_ref() {
                        theme::show_logo(ui, tex, 44.0);
                    } else {
                        theme::logo_fallback(ui);
                    }
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("HAC Miner Panel")
                                .size(20.0)
                                .strong()
                                .color(theme::colors::TEXT),
                        );
                        ui.label(
                            egui::RichText::new(t.header_subtitle)
                                .size(12.5)
                                .color(theme::colors::GOLD_DIM),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("lang")
                            .selected_text(self.lang.name())
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                for lang in Lang::ALL {
                                    if ui
                                        .selectable_label(self.lang == lang, lang.name())
                                        .clicked()
                                    {
                                        self.set_lang(lang);
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(t.label_language)
                                .color(theme::colors::TEXT_MUTED)
                                .size(13.0),
                        );
                        ui.add_space(14.0);
                        egui::ComboBox::from_id_salt("currency")
                            .selected_text(self.currency.name())
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                for c in Currency::ALL {
                                    if ui
                                        .selectable_label(self.currency == c, c.name())
                                        .clicked()
                                    {
                                        self.set_currency(c);
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(t.label_currency)
                                .color(theme::colors::TEXT_MUTED)
                                .size(13.0),
                        );
                    });
                });
            });

        egui::TopBottomPanel::bottom("footer")
            .frame(theme::footer_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let badge = self.miner_badge_state();
                    let badge_label = self.miner_status_label();
                    theme::footer_status_chip(ui, badge, badge_label);
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(&self.status_msg)
                            .color(theme::colors::TEXT_MUTED)
                            .size(13.0),
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(theme::content_frame())
            .show(ctx, |ui| {
                theme::tab_bar(ui, |ui| {
                    if theme::tab_pill(ui, self.tab == 0, theme::TabIcon::Settings, t.tab_settings) {
                        self.tab = 0;
                    }
                    if theme::tab_pill(ui, self.tab == 1, theme::TabIcon::Dashboard, t.tab_dashboard) {
                        self.tab = 1;
                    }
                    if theme::tab_pill(ui, self.tab == 2, theme::TabIcon::Help, t.tab_help) {
                        self.tab = 2;
                    }
                });
                ui.add_space(16.0);

                match self.tab {
                    0 | 2 => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.tab == 0 {
                                    self.ui_settings(ui);
                                } else {
                                    self.ui_help(ui);
                                }
                            });
                    }
                    _ => self.ui_dashboard(ui),
                }
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_mining();
    }
}

impl MinerApp {
    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.label(
            egui::RichText::new(t.settings_intro)
                .color(theme::colors::TEXT_MUTED)
                .size(14.0),
        );
        ui.add_space(14.0);

        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.label_mining_type)
                    .strong()
                    .color(theme::colors::TEXT),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.mining_kind == MiningKind::Hac, t.mining_hac)
                    .clicked()
                {
                    self.mining_kind = MiningKind::Hac;
                    save_mining_kind(&self.work_dir, MiningKind::Hac);
                }
                ui.add_space(12.0);
                if ui
                    .selectable_label(self.mining_kind == MiningKind::Hacd, t.mining_hacd)
                    .clicked()
                {
                    self.mining_kind = MiningKind::Hacd;
                    save_mining_kind(&self.work_dir, MiningKind::Hacd);
                }
            });
        });

        ui.add_space(12.0);

        let cpu_idx = self.cpu_idx;
        let gpu_idx = self.gpu_idx;
        let mode_idx = self.mode_idx;

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("hw_grid")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
                    theme::field_label(ui, t.label_cpu);
                    egui::ComboBox::from_id_salt("cpu")
                        .selected_text(self.cpu_label(cpu_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            for (i, _) in self.cpu_presets.iter().enumerate() {
                                let label = if i + 1 == self.cpu_presets.len() {
                                    t.cpu_only
                                } else {
                                    self.cpu_presets[i].label
                                };
                                ui.selectable_value(&mut self.cpu_idx, i, label);
                            }
                        });
                    ui.end_row();

                    theme::field_label(ui, t.label_gpu);
                    let gpu_before = gpu_idx;
                    egui::ComboBox::from_id_salt("gpu")
                        .selected_text(self.gpu_label(gpu_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            for (i, _) in self.gpu_presets.iter().enumerate() {
                                let label = if i + 1 == self.gpu_presets.len() {
                                    t.no_gpu
                                } else {
                                    self.gpu_presets[i].label
                                };
                                ui.selectable_value(&mut self.gpu_idx, i, label);
                            }
                        });
                    if self.gpu_idx != gpu_before {
                        self.apply_gpu_preset_tuning();
                    }
                    ui.end_row();

                    theme::field_label(ui, t.label_mode);
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(self.mode_label(mode_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.mode_idx, 0, t.mode_eco);
                            ui.selectable_value(&mut self.mode_idx, 1, t.mode_profit);
                            ui.selectable_value(&mut self.mode_idx, 2, t.mode_max);
                        });
                    ui.end_row();
                });
        });

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("eco_grid")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
                    theme::field_label(ui, t.label_power_cost);
                    theme::power_cost_slider(ui, &mut self.power_cost, self.currency);
                    ui.end_row();

                    theme::field_label(ui, t.label_hac_price);
                    ui.add(
                        egui::DragValue::new(&mut self.hac_price)
                            .speed(0.01)
                            .range(0.0..=1_000_000.0)
                            .suffix(" $"),
                    );
                    ui.end_row();

                    theme::field_label(ui, t.label_max_temp);
                    ui.add(
                        egui::DragValue::new(&mut self.max_temp_c)
                            .range(0..=100)
                            .suffix(" °C"),
                    );
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut self.pause_unprofitable, t.label_pause_unprofitable);
                    ui.end_row();
                });
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if theme::btn_secondary(ui, t.btn_benchmark).clicked() {
                self.run_benchmark();
            }
            if self.benchmarking {
                ui.spinner();
                ui.label(
                    egui::RichText::new(t.benchmark_running)
                        .color(theme::colors::GOLD)
                        .size(12.5),
                );
            }
        });

        if self.mining_kind == MiningKind::Hacd {
            ui.add_space(12.0);
            theme::section_card().show(ui, |ui| {
                ui.label(
                    egui::RichText::new(t.bid_hint)
                        .size(12.0)
                        .color(theme::colors::TEXT_MUTED),
                );
                ui.add_space(10.0);
                egui::Grid::new("bid_grid")
                    .num_columns(2)
                    .spacing([20.0, 12.0])
                    .show(ui, |ui| {
                        theme::field_label(ui, t.label_bid_password);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.bid_password)
                                .password(true)
                                .desired_width(400.0),
                        );
                        ui.end_row();

                        theme::field_label(ui, t.label_bid_min);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.bid_min)
                                .desired_width(160.0)
                                .hint_text("1:0"),
                        );
                        ui.end_row();

                        theme::field_label(ui, t.label_bid_max);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.bid_max)
                                .desired_width(160.0)
                                .hint_text("31:0"),
                        );
                        ui.end_row();

                        theme::field_label(ui, t.label_bid_step);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.bid_step)
                                .desired_width(160.0)
                                .hint_text("0:5"),
                        );
                        ui.end_row();
                    });
            });
        }

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("net_grid")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
                    theme::field_label(ui, t.label_connect_mode);
                    ui.horizontal(|ui| {
                        let solo = self.connect_mode == ConnectMode::Solo;
                        if ui.selectable_label(solo, t.connect_solo).clicked() {
                            self.set_connect_mode(ConnectMode::Solo);
                        }
                        if ui
                            .selectable_label(!solo, t.connect_pool)
                            .clicked()
                        {
                            self.set_connect_mode(ConnectMode::Pool);
                        }
                    });
                    ui.end_row();

                    theme::field_label(
                        ui,
                        if self.connect_mode == ConnectMode::Solo {
                            t.label_fullnode
                        } else {
                            t.connect_pool
                        },
                    );
                    ui.vertical(|ui| {
                        if self.connect_mode == ConnectMode::Pool {
                            let pools = pool_presets();
                            let preset_label = pools
                                .get(self.pool_preset_idx)
                                .map(|p| p.label)
                                .unwrap_or("Pool");
                            egui::ComboBox::from_id_salt("pool_preset")
                                .selected_text(preset_label)
                                .show_ui(ui, |ui| {
                                    for (i, p) in pools.iter().enumerate() {
                                        if ui
                                            .selectable_value(
                                                &mut self.pool_preset_idx,
                                                i,
                                                p.label,
                                            )
                                            .clicked()
                                        {
                                            self.apply_pool_preset(i);
                                        }
                                    }
                                });
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.connect)
                                .desired_width(400.0)
                                .margin(egui::Margin::symmetric(8.0, 6.0)),
                        );
                        if self.connect_mode == ConnectMode::Pool {
                            ui.label(
                                egui::RichText::new(t.connect_pool_hint)
                                    .size(11.5)
                                    .color(theme::colors::TEXT_MUTED),
                            );
                        }
                    });
                    ui.end_row();

                    theme::field_label(ui, t.label_wallet);
                    ui.vertical(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.wallet)
                                .desired_width(400.0)
                                .hint_text("1LCY6uQS3iNGy2mKSmhFVU2dHgBQLf74Fx")
                                .margin(egui::Margin::symmetric(8.0, 6.0)),
                        );
                        ui.label(
                            egui::RichText::new(if self.mining_kind == MiningKind::Hacd {
                                t.hacd_wallet_hint
                            } else {
                                t.wallet_hint
                            })
                            .size(11.5)
                            .color(theme::colors::TEXT_MUTED),
                        );
                    });
                    ui.end_row();

                    theme::field_label(ui, t.label_opencl);
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.platform_id).range(0..=8));
                        ui.label(
                            egui::RichText::new(t.platform)
                                .color(theme::colors::TEXT_MUTED)
                                .size(12.0),
                        );
                        ui.add_space(8.0);
                        ui.add(egui::DragValue::new(&mut self.device_id).range(0..=8));
                        ui.label(
                            egui::RichText::new(t.device_hint)
                                .color(theme::colors::TEXT_MUTED)
                                .size(12.0),
                        );
                    });
                    ui.end_row();
                });
        });

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if theme::btn_secondary(ui, t.btn_save).clicked() {
                self.save_config();
            }
            ui.add_space(8.0);
            if theme::btn_primary(ui, t.btn_start_mining).clicked() {
                self.start_mining();
                self.tab = 1;
            }
            if self.mining {
                ui.add_space(8.0);
                if theme::btn_danger(ui, t.btn_stop).clicked() {
                    self.stop_mining();
                }
            }
        });
        ui.add_space(8.0);
    }

    fn truncate_wallet(wallet: &str) -> String {
        let w = wallet.trim();
        if w.is_empty() {
            return String::new();
        }
        if w.len() <= 20 {
            return w.to_string();
        }
        format!("{}…{}", &w[..8], &w[w.len() - 6..])
    }

    fn format_stats_age(ms: u64) -> String {
        if ms == 0 {
            return "—".to_string();
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let sec = now_ms.saturating_sub(ms) / 1000;
        if sec < 60 {
            format!("{sec}s")
        } else if sec < 3600 {
            format!("{}m", sec / 60)
        } else {
            format!("{}h", sec / 3600)
        }
    }

    fn ui_dashboard(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let s = &self.stats;
        let cpu_threads = if s.active_cpu_threads > 0 {
            format!(
                "{} / {}",
                s.active_cpu_threads,
                self.cpu_presets[self.cpu_idx].supervene
            )
        } else {
            format!("{}", self.cpu_presets[self.cpu_idx].supervene)
        };
        let revenue_day = if s.daily_revenue_eur > 0.0 {
            self.currency.format_amount(s.daily_revenue_eur)
        } else {
            t.dash_no_data.to_string()
        };
        let mining_type = if self.mining_kind == MiningKind::Hacd {
            t.mining_hacd
        } else {
            t.mining_hac
        };
        let connect_mode = match self.connect_mode {
            ConnectMode::Solo => t.connect_solo,
            ConnectMode::Pool => t.connect_pool,
        };
        let connect_display = format!("{} · {}", self.connect, connect_mode);
        let wallet_display = if self.wallet.trim().is_empty() {
            t.dash_no_data.to_string()
        } else {
            Self::truncate_wallet(&self.wallet)
        };
        let opencl_display = if self.gpu_presets[self.gpu_idx].slug == "none" {
            t.dash_no_data.to_string()
        } else {
            format!("P{} / D{}", self.platform_id, self.device_id)
        };
        let tuning_display = if self.gpu_presets[self.gpu_idx].slug == "none" {
            t.dash_no_data.to_string()
        } else {
            format!("WG {} × US {}", self.work_groups, self.unit_size)
        };
        let stats_status = if !s.status.is_empty() {
            s.status.clone()
        } else if self.mining {
            "mining".to_string()
        } else {
            "stopped".to_string()
        };
        let last_update = Self::format_stats_age(s.updated_unix_ms);
        let power_cost_display = format!(
            "{}/kWh",
            self.currency.format_amount(self.power_cost as f64)
        );

        let badge = self.miner_badge_state();
        let badge_label = self.miner_status_label();
        theme::section_card().show(ui, |ui| {
            theme::status_badge(ui, badge, badge_label);
        });

        ui.add_space(14.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("dash_row1")
                .num_columns(4)
                .spacing([12.0, 12.0])
                .min_col_width(200.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        t.stat_hashrate,
                        &s.hashrate_display,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::GREEN,
                        t.stat_hac_day,
                        &format!("{:.4}", s.hac_per_day),
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::BLUE,
                        t.stat_power,
                        &format!("{:.0} W", s.watts),
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::RED,
                        t.stat_cost_day,
                        &self.currency.format_amount(s.daily_cost_eur),
                    );
                    ui.end_row();
                });
        });

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("dash_row2")
                .num_columns(4)
                .spacing([12.0, 12.0])
                .min_col_width(200.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::BLUE,
                        t.stat_efficiency,
                        &format!("{:.1} kH/J", s.kh_per_j),
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        t.stat_network,
                        &format!("{:.4}%", s.network_pct),
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::GREEN,
                        t.stat_block_height,
                        &format!("{}", s.height),
                    );
                    if s.daily_revenue_eur > 0.0 {
                        theme::show_stat_card(
                            ui,
                            theme::colors::GREEN,
                            t.stat_net_day,
                            &self.currency.format_amount(s.daily_net_eur),
                        );
                    } else if s.mining_kind == "hacd" && !s.diamond_best.is_empty() {
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_diamond_best,
                            &s.diamond_best,
                        );
                    } else {
                        theme::show_stat_card(
                            ui,
                            theme::colors::TEXT_MUTED,
                            t.stat_gpu_profile,
                            &s.gpu_profile,
                        );
                    }
                    ui.end_row();
                });
        });

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("dash_row3")
                .num_columns(4)
                .spacing([12.0, 12.0])
                .min_col_width(200.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::GREEN,
                        t.stat_revenue_day,
                        &revenue_day,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        t.stat_cpu_threads,
                        &cpu_threads,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::BLUE,
                        t.stat_mining_type_short,
                        mining_type,
                    );
                    theme::show_stat_card(
                        ui,
                        theme::colors::GOLD,
                        t.stat_mode_short,
                        self.mode_label(self.mode_idx),
                    );
                    ui.end_row();
                });
        });

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.dash_details_title)
                    .size(14.0)
                    .strong()
                    .color(theme::colors::TEXT),
            );
            ui.add_space(10.0);
            egui::Grid::new("dash_details")
                .num_columns(2)
                .spacing([24.0, 6.0])
                .min_col_width(320.0)
                .show(ui, |ui| {
                    theme::show_detail_row(ui, t.dash_detail_cpu, self.cpu_label(self.cpu_idx));
                    theme::show_detail_row(ui, t.dash_detail_gpu, self.gpu_label(self.gpu_idx));
                    ui.end_row();
                    theme::show_detail_row(ui, t.dash_detail_connect, &connect_display);
                    theme::show_detail_row(ui, t.dash_detail_wallet, &wallet_display);
                    ui.end_row();
                    theme::show_detail_row(ui, t.dash_detail_opencl, &opencl_display);
                    theme::show_detail_row(ui, t.dash_detail_tuning, &tuning_display);
                    ui.end_row();
                    theme::show_detail_row(ui, t.dash_detail_power_cost, &power_cost_display);
                    theme::show_detail_row(
                        ui,
                        t.dash_detail_max_temp,
                        &format!("{}°C", self.max_temp_c),
                    );
                    ui.end_row();
                    theme::show_detail_row(ui, t.dash_detail_last_update, &last_update);
                    theme::show_detail_row(ui, t.dash_detail_stats_status, &stats_status);
                    ui.end_row();
                    let diamond_label = (self.mining_kind == MiningKind::Hacd && s.diamond_number > 0)
                        .then(|| (t.dash_detail_diamond, format!("#{}", s.diamond_number)));
                    let profile_label = (!s.gpu_profile.is_empty())
                        .then(|| (t.stat_gpu_profile, s.gpu_profile.clone()));
                    match (profile_label, diamond_label) {
                        (Some((pl, pv)), Some((dl, dv))) => {
                            theme::show_detail_row(ui, pl, &pv);
                            theme::show_detail_row(ui, dl, &dv);
                            ui.end_row();
                        }
                        (Some((pl, pv)), None) => {
                            theme::show_detail_row(ui, pl, &pv);
                            ui.end_row();
                        }
                        (None, Some((dl, dv))) => {
                            theme::show_detail_row(ui, dl, &dv);
                            ui.end_row();
                        }
                        (None, None) => {}
                    }
                });
        });

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if !self.mining && theme::btn_primary(ui, t.btn_start).clicked() {
                self.start_mining();
            }
            if self.mining && theme::btn_danger(ui, t.btn_stop_icon).clicked() {
                self.stop_mining();
            }
        });
    }

    fn ui_help(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.label(
            egui::RichText::new(t.help_title)
                .size(16.0)
                .strong()
                .color(theme::colors::TEXT),
        );
        ui.add_space(12.0);

        ui.label(
            egui::RichText::new(t.help_hac_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(8.0);
        theme::help_step(ui, 1, t.help_step1);
        theme::help_step(ui, 2, t.help_step2);
        theme::help_step(ui, 3, t.help_step3);

        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(t.help_hacd_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(8.0);
        theme::help_step(ui, 1, t.help_hacd_step1);
        theme::help_step(ui, 2, t.help_hacd_step2);
        theme::help_step(ui, 3, t.help_hacd_step3);
        theme::help_step(ui, 4, t.help_hacd_step4);
        theme::help_step(ui, 5, t.help_hacd_step5);

        ui.add_space(10.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.wallet_restart_hint)
                    .color(theme::colors::GOLD)
                    .size(13.0),
            );
        });

        ui.add_space(10.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.help_hardware_note)
                    .color(theme::colors::TEXT)
                    .size(13.0),
            );
        });

        ui.add_space(12.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    t.help_work_dir_prefix,
                    self.work_dir.display()
                ))
                .color(theme::colors::TEXT_MUTED)
                .size(12.5),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    t.help_miner_prefix,
                    if self.mining_kind == MiningKind::Hacd {
                        self.diaworker_path.display().to_string()
                    } else {
                        self.poworker_path.display().to_string()
                    }
                ))
                .color(theme::colors::TEXT_MUTED)
                .size(12.5),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(t.help_opencl_tip)
                    .color(theme::colors::TEXT_MUTED)
                    .size(12.5),
            );
        });

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(t.help_options_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(6.0);
        theme::section_card().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in help_options::option_reference(self.lang) {
                        ui.label(
                            egui::RichText::new(section.title)
                                .strong()
                                .color(theme::colors::TEXT)
                                .size(13.0),
                        );
                        ui.add_space(4.0);
                        for line in section.lines {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("•")
                                        .color(theme::colors::ACCENT)
                                        .size(12.0),
                                );
                                ui.label(
                                    egui::RichText::new(*line)
                                        .color(theme::colors::TEXT_MUTED)
                                        .size(12.0),
                                );
                            });
                        }
                        ui.add_space(10.0);
                    }
                });
        });
        ui.add_space(8.0);
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn spawn_log_drainer<R: Read + Send + 'static>(stream: R, tx: Sender<String>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
}

fn rpc_reachable(connect: &str) -> bool {
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

fn find_hacash_exe(work_dir: &Path) -> PathBuf {
    for name in ["hacash.exe", "fullnode.exe"] {
        let p = work_dir.join(name);
        if p.is_file() {
            return p;
        }
    }
    work_dir.join("hacash.exe")
}

fn find_worker_exe(work_dir: &PathBuf, name: &str) -> PathBuf {
    let candidates = [
        work_dir.join(name),
        work_dir.join("..").join(name),
        work_dir.join("..")
            .join("..")
            .join("target")
            .join("release")
            .join(name),
        work_dir.join("..")
            .join("..")
            .join("target")
            .join("debug")
            .join(name),
    ];
    for c in candidates {
        if c.exists() {
            return c.canonicalize().unwrap_or(c);
        }
    }
    work_dir.join(name)
}

fn opencl_dir_for(work_dir: &PathBuf) -> String {
    let candidates = [
        work_dir.join("x16rs").join("opencl"),
        work_dir.join("..").join("..").join("x16rs").join("opencl"),
    ];
    for c in candidates {
        if c.is_dir() {
            return format!("{}/", c.to_string_lossy().replace('\\', "/"));
        }
    }
    "../../x16rs/opencl/".to_string()
}