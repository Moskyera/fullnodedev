//! Dashboard tab UI (live stats and mining controls).

use eframe::egui;

use crate::MinerApp;
use crate::connect::ConnectMode;
use crate::currency::Currency;
use crate::dashboard;
use crate::mining_kind::MiningKind;
use crate::theme;

fn abbreviated_wallet(wallet: &str) -> String {
    let wallet = wallet.trim();
    let chars: Vec<char> = wallet.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 20 {
        return wallet.to_string();
    }
    let prefix: String = chars.iter().take(8).collect();
    let suffix: String = chars[chars.len() - 6..].iter().collect();
    format!("{prefix}…{suffix}")
}

impl MinerApp {
    fn truncate_wallet(wallet: &str) -> String {
        abbreviated_wallet(wallet)
    }

    fn stat_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
        if value.trim().is_empty() {
            fallback
        } else {
            value
        }
    }

    pub(super) fn ui_dashboard(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let s = &self.stats;
        let is_hacd = self.mining_kind == MiningKind::Hacd;
        let configured_cpu = self.cpu_presets[self.cpu_idx].supervene;
        let active_cpu = if s.active_cpu_threads > 0 {
            s.active_cpu_threads
        } else {
            configured_cpu
        };
        let cpu_threads = format!("{} / {}", active_cpu, configured_cpu.max(active_cpu));
        let mining_type = if is_hacd { t.mining_hacd } else { t.mining_hac };
        let backend = if is_hacd {
            "CPU • full node"
        } else {
            "OpenCL • GPU"
        };
        let connect_mode = match (self.connect_mode, is_hacd) {
            (ConnectMode::Solo, true) => "Local full node",
            (ConnectMode::Pool, true) => "LAN / remote full node",
            (ConnectMode::Solo, false) => t.connect_solo,
            (ConnectMode::Pool, false) => t.connect_pool,
        };
        let connect_display = format!("{} • {}", self.connect, connect_mode);
        let wallet_display = if self.wallet.trim().is_empty() {
            t.dash_no_data.to_string()
        } else {
            Self::truncate_wallet(&self.wallet)
        };
        let opencl_display = if is_hacd {
            "Not used (CPU-only)".to_string()
        } else if self.gpu_presets[self.gpu_idx].slug == "none" {
            t.dash_no_data.to_string()
        } else {
            format!("Platform {} • Device {}", self.platform_id, self.device_id)
        };
        let tuning_display = if is_hacd {
            format!("{} CPU threads", configured_cpu)
        } else if self.gpu_presets[self.gpu_idx].slug == "none" {
            t.dash_no_data.to_string()
        } else {
            format!(
                "{} • WG {} × US {}",
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
        let hash_display = Self::stat_or(&s.hashrate_display, t.dash_no_data);
        let daily_cost = self.currency.format_amount(Currency::convert(
            s.daily_cost_eur,
            Currency::Eur,
            self.currency,
        ));
        let diamond_number_display = if s.diamond_number > 0 {
            format!("#{}", s.diamond_number)
        } else {
            t.dash_no_data.to_string()
        };

        theme::section_card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("MINING OVERVIEW")
                            .size(11.0)
                            .strong()
                            .color(theme::colors::ACCENT),
                    );
                    ui.label(
                        egui::RichText::new(format!("{mining_type}  •  {backend}"))
                            .size(20.0)
                            .strong()
                            .color(theme::colors::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(format!("Updated: {last_update}"))
                            .size(11.5)
                            .color(theme::colors::TEXT_MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::status_badge(ui, self.miner_badge_state(), self.miner_status_label());
                });
            });
        });

        ui.add_space(12.0);
        theme::section_card().show(ui, |ui| {
            egui::Grid::new("dash_primary_stats")
                .num_columns(4)
                .spacing([12.0, 12.0])
                .min_col_width(190.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        if is_hacd {
                            "CPU hashrate"
                        } else {
                            t.stat_hashrate
                        },
                        hash_display,
                    );
                    if is_hacd {
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_cpu_threads,
                            &cpu_threads,
                        );
                        theme::show_stat_card(
                            ui,
                            theme::colors::ACCENT,
                            t.dash_detail_diamond,
                            &diamond_number_display,
                        );
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_diamond_best,
                            Self::stat_or(&s.diamond_best, t.dash_no_data),
                        );
                    } else {
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_hac_day,
                            &format!("{:.4}", s.hac_per_day),
                        );
                        theme::show_stat_card(
                            ui,
                            theme::colors::ACCENT,
                            t.stat_net_day,
                            &self.currency.format_amount(Currency::convert(
                                s.daily_net_eur,
                                Currency::Eur,
                                self.currency,
                            )),
                        );
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_gpu_profile,
                            Self::stat_or(&s.gpu_profile, &self.gpu_profile),
                        );
                    }
                    ui.end_row();
                });
        });

        ui.add_space(12.0);
        theme::section_card().show(ui, |ui| {
            egui::Grid::new("dash_health_stats")
                .num_columns(4)
                .spacing([12.0, 12.0])
                .min_col_width(190.0)
                .show(ui, |ui| {
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        t.stat_power,
                        &format!("{:.0} W", s.watts),
                    );
                    theme::show_stat_card(ui, theme::colors::GOLD, t.stat_cost_day, &daily_cost);
                    theme::show_stat_card(
                        ui,
                        theme::colors::ACCENT,
                        t.stat_efficiency,
                        &format!("{:.1} kH/J", s.kh_per_j),
                    );
                    if is_hacd {
                        theme::show_stat_card(ui, theme::colors::GOLD, "Backend", "CPU only");
                    } else {
                        theme::show_stat_card(
                            ui,
                            theme::colors::GOLD,
                            t.stat_block_height,
                            &format!("{}", s.height),
                        );
                    }
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

        ui.add_space(12.0);
        self.fleet.show_dashboard(ui, &self.stats);

        if is_hacd && self.connect_mode == ConnectMode::Pool {
            ui.add_space(12.0);
            theme::section_card().show(ui, |ui| {
                ui.label(
                    egui::RichText::new("SHARED HACD FULL NODE")
                        .size(11.0)
                        .strong()
                        .color(theme::colors::ACCENT),
                );
                ui.label(
                    egui::RichText::new(
                        "Multiple CPU miners can use this endpoint. Their work is accumulated by the same full node.",
                    )
                    .color(theme::colors::TEXT),
                );
                ui.label(
                    egui::RichText::new(&self.connect)
                        .color(theme::colors::TEXT_MUTED)
                        .monospace(),
                );
            });
        }

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            let worker_active = self.worker_operation_active();
            if !worker_active
                && !self.benchmark_operation_active()
                && !self.opencl_probe_active()
                && theme::btn_primary(ui, t.btn_start).clicked()
            {
                self.start_mining();
            }
            if self.opencl_probe_active() {
                if theme::btn_danger(ui, "Cancel OpenCL check").clicked() {
                    self.cancel_opencl_probe();
                }
            } else if self.benchmark_operation_active() {
                let label = if self.benchmark_stopping() {
                    "Stopping Auto Tune..."
                } else if self.benchmarking {
                    "Cancel Auto Tune"
                } else {
                    "Retry settings restore"
                };
                if !self.benchmark_stopping() && theme::btn_danger(ui, label).clicked() {
                    self.stop_benchmark();
                }
            } else if self.worker_stopping() {
                ui.add_enabled(false, egui::Button::new("Stopping miner..."));
            } else if self.worker_stop_needs_restart() {
                ui.add_enabled(false, egui::Button::new("Worker stop not confirmed"));
            } else if worker_active {
                let label = if self.pending_start.is_some() || self.restart_worker.is_some() {
                    "Cancel start / retry"
                } else {
                    t.btn_stop_icon
                };
                if theme::btn_danger(ui, label).clicked() {
                    self.stop_mining();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::abbreviated_wallet;

    #[test]
    fn wallet_abbreviation_is_unicode_safe() {
        let wallet = "παράδειγμα-πορτοφόλι-δοκιμής";
        let abbreviated = abbreviated_wallet(wallet);
        assert!(abbreviated.contains('…'));
        assert!(abbreviated.starts_with("παράδειγ"));
        assert!(abbreviated.ends_with("οκιμής"));
    }

    #[test]
    fn short_wallet_is_preserved() {
        assert_eq!(abbreviated_wallet("  1ShortWallet  "), "1ShortWallet");
    }
}
