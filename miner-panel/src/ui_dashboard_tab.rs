//! Dashboard tab UI (live stats and mining controls).

use eframe::egui;

use crate::connect::ConnectMode;
use crate::dashboard;
use crate::mining_kind::MiningKind;
use crate::theme;
use crate::MinerApp;

impl MinerApp {
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

    pub(super) fn ui_dashboard(&mut self, ui: &mut egui::Ui) {
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
            format!(
                "{} · WG {} × US {}",
                self.gpu_profile, self.work_groups, self.unit_size
            )
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
            dashboard::show_dashboard_details(
                ui,
                &dashboard::DashboardDetails {
                    t: &t,
                    stats: s,
                    cpu_label: self.cpu_label(self.cpu_idx),
                    gpu_label: self.gpu_label(self.gpu_idx),
                    gpu_slug: &self.gpu_presets[self.gpu_idx].slug,
                    connect_display,
                    wallet_display,
                    opencl_display,
                    tuning_display,
                    stats_status,
                    last_update,
                    power_cost_display,
                    max_temp_c: self.max_temp_c,
                    mining_kind: self.mining_kind,
                },
            );
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
}