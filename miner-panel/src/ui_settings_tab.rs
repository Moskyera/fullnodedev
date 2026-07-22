//! Settings tab UI (mining type, tuning, network, wallet).

use eframe::egui;

use crate::MinerApp;
use crate::OpenClAction;
use crate::connect::{ConnectMode, pool_presets};
use crate::mining_kind::MiningKind;
use crate::theme;

impl MinerApp {
    pub(super) fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let settings_locked = self.mining_settings_locked();
        if settings_locked {
            let activity = if self.opencl_probe_active() {
                "the OpenCL GPU check is running"
            } else if self.worker_stopping() {
                "the miner is stopping safely"
            } else if self.worker_stop_needs_restart() {
                "the previous worker stop could not be confirmed"
            } else if self.benchmark_stopping() {
                "Auto Tune is stopping safely"
            } else if self.benchmark_config_backup.is_some() && !self.benchmarking {
                "Auto Tune recovery needs attention"
            } else if self.benchmarking {
                "Auto Tune is running"
            } else if self.pending_start.is_some() {
                "the miner is waiting for the full node"
            } else if self.restart_worker.is_some() {
                "an automatic worker retry is pending"
            } else {
                "mining is active"
            };
            theme::section_card().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Mining settings are locked while {activity}."
                        ))
                        .strong()
                        .color(theme::colors::GOLD),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if self.opencl_probe_active() {
                            "Cancel OpenCL check"
                        } else if self.worker_stopping() {
                            "Stopping miner..."
                        } else if self.worker_stop_needs_restart() {
                            "Worker stop not confirmed"
                        } else if self.benchmark_stopping() {
                            "Stopping..."
                        } else if self.benchmark_config_backup.is_some() && !self.benchmarking {
                            "Retry settings restore"
                        } else if self.benchmarking {
                            "Cancel Auto Tune"
                        } else if self.pending_start.is_some() || self.restart_worker.is_some() {
                            "Cancel start / retry"
                        } else {
                            t.btn_stop
                        };
                        if self.benchmark_stopping()
                            || self.worker_stopping()
                            || self.worker_stop_needs_restart()
                        {
                            ui.add_enabled(false, egui::Button::new(label));
                        } else if theme::btn_danger(ui, label).clicked() {
                            if self.opencl_probe_active() {
                                self.cancel_opencl_probe();
                            } else if self.benchmark_operation_active() {
                                self.stop_benchmark();
                            } else {
                                self.stop_mining();
                            }
                        }
                    });
                });
            });
            ui.add_space(12.0);
        }

        ui.add_enabled_ui(!settings_locked, |ui| {
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
                    self.set_mining_kind(MiningKind::Hac);
                }
                ui.add_space(12.0);
                if ui
                    .selectable_label(self.mining_kind == MiningKind::Hacd, t.mining_hacd)
                    .clicked()
                {
                    self.set_mining_kind(MiningKind::Hacd);
                }
            });
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(if self.mining_kind == MiningKind::Hacd {
                    "HACD: CPU-only diamond mining through a local or LAN full node."
                } else {
                    "HAC: OpenCL GPU mining with automatic safe tuning."
                })
                .color(theme::colors::ACCENT)
                .size(12.0),
            );
        });

        ui.add_space(12.0);

        self.show_hw_tuning_section(ui);

        ui.add_space(12.0);

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("eco_grid")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
                    theme::field_label(ui, t.label_power_cost);
                    theme::power_cost_slider(ui, &mut self.power_cost, self.currency);
                    ui.end_row();

                    if self.mining_kind == MiningKind::Hac {
                        theme::field_label(ui, t.label_hac_price);
                        ui.add(
                            egui::DragValue::new(&mut self.hac_price)
                                .speed(0.01)
                                .range(0.0..=1_000_000.0)
                                .suffix(" $"),
                        );
                        ui.end_row();

                        theme::field_label(ui, t.label_max_temp);
                        ui.vertical(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.max_temp_c)
                                    .range(0..=95)
                                    .suffix(" °C"),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "0 = off. Thermal protection requires a readable GPU sensor.",
                                )
                                .size(11.0)
                                .color(theme::colors::TEXT_MUTED),
                            );
                        });
                        ui.end_row();

                        ui.label("");
                        ui.checkbox(&mut self.pause_unprofitable, t.label_pause_unprofitable);
                        ui.end_row();
                    } else {
                        theme::field_label(ui, "Power estimate:");
                        ui.label(
                            egui::RichText::new(
                                "CPU threads × 8 W. GPU power and temperature do not apply.",
                            )
                            .color(theme::colors::TEXT_MUTED),
                        );
                        ui.end_row();
                    }
                });
        });

        if self.mining_kind == MiningKind::Hac {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::btn_secondary(ui, "Auto Tune OpenCL GPU").clicked() {
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
        }

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
                                .hint_text("1"),
                        );
                        ui.end_row();

                        theme::field_label(ui, t.label_bid_max);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.bid_max)
                                .desired_width(160.0)
                                .hint_text("31"),
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
                        let local_label = if self.mining_kind == MiningKind::Hacd {
                            "Local full node"
                        } else {
                            t.connect_solo
                        };
                        let remote_label = if self.mining_kind == MiningKind::Hacd {
                            "LAN / remote full node"
                        } else {
                            t.connect_pool
                        };
                        if ui.selectable_label(solo, local_label).clicked() {
                            self.set_connect_mode(ConnectMode::Solo);
                        }
                        if ui.selectable_label(!solo, remote_label).clicked() {
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
                        if self.connect_mode == ConnectMode::Pool && self.mining_kind == MiningKind::Hac {
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
                                            .selectable_value(&mut self.pool_preset_idx, i, p.label)
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
                                egui::RichText::new(if self.mining_kind == MiningKind::Hacd {
                                    "All HACD miners may point to the same full node; its hashrate is accumulated."
                                } else {
                                    t.connect_pool_hint
                                })
                                    .size(11.5)
                                    .color(theme::colors::TEXT_MUTED),
                            );
                        }
                    });
                    ui.end_row();
                });
        });

        // All-in-one public free-IP pool (hac-pool)
        if self.mining_kind == MiningKind::Hac {
            ui.add_space(12.0);
            theme::section_card().show(ui, |ui| {
                ui.label(
                    egui::RichText::new("PUBLIC FREE-IP POOL (ALL-IN-ONE)")
                        .strong()
                        .size(12.0)
                        .color(theme::colors::ACCENT),
                );
                ui.label(
                    egui::RichText::new(
                        "Host a public pool from this PC. Others connect with your IP:HTTP port. \
Local mining can use 127.0.0.1 via the pool. Requires hac-pool.exe next to the panel.",
                    )
                    .size(11.5)
                    .color(theme::colors::TEXT_MUTED),
                );
                ui.add_space(8.0);

                let mut host = self.public_pool.host_enabled;
                if ui
                    .checkbox(&mut host, "Enable public pool controls")
                    .changed()
                {
                    self.public_pool.host_enabled = host;
                    self.save_public_pool_settings();
                }

                ui.add_enabled_ui(self.public_pool.host_enabled, |ui| {
                    egui::Grid::new("public_pool_grid")
                        .num_columns(2)
                        .spacing([20.0, 10.0])
                        .show(ui, |ui| {
                            theme::field_label(ui, "Upstream fullnode");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.public_pool.upstream)
                                        .desired_width(280.0)
                                        .hint_text("127.0.0.1:8080"),
                                )
                                .changed()
                            {
                                self.save_public_pool_settings();
                            }
                            ui.end_row();

                            theme::field_label(ui, "HTTP port (workers)");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.public_pool.http_port)
                                        .range(1024..=65535),
                                )
                                .changed()
                            {
                                self.save_public_pool_settings();
                            }
                            ui.end_row();

                            theme::field_label(ui, "Stratum port");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.public_pool.stratum_port)
                                        .range(1024..=65535),
                                )
                                .changed()
                            {
                                self.save_public_pool_settings();
                            }
                            ui.end_row();

                            theme::field_label(ui, "Pool token (optional)");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.public_pool.token)
                                        .desired_width(280.0)
                                        .hint_text("empty = open free pool"),
                                )
                                .changed()
                            {
                                self.save_public_pool_settings();
                            }
                            ui.end_row();
                        });

                    if ui
                        .checkbox(
                            &mut self.public_pool.mine_through_pool,
                            "When pool starts, mine through it (set Connect to 127.0.0.1:HTTP)",
                        )
                        .changed()
                    {
                        self.save_public_pool_settings();
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let can_start = !self.public_pool_running;
                        if ui
                            .add_enabled(can_start, egui::Button::new("Start public pool"))
                            .clicked()
                        {
                            self.start_public_pool();
                        }
                        if ui
                            .add_enabled(self.public_pool_running, egui::Button::new("Stop public pool"))
                            .clicked()
                        {
                            self.stop_public_pool();
                        }
                        let badge = if self.public_pool_running {
                            ("RUNNING", theme::colors::GREEN)
                        } else {
                            ("STOPPED", theme::colors::TEXT_MUTED)
                        };
                        ui.label(egui::RichText::new(badge.0).color(badge.1).strong());
                    });

                    if !self.public_pool_status.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.public_pool_status)
                                .size(11.5)
                                .color(theme::colors::TEXT_MUTED),
                        );
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "External workers: connect = YOUR_PUBLIC_IP:{}  (firewall must allow it)",
                            self.public_pool.http_port
                        ))
                        .size(11.0)
                        .color(theme::colors::GOLD_DIM),
                    );
                });
            });
        }

        ui.add_space(12.0);
        theme::section_card().show(ui, |ui| {
            egui::Grid::new("wallet_grid_after_pool")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
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

                    if self.mining_kind == MiningKind::Hac {
                        theme::field_label(ui, t.label_opencl);
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(self.opencl_status.device_summary())
                                    .color(if self.opencl_status.has_usable_device() {
                                        theme::colors::GREEN
                                    } else {
                                        theme::colors::RED
                                    })
                                    .strong(),
                            );
                            if ui.small_button("Auto-detect OpenCL GPU").clicked() {
                                self.request_opencl_probe(OpenClAction::AutoDetect);
                            }
                            ui.collapsing("Advanced OpenCL IDs", |ui| {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut self.platform_id).range(0..=32),
                                    );
                                    ui.label(
                                        egui::RichText::new(t.platform)
                                            .color(theme::colors::TEXT_MUTED)
                                            .size(12.0),
                                    );
                                    ui.add_space(8.0);
                                    ui.add(egui::DragValue::new(&mut self.device_id).range(0..=32));
                                    ui.label(
                                        egui::RichText::new(t.device_hint)
                                            .color(theme::colors::TEXT_MUTED)
                                            .size(12.0),
                                    );
                                });
                            });
                        });
                        ui.end_row();
                        if !self.opencl_status.warnings.is_empty() {
                            ui.label(
                                egui::RichText::new("OpenCL")
                                    .color(theme::colors::TEXT_MUTED)
                                    .size(12.0),
                            );
                            ui.vertical(|ui| {
                                for w in &self.opencl_status.warnings {
                                    ui.label(
                                        egui::RichText::new(format!("! {w}"))
                                            .color(theme::colors::GOLD)
                                            .size(11.5),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    } else {
                        theme::field_label(ui, "Mining backend:");
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("CPU-only • OpenCL not required")
                                    .color(theme::colors::ACCENT)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "The panel starts diaworker and the Hacash full node automatically.",
                                )
                                .color(theme::colors::TEXT_MUTED)
                                .size(11.5),
                            );
                        });
                        ui.end_row();
                    }
                });
        });

        ui.add_space(12.0);
        self.fleet.show_settings(ui);

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
        });
        ui.add_space(8.0);
        }); // end add_enabled_ui(!settings_locked)
    }
}
