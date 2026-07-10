//! Settings tab UI (mining type, tuning, network, wallet).

use eframe::egui;

use crate::connect::{pool_presets, ConnectMode};
use crate::mining_kind::{save_mining_kind, MiningKind};
use crate::theme;
use crate::MinerApp;

impl MinerApp {
    pub(super) fn ui_settings(&mut self, ui: &mut egui::Ui) {
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
}