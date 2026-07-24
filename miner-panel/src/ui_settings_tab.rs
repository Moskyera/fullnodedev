//! Settings tab UI (mining type, tuning, network, wallet).

use eframe::egui;

use crate::MinerApp;
use crate::OpenClAction;
use crate::connect::{ConnectMode, PoolInfo};
use crate::mining_kind::MiningKind;
use crate::theme;

/// Dropdown label for a pool: appends a check mark for endpoints we verified.
fn pool_menu_label(p: &PoolInfo) -> String {
    if p.verified {
        format!("{} \u{2713}", p.name)
    } else {
        p.name.clone()
    }
}

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

        // Simple by default: a newcomer sees three steps and one button.
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(if self.simple_mode {
                    "Set up mining"
                } else {
                    "All settings"
                })
                .strong()
                .color(theme::colors::TEXT)
                .size(17.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(i) = theme::segmented(
                    ui,
                    &["Simple", "Advanced"],
                    if self.simple_mode { 0 } else { 1 },
                ) {
                    self.set_simple_mode(i == 0);
                }
            });
        });
        ui.add_space(14.0);

        ui.add_enabled_ui(!settings_locked, |ui| {
            if self.simple_mode {
                self.ui_settings_simple(ui);
            } else {
                self.ui_settings_full(ui);
            }
        });
    }

    /// Every setting, for people who want the knobs.
    fn ui_settings_full(&mut self, ui: &mut egui::Ui) {
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
                                .hint_text("0.5"),
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
                    self.connect_mode_row(ui);
                    ui.end_row();

                    theme::field_label(
                        ui,
                        if self.connect_mode == ConnectMode::Solo {
                            t.label_fullnode
                        } else {
                            t.connect_pool
                        },
                    );
                    self.connect_target_block(ui);
                    ui.end_row();
                });
        });

        self.ui_settings_advanced_tail(ui);
    }

    /// Where the miner connects: the pool picker (HAC pool mode), the address
    /// box, a reachability test and the selected pool's guidance. Shared by the
    /// simple and advanced views so they can never drift apart.
    fn connect_target_block(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.vertical(|ui| {
                        let hac_pool = self.connect_mode == ConnectMode::Pool
                            && self.mining_kind == MiningKind::Hac;
                        if hac_pool {
                            // Clone the directory so the combo closure can call
                            // &mut self (apply/refresh) without aliasing self.
                            let pools = self.pool_directory.clone();
                            let selected_label = pools
                                .get(self.pool_preset_idx)
                                .map(pool_menu_label)
                                .unwrap_or_else(|| "Pool".to_string());
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("pool_preset")
                                    .selected_text(selected_label)
                                    .width(300.0)
                                    .show_ui(ui, |ui| {
                                        for (i, p) in pools.iter().enumerate() {
                                            if ui
                                                .selectable_value(
                                                    &mut self.pool_preset_idx,
                                                    i,
                                                    pool_menu_label(p),
                                                )
                                                .clicked()
                                            {
                                                self.apply_pool_preset(i);
                                            }
                                        }
                                    });
                                if ui
                                    .button("Refresh")
                                    .on_hover_text("Reload pools.json next to the panel")
                                    .clicked()
                                {
                                    self.refresh_pool_directory();
                                }
                            });
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.connect)
                                .desired_width(400.0)
                                .margin(egui::Margin::symmetric(8.0, 6.0)),
                        );
                        if self.connect_mode == ConnectMode::Pool {
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Test connection")
                                    .on_hover_text("Check the address is reachable from this PC")
                                    .clicked()
                                {
                                    self.connect_test_status = match crate::connect::probe_reachable(
                                        &self.connect,
                                        1500,
                                    ) {
                                        Ok(ms) => format!("Reachable ({} ms)", ms),
                                        Err(e) => format!("Not reachable: {}", e),
                                    };
                                }
                                if !self.connect_test_status.is_empty() {
                                    let color = if self.connect_test_status.starts_with("Reachable") {
                                        theme::colors::GREEN
                                    } else {
                                        theme::colors::GOLD_DIM
                                    };
                                    ui.label(
                                        egui::RichText::new(&self.connect_test_status)
                                            .size(11.5)
                                            .color(color),
                                    );
                                }
                            });
                        }
                        if self.connect_mode == ConnectMode::Pool {
                            if hac_pool {
                                // Per-pool guidance + link from the directory entry.
                                if let Some(p) =
                                    self.pool_directory.get(self.pool_preset_idx).cloned()
                                {
                                    let note = if p.note.is_empty() {
                                        t.connect_pool_hint.to_string()
                                    } else {
                                        p.note.clone()
                                    };
                                    ui.label(
                                        egui::RichText::new(note)
                                            .size(11.5)
                                            .color(theme::colors::TEXT_MUTED),
                                    );
                                    if !p.url.is_empty() {
                                        ui.hyperlink_to(format!("Open {}", p.url), &p.url);
                                    }
                                }
                            } else {
                                ui.label(
                                    egui::RichText::new(
                                        "All HACD miners may point to the same full node; its hashrate is accumulated.",
                                    )
                                    .size(11.5)
                                    .color(theme::colors::TEXT_MUTED),
                                );
                            }
                        }
        });
    }

    /// Solo or pool, worded for the mining type in play.
    fn connect_mode_row(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
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
            ui.add_space(8.0);
            if ui.selectable_label(!solo, remote_label).clicked() {
                self.set_connect_mode(ConnectMode::Pool);
            }
        });
    }

    /// The reward address box plus the hint for the current mining type.
    fn wallet_field(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.add(
            egui::TextEdit::singleline(&mut self.wallet)
                .desired_width(420.0)
                .hint_text("1LCY6uQS3iNGy2mKSmhFVU2dHgBQLf74Fx")
                .margin(egui::Margin::symmetric(10.0, 8.0)),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(if self.mining_kind == MiningKind::Hacd {
                t.hacd_wallet_hint
            } else {
                t.wallet_hint
            })
            .size(11.5)
            .color(theme::colors::TEXT_MUTED),
        );
    }

    /// Three steps and one button. Everything else lives under Advanced.
    fn ui_settings_simple(&mut self, ui: &mut egui::Ui) {
        let t = self.t();

        theme::step_card(
            ui,
            1,
            "What do you want to mine?",
            "HAC uses your graphics card. HACD (diamonds) runs on the CPU through a full node.",
            |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(self.mining_kind == MiningKind::Hac, t.mining_hac)
                        .clicked()
                    {
                        self.set_mining_kind(MiningKind::Hac);
                    }
                    ui.add_space(8.0);
                    if ui
                        .selectable_label(self.mining_kind == MiningKind::Hacd, t.mining_hacd)
                        .clicked()
                    {
                        self.set_mining_kind(MiningKind::Hacd);
                    }
                });
                if self.mining_kind == MiningKind::Hac {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Graphics card:")
                                .size(12.5)
                                .color(theme::colors::TEXT_MUTED),
                        );
                        let usable = self.opencl_status.has_usable_device();
                        ui.label(
                            egui::RichText::new(self.opencl_status.device_summary())
                                .size(12.5)
                                .strong()
                                .color(if usable {
                                    theme::colors::GREEN
                                } else {
                                    theme::colors::GOLD
                                }),
                        );
                        if !usable && ui.small_button("Detect").clicked() {
                            self.request_opencl_probe(OpenClAction::AutoDetect);
                        }
                    });
                }
            },
        );

        theme::step_card(
            ui,
            2,
            "Where do you connect?",
            "A pool pays you small amounts often. Solo pays only when you find a whole block yourself.",
            |ui| {
                self.connect_mode_row(ui);
                ui.add_space(12.0);
                self.connect_target_block(ui);
            },
        );

        theme::step_card(
            ui,
            3,
            "Where should your coins go?",
            "Paste your HAC address. In pool mode this is also the address the pool pays.",
            |ui| {
                self.wallet_field(ui);
            },
        );

        // HACD diamond mining also needs the bid account password; without it
        // Start would dead-end. Ask for it here instead of hiding it in Advanced.
        if self.mining_kind == MiningKind::Hacd {
            theme::step_card(
                ui,
                4,
                "Diamond bid password",
                "Diamond mining bids from your full node account. Enter its password. The bid amounts use safe defaults, which you can change under Advanced.",
                |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.bid_password)
                            .password(true)
                            .desired_width(420.0)
                            .margin(egui::Margin::symmetric(10.0, 8.0)),
                    );
                },
            );
        }

        if self.connect_mode == ConnectMode::Pool {
            theme::note(
                ui,
                theme::colors::ACCENT,
                "Your address is sent to the pool automatically so it can credit your work and pay you. There is nothing else to set up.",
            );
            ui.add_space(14.0);
        }

        self.action_row(ui);
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Want GPU tuning, power limits or to host a shared node? Switch to Advanced at the top.",
            )
            .size(11.5)
            .color(theme::colors::TEXT_MUTED),
        );
    }

    /// The sections only an experienced user needs: worker tuning knobs, hosting
    /// a shared node, the reward address card, fleet settings and the actions.
    fn ui_settings_advanced_tail(&mut self, ui: &mut egui::Ui) {
        let t = self.t();

        // Everything a different pool might need, editable from the GUI so the
        // user never has to open poworker.config.ini. Defaults suit every pool;
        // a directory entry can also preset these when a pool is selected.
        if self.mining_kind == MiningKind::Hac && self.connect_mode == ConnectMode::Pool {
            ui.add_space(8.0);
            egui::CollapsingHeader::new("Advanced worker settings (optional)")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Only change these if a pool documents specific values.",
                        )
                        .size(11.0)
                        .color(theme::colors::TEXT_MUTED),
                    );
                    egui::Grid::new("adv_worker_grid")
                        .num_columns(2)
                        .spacing([20.0, 8.0])
                        .show(ui, |ui| {
                            theme::field_label(ui, "nonce_max");
                            ui.add(egui::DragValue::new(&mut self.nonce_max));
                            ui.end_row();
                            theme::field_label(ui, "notice_wait (s)");
                            ui.add(egui::DragValue::new(&mut self.notice_wait).range(1..=600));
                            ui.end_row();
                        });
                    if ui.button("Reset to defaults").clicked() {
                        self.nonce_max = u32::MAX;
                        self.notice_wait = 45;
                    }
                });
        }

        // All-in-one public free-IP pool (hac-pool)
        if self.mining_kind == MiningKind::Hac {
            ui.add_space(12.0);
            theme::section_card().show(ui, |ui| {
                ui.label(
                    egui::RichText::new("SHARED NODE / OPEN WORK RELAY")
                        .strong()
                        .size(12.0)
                        .color(theme::colors::ACCENT),
                );
                ui.label(
                    egui::RichText::new(
                        "Share this PC's mining work so others can point their miners at YOUR IP:HTTP port \
(local mining can use 127.0.0.1). Requires hac-pool.exe next to the panel.",
                    )
                    .size(11.5)
                    .color(theme::colors::TEXT_MUTED),
                );
                ui.label(
                    egui::RichText::new(
                        "Honest note: this is a work relay, not a share/payout pool. Any block found is \
minted to THIS node's reward wallet (the host) - connected workers help find blocks but are not \
individually paid. No share accounting or payouts (v1).",
                    )
                    .size(11.0)
                    .color(theme::colors::GOLD_DIM),
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

                            theme::field_label(ui, "Max connections per IP");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.public_pool.max_conns_per_ip)
                                        .range(0..=100000),
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

                    ui.label(
                        egui::RichText::new(
                            "Max connections per IP: 0 = unlimited. A large farm behind one NAT \
or router may need this raised.",
                        )
                        .size(11.0)
                        .color(theme::colors::TEXT_MUTED),
                    );

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

                    ui.horizontal(|ui| {
                        if ui
                            .button("Test upstream")
                            .on_hover_text("Check the upstream full node is reachable from this PC")
                            .clicked()
                        {
                            self.upstream_test_status = match crate::connect::probe_reachable(
                                &self.public_pool.upstream,
                                1500,
                            ) {
                                Ok(ms) => format!("Upstream reachable ({} ms)", ms),
                                Err(e) => format!("Upstream not reachable: {}", e),
                            };
                        }
                        if !self.upstream_test_status.is_empty() {
                            let color =
                                if self.upstream_test_status.starts_with("Upstream reachable") {
                                    theme::colors::GREEN
                                } else {
                                    theme::colors::GOLD_DIM
                                };
                            ui.label(
                                egui::RichText::new(&self.upstream_test_status)
                                    .size(11.5)
                                    .color(color),
                            );
                        }
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
                            "External workers connect to  YOUR_PUBLIC_IP:{}. For this to reach them over \
the internet you need a public IP and the port forwarded/allowed by your router + firewall (home \
NAT/CGNAT often blocks it). This panel cannot verify external reachability - test from another network.",
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
        self.action_row(ui);
        ui.add_space(8.0);
    }

    /// Save and Start: the two things every view ends with.
    fn action_row(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.horizontal(|ui| {
            if theme::btn_secondary(ui, t.btn_save).clicked() {
                self.save_config();
            }
            ui.add_space(10.0);
            if theme::btn_primary_large(ui, t.btn_start_mining).clicked() {
                self.start_mining();
                self.tab = 1;
            }
        });
    }
}
