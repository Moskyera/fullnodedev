//! Step 2 of the setup screen: the hardware pickers, the backend, the
//! efficiency mode and Auto Tune.
//!
//! These draw fields only, no card of their own: the caller wraps them in the
//! numbered step card so the whole screen reads as one sequence.

use eframe::egui;

use crate::MinerApp;
use crate::OpenClAction;
use crate::mining_kind::MiningKind;
use crate::presets;
use crate::theme;

impl MinerApp {
    pub(super) fn hardware_fields(&mut self, ui: &mut egui::Ui) {
        let t = self.t();

        if self.mining_kind == MiningKind::Hacd {
            let col = crate::ui_settings_tab::col_width(ui);
            theme::field_col(ui, col, "CPU mining threads:", |ui, w| {
                // Every label already carries its own thread count, so the
                // count is not appended again here.
                let selected = self.cpu_presets[self.cpu_idx].label.clone();
                egui::ComboBox::from_id_salt("hacd_cpu")
                    .icon(theme::combo_chevron)
                    .selected_text(selected)
                    .width(w - 24.0)
                    .show_ui(ui, |ui| {
                        for (i, preset) in self.cpu_presets.iter().enumerate() {
                            if preset.supervene > 0 {
                                ui.selectable_value(&mut self.cpu_idx, i, preset.label.clone());
                            }
                        }
                    });
            });
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "OpenCL, GPU profiles and Auto Tune are intentionally disabled for HACD.",
                )
                .size(11.5)
                .color(theme::colors::TEXT_MUTED),
            );
            return;
        }

        let col = crate::ui_settings_tab::col_width(ui);
        let gpu_idx = self.gpu_idx;
        let cpu_idx = self.cpu_idx;

        ui.horizontal_top(|ui| {
            theme::field_col(ui, col, t.label_gpu, |ui, w| {
                let gpu_before = gpu_idx;
                egui::ComboBox::from_id_salt("gpu")
                    .icon(theme::combo_chevron)
                    .selected_text(self.gpu_label(gpu_idx))
                    .width(w - 24.0)
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
            });
            theme::field_col(ui, col, t.label_cpu, |ui, w| {
                egui::ComboBox::from_id_salt("cpu")
                    .icon(theme::combo_chevron)
                    .selected_text(self.cpu_label(cpu_idx))
                    .width(w - 24.0)
                    .show_ui(ui, |ui| {
                        for (i, preset) in self.cpu_presets.iter().enumerate() {
                            ui.selectable_value(&mut self.cpu_idx, i, preset.label.clone());
                        }
                    });
            });
        });

        ui.add_space(10.0);
        ui.horizontal_top(|ui| {
            // The backend picker is NVIDIA only: CUDA needs a miner built with
            // `--features cuda`, and OpenCL is the only backend anyone else has.
            if presets::profile_is_nvidia(self.gpu_presets[self.gpu_idx].profile) {
                theme::field_col(ui, col, t.label_backend, |ui, w| {
                    egui::ComboBox::from_id_salt("backend")
                        .icon(theme::combo_chevron)
                        .selected_text(if self.use_cuda { "CUDA" } else { "OpenCL" })
                        .width(w - 24.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.use_cuda, false, "OpenCL");
                            ui.selectable_value(&mut self.use_cuda, true, "CUDA");
                        });
                });
            }
            theme::field_col(ui, col, t.label_opencl, |ui, _w| {
                let usable = self.opencl_status.has_usable_device();
                ui.label(
                    egui::RichText::new(self.opencl_status.device_summary())
                        .color(if usable {
                            theme::colors::ACCENT
                        } else {
                            theme::colors::RED
                        })
                        .size(13.0),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button("Auto-detect OpenCL GPU").clicked() {
                        self.request_opencl_probe(OpenClAction::AutoDetect);
                    }
                });
                ui.collapsing("Advanced OpenCL IDs", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.platform_id).range(0..=32));
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
        });

        for w in &self.opencl_status.warnings {
            ui.label(
                egui::RichText::new(format!("! {w}"))
                    .color(theme::colors::GOLD)
                    .size(11.5),
            );
        }

        ui.add_space(14.0);
        theme::field_caption(ui, t.label_mode);
        ui.add_space(5.0);
        if let Some(idx) =
            theme::segmented_wide(ui, &[t.mode_eco, t.mode_profit, t.mode_max], self.mode_idx)
        {
            if idx != self.mode_idx {
                self.mode_idx = idx;
                self.apply_panel_tuning();
            }
        }
        // A mode that cannot differ must not silently pretend to. Said here,
        // where the choice is made, and not only in a worker log line nobody
        // reads: with no power reading every candidate is divided by the same
        // configured constant, so Eco and Profit rank exactly as Max does.
        //
        // Only when a probe has actually answered. Before that `power_measured`
        // is `None` and the panel says nothing rather than guessing.
        if self.selected_gpu_power_measured() == Some(false) {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("! {}", t.mode_no_power_sensor))
                    .color(theme::colors::GOLD)
                    .size(11.5),
            );
        }

        ui.add_space(14.0);
        self.autotune_card(ui);
    }

    /// Auto Tune, in the tinted inner card the mockup gives it: what it does on
    /// the left, the one button on the right.
    ///
    /// On CUDA the button is disabled and the card says why. The tuner measures
    /// OpenCL launch shapes only, so with CUDA selected pressing it wrote a
    /// config poworker refuses, waited, failed, and rolled the settings back
    /// without CUDA ever being mentioned.
    fn autotune_card(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let cuda = self.cuda_backend_selected();
        theme::section_card_active().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    theme::card_title(ui, t.autotune_title, t.autotune_hint);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clicked = ui
                        .add_enabled_ui(!cuda, |ui| theme::btn_primary(ui, t.btn_run_autotune))
                        .inner
                        .clicked();
                    if clicked {
                        self.run_benchmark();
                    }
                    if self.benchmarking {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(t.benchmark_running)
                                .color(theme::colors::GOLD)
                                .size(11.5),
                        );
                        ui.spinner();
                    }
                });
            });
            if cuda {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("! {}", t.autotune_cuda_unsupported))
                        .color(theme::colors::GOLD)
                        .size(11.5),
                );
            }
        });
    }
}
