//! GPU/CPU/mode comboboxes and RDNA4 badge (settings tab hardware section).

use eframe::egui;

use crate::presets;
use crate::theme;
use crate::MinerApp;

impl MinerApp {
    pub(super) fn show_hw_tuning_section(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let cpu_idx = self.cpu_idx;
        let gpu_idx = self.gpu_idx;
        let mode_idx = self.mode_idx;

        theme::section_card().show(ui, |ui| {
            egui::Grid::new("hw_grid")
                .num_columns(2)
                .spacing([20.0, 12.0])
                .show(ui, |ui| {
                    theme::field_label(ui, t.label_cpu);
                    egui::ComboBox::from_id_salt("cpu")
                        .selected_text(self.cpu_label(cpu_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            for (i, _) in self.cpu_presets.iter().enumerate() {
                                let label = if i + 1 == self.cpu_presets.len() {
                                    t.cpu_only
                                } else {
                                    self.cpu_presets[i].label
                                };
                                ui.selectable_value(&mut self.cpu_idx, i, label);
                            }
                        });
                    ui.end_row();

                    theme::field_label(ui, t.label_gpu);
                    let gpu_before = gpu_idx;
                    egui::ComboBox::from_id_salt("gpu")
                        .selected_text(self.gpu_label(gpu_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            for (i, _) in self.gpu_presets.iter().enumerate() {
                                let label = if i + 1 == self.gpu_presets.len() {
                                    t.no_gpu
                                } else {
                                    self.gpu_presets[i].label
                                };
                                ui.selectable_value(&mut self.gpu_idx, i, label);
                            }
                        });
                    if self.gpu_idx != gpu_before {
                        self.apply_panel_tuning();
                    }
                    ui.end_row();

                    if presets::is_rdna4_experimental(&self.gpu_presets[self.gpu_idx].slug) {
                        ui.label("");
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(t.gpu_rdna4_badge)
                                    .color(theme::colors::GOLD)
                                    .strong()
                                    .size(12.5),
                            );
                            ui.label(
                                egui::RichText::new(t.gpu_rdna4_hint)
                                    .color(theme::colors::TEXT_MUTED)
                                    .size(11.5),
                            );
                        });
                        ui.end_row();
                    }

                    theme::field_label(ui, t.label_mode);
                    let mode_before = mode_idx;
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(self.mode_label(mode_idx))
                        .width(400.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.mode_idx, 0, t.mode_eco);
                            ui.selectable_value(&mut self.mode_idx, 1, t.mode_profit);
                            ui.selectable_value(&mut self.mode_idx, 2, t.mode_max);
                        });
                    if self.mode_idx != mode_before {
                        self.apply_panel_tuning();
                    }
                    ui.end_row();
                });
        });
    }
}