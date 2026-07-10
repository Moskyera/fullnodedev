mod assets;
mod config;
mod connect;
mod dashboard;
mod mining_control;
mod opencl_status;
mod stats_poll;
mod ui_settings;
mod ui_settings_tab;
mod ui_dashboard_tab;
mod ui_help_tab;
mod currency;
mod fonts;
mod hacash_config;
mod help_options;
mod i18n;
mod mining_kind;
mod platform;
mod presets;
mod theme;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use app::efficiency::{EfficiencyMode, MiningStatsSnapshot};
use config::{
    apply_benchmark_ini, apply_loaded_ini, load_panel_ini, write_diaworker_config,
    write_poworker_benchmark_config, write_poworker_config, PanelSettings,
};
use presets::resolve_panel_tuning;
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
    gpu_profile: String,
    benchmarking: bool,
    benchmark_child: Option<Child>,
    pending_start: Option<mining_control::PendingStart>,
    mining: bool,
    child: Option<Child>,
    stats: MiningStatsSnapshot,
    status_msg: String,
    log_rx: Option<Receiver<String>>,
    tab: usize,
    logo_texture: Option<egui::TextureHandle>,
    opencl_status: opencl_status::OpenClStatus,
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
        let poworker_path = platform::find_worker(&work_dir, "poworker");
        let diaworker_path = platform::find_worker(&work_dir, "diaworker");
        let cpus = cpu_presets();
        let gpus = gpu_presets();
        let lang = load_lang(&work_dir);
        let currency = load_currency(&work_dir, lang);
        let t = strings(lang);
        let mut cpu_idx = 2usize;
        let mut gpu_idx = 6usize; // rx9070xt default
        let mut mode_idx = 1usize;
        let mut power_cost = currency.default_power_cost();
        let mut hac_price = 0.0f32;
        let mut platform_id = 0u32;
        let mut device_id = 0u32;
        let mut connect = SOLO_DEFAULT.to_string();
        let mut max_temp_c = 83u32;
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
        );
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
        let opencl_status = opencl_status::load_opencl_status(&work_dir);
        let mut status_msg = t.ready_status.to_string();
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
            gpu_profile,
            benchmarking: false,
            benchmark_child: None,
            pending_start: None,
            mining: false,
            child: None,
            stats: MiningStatsSnapshot::default(),
            status_msg,
            log_rx: None,
            tab: 0,
            logo_texture,
            opencl_status,
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
            gpu_profile: self.gpu_profile.clone(),
            mode: self.efficiency_mode(),
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
                let mode = self.efficiency_mode();
                apply_benchmark_ini(
                    &load_panel_ini(&self.config_path),
                    &self.gpu_presets,
                    &mut self.gpu_idx,
                    &mut self.work_groups,
                    &mut self.unit_size,
                    &mut self.gpu_profile,
                    mode,
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
        self.apply_panel_tuning();
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

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
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
