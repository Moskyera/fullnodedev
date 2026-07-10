use app::efficiency::MiningStatsSnapshot;
use eframe::egui;

use crate::i18n::Strings;
use crate::mining_kind::MiningKind;
use crate::presets;
use crate::theme;

pub struct DashboardDetails<'a> {
    pub t: &'a Strings,
    pub stats: &'a MiningStatsSnapshot,
    pub cpu_label: &'a str,
    pub gpu_label: &'a str,
    pub gpu_slug: &'a str,
    pub connect_display: String,
    pub wallet_display: String,
    pub opencl_display: String,
    pub tuning_display: String,
    pub stats_status: String,
    pub last_update: String,
    pub power_cost_display: String,
    pub max_temp_c: u32,
    pub mining_kind: MiningKind,
}

pub fn show_dashboard_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    ui.label(
        egui::RichText::new(d.t.dash_details_title)
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
            theme::show_detail_row(ui, d.t.dash_detail_cpu, &d.cpu_label);
            theme::show_detail_row(ui, d.t.dash_detail_gpu, &d.gpu_label);
            if presets::is_rdna4_experimental(d.gpu_slug) {
                ui.label(
                    egui::RichText::new(d.t.gpu_rdna4_badge)
                        .color(theme::colors::GOLD)
                        .size(12.0),
                );
                ui.label(
                    egui::RichText::new(d.t.gpu_rdna4_hint)
                        .color(theme::colors::TEXT_MUTED)
                        .size(11.0),
                );
                ui.end_row();
            } else {
                ui.end_row();
            }
            theme::show_detail_row(ui, d.t.dash_detail_connect, &d.connect_display);
            theme::show_detail_row(ui, d.t.dash_detail_wallet, &d.wallet_display);
            ui.end_row();
            theme::show_detail_row(ui, d.t.dash_detail_opencl, &d.opencl_display);
            if d.stats.gpu_hashrate_hps > 0.0 {
                theme::show_detail_row(
                    ui,
                    d.t.dash_detail_gpu_hashrate,
                    &d.stats.gpu_hashrate_display,
                );
            }
            if d.stats.effective_work_groups > 0 {
                let wg_detail = d.t.wg_breakdown_display(
                    d.stats.effective_work_groups,
                    d.stats.configured_work_groups,
                    d.stats.oom_work_groups,
                    d.stats.thermal_cap_work_groups,
                );
                theme::show_detail_row(ui, d.t.dash_detail_effective_wg, &wg_detail);
            }
            theme::show_detail_row(ui, d.t.dash_detail_tuning, &d.tuning_display);
            ui.end_row();
            theme::show_detail_row(ui, d.t.dash_detail_power_cost, &d.power_cost_display);
            theme::show_detail_row(
                ui,
                d.t.dash_detail_max_temp,
                &format!("{}°C", d.max_temp_c),
            );
            ui.end_row();
            theme::show_detail_row(ui, d.t.dash_detail_last_update, &d.last_update);
            theme::show_detail_row(ui, d.t.dash_detail_stats_status, &d.stats_status);
            ui.end_row();
            let diamond_label = (d.mining_kind == MiningKind::Hacd && d.stats.diamond_number > 0)
                .then(|| (d.t.dash_detail_diamond, format!("#{}", d.stats.diamond_number)));
            let profile_label = (!d.stats.gpu_profile.is_empty())
                .then(|| (d.t.stat_gpu_profile, d.stats.gpu_profile.clone()));
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
}