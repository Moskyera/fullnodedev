mod assets;
mod config;
mod connect;
mod currency;
mod dashboard;
mod fleet;
mod fonts;
mod hacash_config;
mod help_options;
mod i18n;
mod mining_control;
mod mining_kind;
mod opencl_status;
mod platform;
mod presets;
mod stats_poll;
mod theme;
mod ui_dashboard_tab;
mod ui_help_tab;
mod ui_settings;
mod ui_settings_tab;

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use app::efficiency::{EfficiencyMode, MiningStatsSnapshot};
use config::{
    BenchmarkConfigBackup, PanelSettings, apply_benchmark_ini, apply_loaded_ini,
    commit_benchmark_backup, create_benchmark_backup, interrupted_benchmark_backup, load_panel_ini,
    recover_interrupted_benchmark, restore_benchmark_backup, write_diaworker_config,
    write_poworker_benchmark_config, write_poworker_config,
};
use connect::{ConnectMode, SOLO_DEFAULT, connect_port, normalize_connect, pool_presets};
use currency::{Currency, load_currency, save_currency};
use eframe::egui;
use hacash_config::{
    DiamondMinerSettings, find_hacash_config, read_diamond_miner, read_reward_wallet,
    validate_diamond_settings, validate_hacd_wallet, validate_wallet, write_diamond_miner,
    write_hac_miner_only,
};
use i18n::{Lang, Strings, load_lang, save_lang, strings};
use mining_kind::{MiningKind, load_mining_kind, save_mining_kind};
use presets::resolve_panel_tuning;
use presets::{CpuPreset, GpuPreset, cpu_presets, gpu_idx_for_opencl, gpu_presets};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([900.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HAC Miner Panel",
        options,
        Box::new(|cc| Ok(Box::new(MinerApp::new(cc)))),
    )
}

#[derive(Clone, Copy)]
enum OpenClAction {
    InitialScan { preserve_selection: bool },
    AutoDetect,
    StartMining,
    AutoTune,
}

struct OpenClProbeResult {
    status: opencl_status::OpenClStatus,
    /// `None` when thermal protection is disabled or no usable GPU exists.
    thermal_available: Option<bool>,
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
    hac_wallet: String,
    hacd_wallet: String,
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
    gpu_profile: String,
    benchmarking: bool,
    benchmark_child: Option<Child>,
    benchmark_log_rx: Option<Receiver<String>>,
    benchmark_stop_rx: Option<Receiver<Result<(), String>>>,
    benchmark_stop_message: Option<String>,
    benchmark_config_backup: Option<BenchmarkConfigBackup>,
    benchmark_last_log: String,
    stats_next_read: Instant,
    pending_start: Option<mining_control::PendingStart>,
    mining: bool,
    child: Option<Child>,
    worker_stop_rx: Option<Receiver<Result<(), String>>>,
    worker_stop_failed: bool,
    restart_worker: Option<(PathBuf, Instant)>,
    restart_attempts: u8,
    worker_started_at: Option<Instant>,
    stats: MiningStatsSnapshot,
    status_msg: String,
    log_rx: Option<Receiver<String>>,
    fullnode_log_rx: Option<Receiver<String>>,
    last_worker_log: String,
    tab: usize,
    logo_texture: Option<egui::TextureHandle>,
    opencl_status: opencl_status::OpenClStatus,
    opencl_probe_rx: Option<Receiver<OpenClProbeResult>>,
    pending_opencl_action: Option<OpenClAction>,
    auto_select_detected_gpu: bool,
    fleet: fleet::FleetState,
}

