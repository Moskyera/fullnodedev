//! The Setup screen: four numbered steps, in the order the decisions actually
//! have to be made.
//!
//! 1 what to mine, 2 what to mine it with, 3 what it may cost and how hot it
//! may get, 4 where the work comes from and where the coins go. Advanced adds
//! the knobs that only matter once those four are answered; Simple keeps the
//! same steps with everything derived left out.

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

/// One column of the two column field grid the step cards use. Shared with
/// `ui_settings.rs` so both halves of a row line up.
pub(crate) fn col_width(ui: &egui::Ui) -> f32 {
    ((ui.available_width() - 18.0) / 2.0).max(150.0)
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

        // Page heading, with the Simple / Advanced switch on the right where the
        // mockup puts it.
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(t.setup_page_title)
                        .strong()
                        .color(theme::colors::TEXT)
                        .size(22.0),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(if self.simple_mode {
                        t.settings_intro
                    } else {
                        t.setup_page_sub
                    })
                    .color(theme::colors::TEXT_MUTED)
                    .size(theme::typo::SUB),
                );
            });
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
        ui.add_space(18.0);

        ui.add_enabled_ui(!settings_locked, |ui| {
            if self.simple_mode {
                self.ui_settings_simple(ui);
            } else {
                self.ui_settings_full(ui);
            }
        });
    }

    /// Every setting, arranged as the four numbered steps.
    fn ui_settings_full(&mut self, ui: &mut egui::Ui) {
        let t = self.t();

        theme::step_card(
            ui,
            1,
            t.step_mining_type_title,
            t.step_mining_type_hint,
            |ui| self.mining_kind_cards(ui),
        );

        theme::step_card(ui, 2, t.step_hardware_title, t.step_hardware_hint, |ui| {
            self.hardware_fields(ui)
        });

        theme::step_card(ui, 3, t.step_profit_title, t.step_profit_hint, |ui| {
            self.profit_fields(ui)
        });

        theme::step_card(
            ui,
            4,
            t.step_connection_title,
            t.step_connection_hint,
            |ui| {
                self.connect_mode_cards(ui);
                ui.add_space(16.0);
                self.connect_target_fields(ui, true);
            },
        );

        // Diamond bidding spends real HAC from the node's account, so it is a
        // step of its own rather than a knob inside another card.
        if self.mining_kind == MiningKind::Hacd {
            theme::step_card(ui, 5, t.step_bid_title, t.bid_hint, |ui| {
                self.bid_fields(ui)
            });
        }

        self.ui_settings_advanced_tail(ui);
    }

    /// HAC or HACD, as two cards you pick between.
    fn mining_kind_cards(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let col = col_width(ui);
        let kind = self.mining_kind;
        ui.horizontal_top(|ui| {
            if theme::option_card(
                ui,
                col,
                theme::OptionCard {
                    badge: "HAC",
                    title: t.mining_hac,
                    sub: t.mining_hac_sub,
                    selected: kind == MiningKind::Hac,
                    enabled: true,
                },
            )
            .clicked()
            {
                self.set_mining_kind(MiningKind::Hac);
            }
            if theme::option_card(
                ui,
                col,
                theme::OptionCard {
                    badge: "HACD",
                    title: t.mining_hacd,
                    sub: t.mining_hacd_sub,
                    selected: kind == MiningKind::Hacd,
                    enabled: true,
                },
            )
            .clicked()
            {
                self.set_mining_kind(MiningKind::Hacd);
            }
        });
    }

    /// The first real decision: run a full node yourself, or send the work
    /// somewhere that already has one.
    ///
    /// A miner-only package has no full node binary, and then the first choice
    /// cannot work. It is drawn greyed out with the reason on the card, because
    /// finding that out from a failed Start is finding it out too late.
    fn connect_mode_cards(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let hacd = self.mining_kind == MiningKind::Hacd;
        let solo = self.connect_mode == ConnectMode::Solo;
        let solo_available = self.fullnode_present;
        let (solo_title, solo_sub, pool_title, pool_sub) = if hacd {
            (
                t.connect_hacd_local_title,
                t.connect_hacd_local_sub,
                t.connect_hacd_remote_title,
                t.connect_hacd_remote_sub,
            )
        } else {
            (
                t.connect_solo_title,
                t.connect_solo_sub,
                t.connect_pool_title,
                t.connect_pool_sub,
            )
        };
        let col = col_width(ui);

        ui.horizontal_top(|ui| {
            let mut response = theme::option_card(
                ui,
                col,
                theme::OptionCard {
                    badge: "",
                    title: solo_title,
                    sub: if solo_available {
                        solo_sub
                    } else {
                        t.connect_solo_unavailable
                    },
                    selected: solo,
                    enabled: solo_available,
                },
            );
            if !solo_available {
                response = response.on_hover_text(t.connect_solo_unavailable_hint);
            }
            if response.clicked() {
                self.set_connect_mode(ConnectMode::Solo);
            }
            if theme::option_card(
                ui,
                col,
                theme::OptionCard {
                    badge: "",
                    title: pool_title,
                    sub: pool_sub,
                    selected: !solo,
                    enabled: true,
                },
            )
            .clicked()
            {
                self.set_connect_mode(ConnectMode::Pool);
            }
        });

        if !solo_available {
            ui.add_space(10.0);
            theme::note(ui, theme::colors::GOLD, t.connect_solo_unavailable_hint);
        }
    }

    /// The pool picker, the address, the reachability test and, in the full
    /// view, the reward address. Shared by both views so they cannot drift.
    fn connect_target_fields(&mut self, ui: &mut egui::Ui, with_wallet: bool) {
        let t = self.t();
        let pool_mode = self.connect_mode == ConnectMode::Pool;
        let hac_pool = pool_mode && self.mining_kind == MiningKind::Hac;
        let col = col_width(ui);

        if pool_mode {
            ui.horizontal_top(|ui| {
                if hac_pool {
                    theme::field_col(ui, col, t.label_pool_directory, |ui, w| {
                        // Clone the directory so the combo closure can call
                        // &mut self (apply/refresh) without aliasing self.
                        let pools = self.pool_directory.clone();
                        let selected_label = pools
                            .get(self.pool_preset_idx)
                            .map(pool_menu_label)
                            .unwrap_or_else(|| t.connect_pool.to_string());
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("pool_preset")
                                .icon(theme::combo_chevron)
                                .selected_text(selected_label)
                                .width((w - 120.0).max(110.0))
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
                    });
                }
                theme::field_col(ui, col, t.label_reachability, |ui, w| {
                    if ui
                        .button(t.btn_test_connection)
                        .on_hover_text("Check the address is reachable from this PC")
                        .clicked()
                    {
                        self.connect_test_status =
                            match crate::connect::probe_reachable(&self.connect, 1500) {
                                Ok(ms) => format!("Reachable ({} ms)", ms),
                                Err(e) => format!("Not reachable: {}", e),
                            };
                    }
                    if !self.connect_test_status.is_empty() {
                        ui.add_space(6.0);
                        let tint = if self.connect_test_status.starts_with("Reachable") {
                            theme::colors::ACCENT
                        } else {
                            theme::colors::RED
                        };
                        theme::readonly_field(ui, w, &self.connect_test_status, tint);
                    }
                });
            });
            ui.add_space(12.0);
        }

        ui.horizontal_top(|ui| {
            // HACD always talks to a full node, local or remote, so calling the
            // remote one "Pool" would be wrong: there is no diamond pool.
            let address_label =
                if self.connect_mode == ConnectMode::Solo || self.mining_kind == MiningKind::Hacd {
                    t.label_fullnode
                } else {
                    t.connect_pool
                };
            theme::field_col(ui, col, address_label, |ui, w| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.connect)
                        .desired_width(w - 24.0)
                        .margin(egui::Margin::symmetric(10.0, 8.0)),
                );
            });
            if with_wallet {
                theme::field_col(ui, col, t.label_wallet, |ui, w| {
                    self.wallet_field(ui, w - 24.0);
                });
            }
        });

        if pool_mode {
            ui.add_space(10.0);
            if hac_pool {
                // Per-pool guidance + link from the directory entry.
                if let Some(p) = self.pool_directory.get(self.pool_preset_idx).cloned() {
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
    }

    /// Step 3: what mining is allowed to cost, and how hot it may get.
    fn profit_fields(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        if self.mining_kind == MiningKind::Hacd {
            ui.label(
                egui::RichText::new("CPU threads × 8 W. GPU power and temperature do not apply.")
                    .size(12.0)
                    .color(theme::colors::TEXT_MUTED),
            );
            return;
        }

        let col = col_width(ui);
        ui.horizontal_top(|ui| {
            theme::field_col(ui, col, t.label_power_cost, |ui, _w| {
                let currency = self.currency;
                theme::power_cost_slider(ui, &mut self.power_cost, currency);
            });
            theme::field_col(ui, col, t.label_hac_price, |ui, _w| {
                ui.add(
                    egui::DragValue::new(&mut self.hac_price)
                        .speed(0.01)
                        .range(0.0..=1_000_000.0)
                        .suffix(" $"),
                );
            });
        });

        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            theme::field_col(ui, col, t.label_max_temp, |ui, _w| {
                ui.add(
                    egui::DragValue::new(&mut self.max_temp_c)
                        .range(0..=95)
                        .suffix(" °C"),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "0 = off. Thermal protection requires a readable GPU sensor.",
                    )
                    .size(11.0)
                    .color(theme::colors::TEXT_MUTED),
                );
            });
            // Derived, not typed: the worker throttles to half the work groups
            // when the card gets too hot, and the panel writes that number.
            theme::field_col(ui, col, t.label_thermal_wg_cap, |ui, w| {
                let cap = (self.work_groups / 2).max(1);
                theme::readonly_field(ui, w, &cap.to_string(), theme::colors::TEXT_MUTED);
            });
        });

        ui.add_space(12.0);
        ui.checkbox(&mut self.pause_unprofitable, t.label_pause_unprofitable);
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(t.profit_fixed_note)
                .size(11.0)
                .color(theme::colors::TEXT_DIM),
        );
    }

    /// Step 5, HACD only: what the node is allowed to bid for a diamond.
    fn bid_fields(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let col = col_width(ui);
        ui.horizontal_top(|ui| {
            theme::field_col(ui, col, t.label_bid_password, |ui, w| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bid_password)
                        .password(true)
                        .desired_width(w - 24.0)
                        .margin(egui::Margin::symmetric(10.0, 8.0)),
                );
            });
            theme::field_col(ui, col, t.label_bid_min, |ui, _w| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bid_min)
                        .desired_width(160.0)
                        .hint_text("1"),
                );
            });
        });
        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            theme::field_col(ui, col, t.label_bid_max, |ui, _w| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bid_max)
                        .desired_width(160.0)
                        .hint_text("31"),
                );
            });
            theme::field_col(ui, col, t.label_bid_step, |ui, _w| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bid_step)
                        .desired_width(160.0)
                        .hint_text("0.5"),
                );
            });
        });
    }

    /// The reward address box plus the hint for the current mining type.
    fn wallet_field(&mut self, ui: &mut egui::Ui, width: f32) {
        let t = self.t();
        ui.add(
            egui::TextEdit::singleline(&mut self.wallet)
                .desired_width(width)
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
            .size(11.0)
            .color(theme::colors::TEXT_MUTED),
        );
    }

    /// The same steps, with everything the panel can decide itself left out.
    fn ui_settings_simple(&mut self, ui: &mut egui::Ui) {
        let t = self.t();

        theme::step_card(
            ui,
            1,
            t.step_mining_type_title,
            "HAC uses your graphics card. HACD (diamonds) runs on the CPU through a full node.",
            |ui| {
                self.mining_kind_cards(ui);
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
                                    theme::colors::ACCENT
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
            t.step_connection_title,
            "A pool pays you small amounts often. Your own node pays only when it finds a whole block.",
            |ui| {
                self.connect_mode_cards(ui);
                ui.add_space(16.0);
                self.connect_target_fields(ui, false);
            },
        );

        theme::step_card(
            ui,
            3,
            "Where should your coins go?",
            "Paste your HAC address. In pool mode this is also the address the pool pays.",
            |ui| {
                let w = col_width(ui);
                self.wallet_field(ui, w);
            },
        );

        // HACD diamond mining also needs the bid account password; without it
        // Start would dead-end. Ask for it here instead of hiding it in Advanced.
        if self.mining_kind == MiningKind::Hacd {
            theme::step_card(
                ui,
                4,
                t.step_bid_title,
                "Diamond mining bids from your full node account. Enter its password. The bid amounts use safe defaults, which you can change under Advanced.",
                |ui| {
                    let w = col_width(ui);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.bid_password)
                            .password(true)
                            .desired_width(w)
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
    /// a shared node, fleet settings and the actions.
    fn ui_settings_advanced_tail(&mut self, ui: &mut egui::Ui) {
        // Everything a different pool might need, editable from the GUI so the
        // user never has to open poworker.config.ini. Defaults suit every pool;
        // a directory entry can also preset these when a pool is selected.
        if self.mining_kind == MiningKind::Hac && self.connect_mode == ConnectMode::Pool {
            theme::section_card().show(ui, |ui| {
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
            });
            ui.add_space(12.0);
        }

        // All-in-one public free-IP pool (hac-pool)
        if self.mining_kind == MiningKind::Hac {
            theme::section_card().show(ui, |ui| {
                theme::card_title(
                    ui,
                    "Shared node / open work relay",
                    "Share this PC's mining work so others can point their miners at YOUR IP:HTTP port \
(local mining can use 127.0.0.1). Requires hac-pool.exe next to the panel.",
                );
                ui.add_space(6.0);
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
                            .add_enabled(
                                self.public_pool_running,
                                egui::Button::new("Stop public pool"),
                            )
                            .clicked()
                        {
                            self.stop_public_pool();
                        }
                        let badge = if self.public_pool_running {
                            ("RUNNING", theme::colors::ACCENT)
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
                                    theme::colors::ACCENT
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
            ui.add_space(12.0);
        }

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
                self.tab = crate::TAB_OVERVIEW;
            }
        });
    }
}
