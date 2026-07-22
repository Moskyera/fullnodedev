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

    if d.mining_kind == MiningKind::Hacd {
        show_hacd_details(ui, d);
    } else {
        show_hac_details(ui, d);
    }
}

fn show_hacd_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    egui::Grid::new("dash_details_hacd")
        .num_columns(2)
        .spacing([24.0, 8.0])
        .min_col_width(360.0)
        .show(ui, |ui| {
            theme::show_detail_row(ui, d.t.dash_detail_cpu, d.cpu_label);
            theme::show_detail_row(ui, "Backend", "CPU only • no OpenCL");
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_connect, &d.connect_display);
            theme::show_detail_row(ui, d.t.dash_detail_wallet, &d.wallet_display);
            ui.end_row();

            theme::show_detail_row(ui, "CPU configuration", &d.tuning_display);
            theme::show_detail_row(ui, d.t.dash_detail_power_cost, &d.power_cost_display);
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_last_update, &d.last_update);
            theme::show_detail_row(ui, d.t.dash_detail_stats_status, &d.stats_status);
            ui.end_row();

            if d.stats.diamond_number > 0 || !d.stats.diamond_best.is_empty() {
                let number = if d.stats.diamond_number > 0 {
                    format!("#{}", d.stats.diamond_number)
                } else {
                    d.t.dash_no_data.to_string()
                };
                theme::show_detail_row(ui, d.t.dash_detail_diamond, &number);
                theme::show_detail_row(
                    ui,
                    d.t.stat_diamond_best,
                    if d.stats.diamond_best.is_empty() {
                        d.t.dash_no_data
                    } else {
                        &d.stats.diamond_best
                    },
                );
                ui.end_row();
            }
        });
}

fn show_hac_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    egui::Grid::new("dash_details_hac")
        .num_columns(2)
        .spacing([24.0, 8.0])
        .min_col_width(360.0)
        .show(ui, |ui| {
            theme::show_detail_row(ui, d.t.dash_detail_cpu, d.cpu_label);
            theme::show_detail_row(ui, d.t.dash_detail_gpu, d.gpu_label);
            ui.end_row();

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
            }

            theme::show_detail_row(ui, d.t.dash_detail_connect, &d.connect_display);
            theme::show_detail_row(ui, d.t.dash_detail_wallet, &d.wallet_display);
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_opencl, &d.opencl_display);
            theme::show_detail_row(ui, d.t.dash_detail_tuning, &d.tuning_display);
            ui.end_row();

            if d.stats.gpu_hashrate_hps > 0.0 || d.stats.effective_work_groups > 0 {
                theme::show_detail_row(
                    ui,
                    d.t.dash_detail_gpu_hashrate,
                    if d.stats.gpu_hashrate_hps > 0.0 {
                        &d.stats.gpu_hashrate_display
                    } else {
                        d.t.dash_no_data
                    },
                );
                let wg_detail = if d.stats.effective_work_groups > 0 {
                    d.t.wg_breakdown_display(
                        d.stats.effective_work_groups,
                        d.stats.configured_work_groups,
                        d.stats.oom_work_groups,
                        d.stats.thermal_cap_work_groups,
                    )
                } else {
                    d.t.dash_no_data.to_string()
                };
                theme::show_detail_row(ui, d.t.dash_detail_effective_wg, &wg_detail);
                ui.end_row();
            }

            theme::show_detail_row(ui, d.t.dash_detail_power_cost, &d.power_cost_display);
            theme::show_detail_row(
                ui,
                d.t.dash_detail_max_temp,
                if d.max_temp_c == 0 {
                    "Off".to_string()
                } else {
                    format!("{}°C", d.max_temp_c)
                }
                .as_str(),
            );
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_last_update, &d.last_update);
            theme::show_detail_row(ui, d.t.dash_detail_stats_status, &d.stats_status);
            ui.end_row();
        });
}