impl MinerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        fonts::setup_fonts(&cc.egui_ctx);
        theme::setup_theme(&cc.egui_ctx);
        let work_dir = exe_dir();
        let logo_texture = assets::load_logo(&cc.egui_ctx, &work_dir);
        let config_path = work_dir.join("poworker.config.ini");
        let interrupted_benchmark_recovery = recover_interrupted_benchmark(&config_path);
        let pending_recovery_backup = if interrupted_benchmark_recovery.is_err() {
            interrupted_benchmark_backup(&config_path)
        } else {
            None
        };
        let dia_config_path = work_dir.join("diaworker.config.ini");
        let hacash_config_path = find_hacash_config(&work_dir);
        let mining_kind = load_mining_kind(&work_dir);
        let diamond = read_diamond_miner(&hacash_config_path);
        let hac_wallet = read_reward_wallet(&hacash_config_path);
        let hacd_wallet = diamond.reward.clone();
        let wallet = match mining_kind {
            MiningKind::Hacd => hacd_wallet.clone(),
            MiningKind::Hac => hac_wallet.clone(),
        };
        let stats_path = work_dir.join("miner-stats.json");
        let fleet = fleet::FleetState::load(&work_dir, &stats_path);
        let poworker_path = platform::find_worker(&work_dir, "poworker");
        let diaworker_path = platform::find_worker(&work_dir, "diaworker");
        let cpus = cpu_presets();
        let gpus = gpu_presets();
        let lang = load_lang(&work_dir);
        let currency = load_currency(&work_dir, lang);
        let t = strings(lang);
        let mut cpu_idx = 0usize;
        let mut gpu_idx = 6usize; // rx9070xt default
        let mut mode_idx = 1usize;
        let mut power_cost = currency.default_power_cost();
        let mut hac_price = 0.0f32;
        let mut platform_id = 0u32;
        let mut device_id = 0u32;
        let mut connect = SOLO_DEFAULT.to_string();
        let mut max_temp_c = 0u32;
        let mut pause_unprofitable = false;
        let ini_path = work_dir.join("poworker.config.ini");
        let loaded_ini = load_panel_ini(&ini_path);
        let opencl_configured_in_ini =
            loaded_ini.platform_id.is_some() || loaded_ini.device_id.is_some();
        apply_loaded_ini(
            &loaded_ini,
            &cpus,
            &gpus,
            &mut cpu_idx,
            &mut gpu_idx,
            &mut mode_idx,
            &mut platform_id,
            &mut device_id,
            &mut connect,
            &mut power_cost,
            &mut hac_price,
            &mut max_temp_c,
            &mut pause_unprofitable,
            currency,
        );
        if mining_kind == MiningKind::Hacd && cpus[cpu_idx].supervene == 0 {
            cpu_idx = cpus.iter().position(|p| p.supervene > 0).unwrap_or(cpu_idx);
        }
        let mode = match mode_idx {
            0 => EfficiencyMode::Eco,
            2 => EfficiencyMode::Max,
            _ => EfficiencyMode::Profit,
        };
        let mut work_groups = 0u32;
        let mut unit_size = 0u32;
        let mut gpu_profile = String::new();
        if loaded_ini.work_groups.is_some()
            || loaded_ini.unit_size.is_some()
            || loaded_ini.gpu_profile.is_some()
            || loaded_ini.gpu_slug.is_some()
        {
            apply_benchmark_ini(
                &loaded_ini,
                &gpus,
                &mut gpu_idx,
                &mut work_groups,
                &mut unit_size,
                &mut gpu_profile,
                mode,
            );
        } else {
            let tuning = resolve_panel_tuning(&gpus[gpu_idx], mode);
            work_groups = tuning.work_groups;
            unit_size = tuning.unit_size;
            gpu_profile = tuning.profile.to_string();
        }
        let opencl_status = opencl_status::load_cached_opencl_status(&work_dir);
        let gpu_configured_in_ini = loaded_ini.gpu_slug.is_some();
        if !gpu_configured_in_ini {
            if let Some(idx) = gpu_idx_for_opencl(
                &gpus,
                &opencl_status.recommended_name,
                &opencl_status.recommended_slug,
                opencl_status.recommended_vram_mb,
            ) {
                gpu_idx = idx;
                let tuning = resolve_panel_tuning(&gpus[gpu_idx], mode);
                work_groups = tuning.work_groups;
                unit_size = tuning.unit_size;
                gpu_profile = tuning.profile.to_string();
            }
        }
        let mut status_msg = if mining_kind == MiningKind::Hacd {
            "HACD CPU miner ready — OpenCL is not used.".to_string()
        } else if opencl_status.has_usable_device() {
            format!("OpenCL: {}", opencl_status.device_summary())
        } else {
            t.ready_status.to_string()
        };
        if mining_kind == MiningKind::Hac {
            if let Some(msg) = opencl_status::apply_recommended_opencl(
                &opencl_status,
                &mut platform_id,
                &mut device_id,
                opencl_configured_in_ini,
            ) {
                status_msg = msg;
            }
            if let Some(w) = opencl_status.warnings.first() {
                if status_msg == t.ready_status {
                    status_msg = format!("OpenCL: {}", w);
                }
            }
        }
        let connect_mode = ConnectMode::for_connect(&connect);
        let mut app = Self {
            work_dir,
            config_path,
            dia_config_path,
            hacash_config_path,
            stats_path,
            poworker_path,
            diaworker_path,
            wallet,
            hac_wallet,
            hacd_wallet,
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
            connect_mode,
            pool_preset_idx: 0,
            max_temp_c,
            pause_unprofitable,
            work_groups,
            unit_size,
            gpu_profile,
            benchmarking: false,
            benchmark_child: None,
            benchmark_log_rx: None,
            benchmark_stop_rx: None,
            benchmark_stop_message: None,
            benchmark_config_backup: pending_recovery_backup,
            benchmark_last_log: String::new(),
            stats_next_read: Instant::now(),
            pending_start: None,
            mining: false,
            child: None,
            worker_stop_rx: None,
            worker_stop_failed: false,
            restart_worker: None,
            restart_attempts: 0,
            worker_started_at: None,
            stats: MiningStatsSnapshot::default(),
            status_msg,
            log_rx: None,
            tab: 0,
            fullnode_log_rx: None,
            last_worker_log: String::new(),
            logo_texture,
            opencl_status,
            opencl_probe_rx: None,
            pending_opencl_action: None,
            auto_select_detected_gpu: !gpu_configured_in_ini,
            fleet,
        };
        if mining_kind == MiningKind::Hac {
            app.request_opencl_probe(OpenClAction::InitialScan {
                preserve_selection: opencl_configured_in_ini,
            });
        }
        match interrupted_benchmark_recovery {
            Ok(true) => {
                app.status_msg =
                    "Recovered mining settings after an interrupted Auto Tune. Checking OpenCL..."
                        .to_string();
            }
            Err(error) => {
                app.status_msg =
                    format!("Auto Tune recovery needs attention: {error}. The backup was kept.");
            }
            Ok(false) => {}
        }
        app
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

    fn opencl_probe_active(&self) -> bool {
        self.opencl_probe_rx.is_some()
    }

    fn benchmark_operation_active(&self) -> bool {
        self.benchmarking
            || self.benchmark_stop_rx.is_some()
            || self.benchmark_config_backup.is_some()
    }

    fn benchmark_stopping(&self) -> bool {
        self.benchmark_stop_rx.is_some()
    }

    fn worker_stopping(&self) -> bool {
        self.worker_stop_rx.is_some()
    }

    fn worker_stop_needs_restart(&self) -> bool {
        self.worker_stop_failed
    }

    fn worker_operation_active(&self) -> bool {
        self.mining
            || self.pending_start.is_some()
            || self.restart_worker.is_some()
            || self.worker_stopping()
            || self.worker_stop_needs_restart()
            || matches!(self.pending_opencl_action, Some(OpenClAction::StartMining))
    }

    fn mining_settings_locked(&self) -> bool {
        self.worker_operation_active()
            || self.benchmark_operation_active()
            || self.opencl_probe_active()
    }

    fn efficiency_mode(&self) -> EfficiencyMode {
        match self.mode_idx {
            0 => EfficiencyMode::Eco,
            2 => EfficiencyMode::Max,
            _ => EfficiencyMode::Profit,
        }
    }

    fn apply_panel_tuning(&mut self) {
        let gpu = &self.gpu_presets[self.gpu_idx];
        let tuning = resolve_panel_tuning(gpu, self.efficiency_mode());
        self.work_groups = tuning.work_groups;
        self.unit_size = tuning.unit_size;
        self.gpu_profile = tuning.profile.to_string();
    }

    fn t(&self) -> Strings {
        strings(self.lang)
    }

    fn cpu_label(&self, idx: usize) -> &str {
        self.cpu_presets[idx].label
    }

    fn gpu_label(&self, idx: usize) -> &str {
        self.gpu_presets[idx].label
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
        self.power_cost = Currency::convert(self.power_cost as f64, from, to) as f32;
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
            gpu_profile: self.gpu_profile.clone(),
            mode: self.efficiency_mode(),
            power_cost_kwh: Currency::convert(self.power_cost as f64, self.currency, Currency::Eur),
            hac_price: Currency::convert(self.hac_price as f64, Currency::Usd, Currency::Eur),
            platform_id: self.platform_id,
            device_id: self.device_id,
            connect: self.connect.clone(),
            stats_file: self.stats_path.to_string_lossy().to_string(),
            opencl_dir: opencl_dir_for(&self.work_dir),
            max_temp_c: self.max_temp_c,
            pause_if_unprofitable: self.pause_unprofitable && self.mining_kind == MiningKind::Hac,
            benchmark_seconds: 0,
            idle_start_hour: 255,
            idle_end_hour: 255,
            benchmark_fine_sweep: true,
            thermal_gpu_index: self.device_id,
            work_groups: self.work_groups,
            unit_size: self.unit_size,
        }
    }

    fn set_mining_kind(&mut self, kind: MiningKind) {
        if kind == self.mining_kind {
            return;
        }
        match self.mining_kind {
            MiningKind::Hac => self.hac_wallet = self.wallet.trim().to_string(),
            MiningKind::Hacd => self.hacd_wallet = self.wallet.trim().to_string(),
        }
        self.mining_kind = kind;
        self.wallet = match kind {
            MiningKind::Hac => self.hac_wallet.clone(),
            MiningKind::Hacd => self.hacd_wallet.clone(),
        };
        if kind == MiningKind::Hacd && self.cpu_presets[self.cpu_idx].supervene == 0 {
            if let Some(idx) = self.cpu_presets.iter().position(|p| p.supervene > 0) {
                self.cpu_idx = idx;
            }
        }
        self.status_msg = match kind {
            MiningKind::Hac => "HAC OpenCL miner ready.".to_string(),
            MiningKind::Hacd => "HACD CPU miner ready — OpenCL is not used.".to_string(),
        };
        save_mining_kind(&self.work_dir, kind);
    }

    fn request_opencl_probe(&mut self, action: OpenClAction) {
        if self.opencl_probe_active() {
            // Reuse the live scan already in flight. Thermal capability is
            // always collected when enabled, so queued Start/Auto Tune can
            // safely resume from the same result.
            self.pending_opencl_action = Some(action);
            self.status_msg = "Checking OpenCL GPU...".to_string();
            return;
        }
        let work_dir = self.work_dir.clone();
        let platform_id = self.platform_id;
        let device_id = self.device_id;
        let thermal_required = self.max_temp_c > 0;
        let (tx, rx) = mpsc::channel();
        let spawn_result = thread::Builder::new()
            .name("hacash-opencl-probe".to_string())
            .spawn(move || {
                let status = opencl_status::load_opencl_status(&work_dir);
                let thermal_available = if thermal_required && status.has_usable_device() {
                    let thermal_device = if status.selection_is_usable(platform_id, device_id) {
                        device_id
                    } else {
                        status.recommended_device.unwrap_or(device_id)
                    };
                    Some(app::efficiency::read_thermal_c_with_gpu("", thermal_device).is_some())
                } else {
                    None
                };
                let _ = tx.send(OpenClProbeResult {
                    status,
                    thermal_available,
                });
            });
        match spawn_result {
            Ok(_) => {
                self.opencl_probe_rx = Some(rx);
                self.pending_opencl_action = Some(action);
                self.status_msg = "Checking OpenCL GPU...".to_string();
            }
            Err(error) => {
                self.status_msg = format!("Could not start the OpenCL check: {error}");
            }
        }
    }

    fn cancel_opencl_probe(&mut self) {
        if self.opencl_probe_rx.take().is_some() {
            self.pending_opencl_action = None;
            self.status_msg = "OpenCL check cancelled.".to_string();
        }
    }

    fn apply_detected_gpu_preset(&mut self, reset_tuning: bool) -> bool {
        let Some(idx) = gpu_idx_for_opencl(
            &self.gpu_presets,
            &self.opencl_status.recommended_name,
            &self.opencl_status.recommended_slug,
            self.opencl_status.recommended_vram_mb,
        ) else {
            return false;
        };
        if idx != self.gpu_idx || reset_tuning {
            self.gpu_idx = idx;
            // This reset is essential on first run: setup configs can contain
            // generic amd_profit/WG1536/US96 values that are unsafe for the
            // actual GPU discovered asynchronously.
            self.apply_panel_tuning();
        }
        true
    }

    fn validate_opencl_probe(
        &mut self,
        result: OpenClProbeResult,
        force_recommended_selection: bool,
        require_start_checks: bool,
    ) -> Result<(), String> {
        let status = result.status;
        if !status.has_usable_device() {
            let detail = status.warnings.first().cloned().unwrap_or_else(|| {
                format!(
                    "OpenCL diagnostic failed. Expected {}",
                    crate::platform::find_worker(&self.work_dir, "diagnose_opencl").display()
                )
            });
            self.opencl_status = status;
            return Err(detail);
        }
        if force_recommended_selection
            || !status.selection_is_usable(self.platform_id, self.device_id)
        {
            let _ = opencl_status::apply_recommended_opencl(
                &status,
                &mut self.platform_id,
                &mut self.device_id,
                !force_recommended_selection,
            );
        }
        self.opencl_status = status;
        if !require_start_checks {
            return Ok(());
        }
        let kernel_dir = PathBuf::from(opencl_dir_for(&self.work_dir));
        if !kernel_dir.join("x16rs_main.cl").is_file() {
            return Err(format!(
                "OpenCL kernels are missing: {}",
                kernel_dir.display()
            ));
        }
        if self.max_temp_c > 0 && result.thermal_available != Some(true) {
            return Err(
                "GPU temperature sensor is unavailable. Keep Max temperature at 0 (off), or install a supported GPU sensor CLI: rocm-smi / amd-smi for AMD, or nvidia-smi for NVIDIA."
                    .into(),
            );
        }
        Ok(())
    }

    fn poll_opencl_probe(&mut self) {
        let result = match self.opencl_probe_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Empty)) | None => return,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.opencl_probe_rx = None;
                self.pending_opencl_action = None;
                self.status_msg = "The OpenCL check stopped unexpectedly.".to_string();
                return;
            }
        };
        self.opencl_probe_rx = None;
        let Some(action) = self.pending_opencl_action.take() else {
            return;
        };
        let first_run_auto_select = self.auto_select_detected_gpu;
        let (force_recommended_selection, require_start_checks) = match action {
            OpenClAction::InitialScan { preserve_selection } => {
                (!preserve_selection || first_run_auto_select, false)
            }
            OpenClAction::AutoDetect => (true, true),
            OpenClAction::StartMining | OpenClAction::AutoTune => (first_run_auto_select, true),
        };
        if let Err(error) =
            self.validate_opencl_probe(result, force_recommended_selection, require_start_checks)
        {
            self.status_msg = format!("OpenCL: {error}");
            return;
        }

        let explicitly_detect = matches!(action, OpenClAction::AutoDetect);
        let tune_action = matches!(action, OpenClAction::AutoTune);
        let selected_is_recommended = self.platform_id
            == self.opencl_status.recommended_platform.unwrap_or(u32::MAX)
            && self.device_id == self.opencl_status.recommended_device.unwrap_or(u32::MAX);
        if first_run_auto_select || explicitly_detect || (tune_action && selected_is_recommended) {
            let reset_tuning = first_run_auto_select || explicitly_detect;
            if self.apply_detected_gpu_preset(reset_tuning) && first_run_auto_select {
                self.auto_select_detected_gpu = false;
            }
        }

        match action {
            OpenClAction::InitialScan { .. } | OpenClAction::AutoDetect => {
                self.status_msg = format!("OpenCL: {}", self.opencl_status.device_summary());
            }
            OpenClAction::StartMining => self.start_mining_after_opencl(),
            OpenClAction::AutoTune => self.run_benchmark_after_opencl(),
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
        if self.mining_settings_locked() {
            return;
        }
        if self.mining_kind == MiningKind::Hacd {
            self.status_msg = "Auto Tune is for HAC OpenCL GPUs; HACD uses CPU mining.".to_string();
            return;
        }
        if self.gpu_presets[self.gpu_idx].slug == "none" {
            self.status_msg = t.no_gpu.to_string();
            return;
        }
        self.request_opencl_probe(OpenClAction::AutoTune);
    }

    fn run_benchmark_after_opencl(&mut self) {
        let t = self.t();
        if !self.poworker_path.exists() {
            self.status_msg = format!("{}\n{}", t.poworker_not_found, self.poworker_path.display());
            return;
        }
        let s = self.panel_settings();
        let backup = match create_benchmark_backup(&self.config_path) {
            Ok(backup) => backup,
            Err(error) => {
                self.status_msg = format!("Could not back up Auto Tune settings: {error}");
                return;
            }
        };
        if let Err(error) = write_poworker_benchmark_config(&self.config_path, &s, 90) {
            let restore = restore_benchmark_backup(&self.config_path, &backup);
            self.status_msg = match restore {
                Ok(()) => format!("{} {error}", t.save_error_prefix),
                Err(restore_error) => format!(
                    "{} {error}; exact config restore also failed: {restore_error}",
                    t.save_error_prefix
                ),
            };
            return;
        }
        self.benchmark_config_backup = Some(backup);
        mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
        let mut cmd = Command::new(&self.poworker_path);
        cmd.current_dir(&self.work_dir);
        match Self::spawn_worker_with_logs(&mut cmd) {
            Ok((child, rx)) => {
                self.benchmark_child = Some(child);
                self.benchmark_log_rx = Some(rx);
                self.benchmark_last_log.clear();
                self.benchmarking = true;
                self.stats_next_read = Instant::now();
                self.status_msg = t.benchmark_running.to_string();
            }
            Err(error) => {
                let restore = self.restore_exact_benchmark_config();
                self.status_msg = match restore {
                    Ok(()) => format!("{} {error}", t.start_failed_prefix),
                    Err(restore_error) => format!(
                        "{} {error}; exact config restore failed: {restore_error}",
                        t.start_failed_prefix
                    ),
                };
            }
        }
    }

    fn restore_exact_benchmark_config(&mut self) -> Result<(), String> {
        let Some(backup) = self.benchmark_config_backup.as_ref() else {
            return Ok(());
        };
        restore_benchmark_backup(&self.config_path, backup).map_err(|error| error.to_string())?;
        self.benchmark_config_backup = None;
        Ok(())
    }

    fn commit_benchmark_config(&mut self) -> Result<(), String> {
        let Some(backup) = self.benchmark_config_backup.as_ref() else {
            return Err("Auto Tune backup marker is missing".to_string());
        };
        commit_benchmark_backup(backup).map_err(|error| error.to_string())?;
        self.benchmark_config_backup = None;
        Ok(())
    }

    fn poll_benchmark_stop(&mut self) -> bool {
        let result = match self.benchmark_stop_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Empty)) => return true,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err("worker reaper stopped unexpectedly".to_string())
            }
            None => return false,
        };
        self.benchmark_stop_rx = None;
        self.benchmarking = false;
        self.benchmark_log_rx = None;
        mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
        let base = self
            .benchmark_stop_message
            .take()
            .unwrap_or_else(|| "Auto Tune stopped.".to_string());
        match result {
            Ok(()) => match self.restore_exact_benchmark_config() {
                Ok(()) => self.status_msg = base,
                Err(error) => {
                    self.status_msg = format!(
                        "{base} Exact settings restore failed: {error}. The recovery backup was kept."
                    );
                }
            },
            Err(error) => {
                self.status_msg = format!(
                    "Could not confirm Auto Tune stopped: {error}. Settings stay locked and the recovery backup was kept."
                );
            }
        }
        true
    }

    fn poll_benchmark(&mut self) {
        if self.poll_benchmark_stop() {
            return;
        }
        if !self.benchmarking {
            return;
        }
        let t = self.t();
        if let Some(rx) = &self.benchmark_log_rx {
            while let Ok(line) = rx.try_recv() {
                if !line.trim().is_empty() {
                    self.benchmark_last_log = line;
                }
            }
        }

        let wait_result = match self.benchmark_child.as_mut() {
            Some(child) => child.try_wait(),
            None => {
                self.benchmarking = false;
                self.benchmark_log_rx = None;
                mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
                self.status_msg = match self.restore_exact_benchmark_config() {
                    Ok(()) => {
                        "Auto Tune stopped because its worker process is unavailable. Previous settings were restored."
                            .to_string()
                    }
                    Err(error) => format!(
                        "Auto Tune worker is unavailable and exact settings restore failed: {error}"
                    ),
                };
                return;
            }
        };
        let exit = match wait_result {
            Ok(Some(exit)) => exit,
            Ok(None) => return,
            Err(error) => {
                if let Some(child) = self.benchmark_child.take() {
                    self.benchmark_stop_rx = Some(mining_control::queue_child_termination(child));
                    self.benchmark_stop_message = Some(format!(
                        "{} benchmark status failed: {error}. Previous settings were restored.",
                        t.start_failed_prefix
                    ));
                    self.status_msg = "Stopping Auto Tune safely...".to_string();
                } else {
                    self.benchmarking = false;
                    let restore = self.restore_exact_benchmark_config();
                    self.status_msg = match restore {
                        Ok(()) => {
                            format!("{} benchmark status failed: {error}", t.start_failed_prefix)
                        }
                        Err(restore_error) => format!(
                            "{} benchmark status failed: {error}; restore failed: {restore_error}",
                            t.start_failed_prefix
                        ),
                    };
                }
                return;
            }
        };

        self.benchmark_child = None;
        self.benchmark_log_rx = None;
        self.benchmarking = false;
        if !exit.success() {
            mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
            let base = format!("{} benchmark exited with {exit}", t.start_failed_prefix);
            self.status_msg = match self.restore_exact_benchmark_config() {
                Ok(()) => format!("{base}. Previous settings were restored."),
                Err(error) => format!("{base}; exact settings restore failed: {error}"),
            };
            return;
        }
        let loaded = load_panel_ini(&self.config_path);
        if loaded.benchmark_seconds != Some(0) {
            mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
            let base = if self.benchmark_last_log.is_empty() {
                "OpenCL benchmark did not produce a valid profile.".to_string()
            } else {
                format!("OpenCL benchmark failed: {}", self.benchmark_last_log)
            };
            self.status_msg = match self.restore_exact_benchmark_config() {
                Ok(()) => format!("{base} Previous settings were restored."),
                Err(error) => format!("{base} Exact settings restore failed: {error}"),
            };
            return;
        }
        if let Err(error) = self.commit_benchmark_config() {
            let restore = self.restore_exact_benchmark_config();
            self.status_msg = match restore {
                Ok(()) => format!(
                    "Auto Tune result could not be committed ({error}); previous settings were restored."
                ),
                Err(restore_error) => format!(
                    "Auto Tune result could not be committed ({error}) and restore failed: {restore_error}"
                ),
            };
            return;
        }
        let mode = self.efficiency_mode();
        apply_benchmark_ini(
            &loaded,
            &self.gpu_presets,
            &mut self.gpu_idx,
            &mut self.work_groups,
            &mut self.unit_size,
            &mut self.gpu_profile,
            mode,
        );
        mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
        self.status_msg = t.benchmark_done.to_string();
    }

    fn stop_benchmark(&mut self) {
        if self.benchmark_stop_rx.is_some() {
            return;
        }
        let was_active = self.benchmark_operation_active() || self.benchmark_child.is_some();
        if let Some(child) = self.benchmark_child.take() {
            self.benchmark_stop_rx = Some(mining_control::queue_child_termination(child));
            self.benchmark_stop_message =
                Some("Auto Tune cancelled. Previous mining settings were restored.".to_string());
            self.status_msg = "Stopping Auto Tune safely...".to_string();
            return;
        }
        self.benchmarking = false;
        self.benchmark_log_rx = None;
        if was_active {
            mining_control::clear_worker_stats(&mut self.stats, &self.stats_path);
            self.benchmark_last_log.clear();
            self.status_msg = match self.restore_exact_benchmark_config() {
                Ok(()) => {
                    "Auto Tune cancelled. Previous mining settings were restored.".to_string()
                }
                Err(error) => format!(
                    "Auto Tune cleanup failed: {error}. The exact recovery backup was kept."
                ),
            };
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
        let normalized_connect = match normalize_connect(&self.connect) {
            Ok(connect) => connect,
            Err(e) => {
                self.status_msg = format!("Invalid connection: {e}");
                return false;
            }
        };
        self.connect = normalized_connect;
        let wallet_check = match self.mining_kind {
            MiningKind::Hac => validate_wallet(&self.wallet),
            MiningKind::Hacd => validate_hacd_wallet(&self.wallet),
        };
        if let Err(e) = wallet_check {
            self.status_msg = if e == "empty" {
                t.wallet_required.to_string()
            } else {
                format!("{} {e}", t.wallet_invalid_prefix)
            };
            return false;
        }
        if self.mining_kind == MiningKind::Hacd && self.bid_password.trim().is_empty() {
            self.status_msg = t.bid_password_required.to_string();
            return false;
        }
        if self.mining_kind == MiningKind::Hacd {
            if let Err(e) = validate_diamond_settings(&self.diamond_settings()) {
                self.status_msg = e;
                return false;
            }
        }
        if !self.power_cost.is_finite()
            || self.power_cost < 0.0
            || !self.hac_price.is_finite()
            || self.hac_price < 0.0
        {
            self.status_msg = "Power cost and HAC price must be valid non-negative numbers.".into();
            return false;
        }
        self.wallet = self.wallet.trim().to_string();
        match self.mining_kind {
            MiningKind::Hac => self.hac_wallet = self.wallet.clone(),
            MiningKind::Hacd => self.hacd_wallet = self.wallet.clone(),
        }
        let s = self.panel_settings();
        let rpc_port = if self.connect_mode == ConnectMode::Solo {
            connect_port(&self.connect)
        } else {
            None
        };
        save_mining_kind(&self.work_dir, self.mining_kind);
        match self.mining_kind {
            MiningKind::Hac => {
                if let Err(e) = write_poworker_config(&self.config_path, &s) {
                    self.status_msg = format!("{} {e}", t.save_error_prefix);
                    return false;
                }
                if let Err(e) =
                    write_hac_miner_only(&self.hacash_config_path, &self.wallet, rpc_port)
                {
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
                if let Err(e) = write_diamond_miner(
                    &self.hacash_config_path,
                    &self.wallet,
                    &self.diamond_settings(),
                    rpc_port,
                ) {
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
}

impl eframe::App for MinerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_opencl_probe();
        self.poll_stats();
        self.fleet.poll();
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
                                    if ui.selectable_label(self.currency == c, c.name()).clicked() {
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
                    if theme::tab_pill(ui, self.tab == 0, theme::TabIcon::Settings, t.tab_settings)
                    {
                        self.tab = 0;
                    }
                    if theme::tab_pill(
                        ui,
                        self.tab == 1,
                        theme::TabIcon::Dashboard,
                        t.tab_dashboard,
                    ) {
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
                    _ => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.ui_dashboard(ui));
                    }
                }
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // The app is already closing, so signal termination directly and do
        // not wait. Auto Tune's durable sidecar is recovered next launch.
        self.stop_mining_on_exit();
        if let Some(mut child) = self.benchmark_child.take() {
            let _ = child.kill();
        }
        self.benchmarking = false;
        self.benchmark_log_rx = None;
        self.fleet.stop();
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn opencl_dir_for(work_dir: &PathBuf) -> String {
    let packaged = work_dir.join("x16rs").join("opencl");
    if packaged.is_dir() {
        return format!("{}/", packaged.to_string_lossy().replace('\\', "/"));
    }

    // Developer builds run from target/debug and need the workspace kernels.
    // Release builds must use the kernels shipped beside the executable so a
    // missing package cannot silently compile sources from an unrelated tree.
    #[cfg(debug_assertions)]
    {
        let workspace = work_dir.join("..").join("..").join("x16rs").join("opencl");
        if workspace.is_dir() {
            return format!("{}/", workspace.to_string_lossy().replace('\\', "/"));
        }
    }

    format!("{}/", packaged.to_string_lossy().replace('\\', "/"))
}
