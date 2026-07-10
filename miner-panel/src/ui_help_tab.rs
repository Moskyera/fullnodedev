//! Help tab UI (getting started and option reference).

use eframe::egui;

use crate::help_options;
use crate::mining_kind::MiningKind;
use crate::theme;
use crate::MinerApp;

impl MinerApp {
    pub(super) fn ui_help(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        ui.label(
            egui::RichText::new(t.help_title)
                .size(16.0)
                .strong()
                .color(theme::colors::TEXT),
        );
        ui.add_space(12.0);

        ui.label(
            egui::RichText::new(t.help_hac_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(8.0);
        theme::help_step(ui, 1, t.help_step1);
        theme::help_step(ui, 2, t.help_step2);
        theme::help_step(ui, 3, t.help_step3);

        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(t.help_hacd_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(8.0);
        theme::help_step(ui, 1, t.help_hacd_step1);
        theme::help_step(ui, 2, t.help_hacd_step2);
        theme::help_step(ui, 3, t.help_hacd_step3);
        theme::help_step(ui, 4, t.help_hacd_step4);
        theme::help_step(ui, 5, t.help_hacd_step5);

        ui.add_space(10.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.wallet_restart_hint)
                    .color(theme::colors::GOLD)
                    .size(13.0),
            );
        });

        ui.add_space(10.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(t.help_hardware_note)
                    .color(theme::colors::TEXT)
                    .size(13.0),
            );
        });

        ui.add_space(12.0);
        theme::section_card().show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    t.help_work_dir_prefix,
                    self.work_dir.display()
                ))
                .color(theme::colors::TEXT_MUTED)
                .size(12.5),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    t.help_miner_prefix,
                    if self.mining_kind == MiningKind::Hacd {
                        self.diaworker_path.display().to_string()
                    } else {
                        self.poworker_path.display().to_string()
                    }
                ))
                .color(theme::colors::TEXT_MUTED)
                .size(12.5),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(t.help_opencl_tip)
                    .color(theme::colors::TEXT_MUTED)
                    .size(12.5),
            );
        });

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(t.help_options_title)
                .size(14.5)
                .strong()
                .color(theme::colors::ACCENT),
        );
        ui.add_space(6.0);
        theme::section_card().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in help_options::option_reference(self.lang) {
                        ui.label(
                            egui::RichText::new(section.title)
                                .strong()
                                .color(theme::colors::TEXT)
                                .size(13.0),
                        );
                        ui.add_space(4.0);
                        for line in section.lines {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("•")
                                        .color(theme::colors::ACCENT)
                                        .size(12.0),
                                );
                                ui.label(
                                    egui::RichText::new(*line)
                                        .color(theme::colors::TEXT_MUTED)
                                        .size(12.0),
                                );
                            });
                        }
                        ui.add_space(10.0);
                    }
                });
        });
        ui.add_space(8.0);
    }
}