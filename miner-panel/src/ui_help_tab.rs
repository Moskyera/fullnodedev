//! The Help screen.
//!
//! The same shapes as the rest of the panel: a page heading, then cards. What
//! used to be one long column of framed sentences is now four cards that answer
//! four different questions, so an operator looking for one of them does not
//! have to read the other three.
//!
//! Nothing here is measured, so nothing here can be stale: it is the panel's own
//! text, plus the two paths this build is actually using, which are read from
//! the running app rather than written down.

use eframe::egui;

use crate::MinerApp;
use crate::dashboard;
use crate::help_options;
use crate::i18n::{PanelLabels, Strings};
use crate::mining_kind::MiningKind;
use crate::theme::colors;

const GAP: f32 = 12.0;
/// Both cards in the top row are this tall, so the row squares off. Five steps
/// plus a heading is what has to fit.
const STEPS_ROW_H: f32 = 288.0;
const NOTES_ROW_H: f32 = 214.0;

/// The step strings were written for a list that printed its own numbers, so
/// they still carry "1. " at the front. The badge draws the number now, and two
/// numbers on one line reads as a typo.
fn strip_step_number(text: &str) -> &str {
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices();
    let Some((_, first)) = chars.next() else {
        return trimmed;
    };
    if !first.is_ascii_digit() {
        return trimmed;
    }
    match chars.next() {
        Some((idx, '.')) => trimmed[idx + 1..].trim_start(),
        _ => trimmed,
    }
}

fn font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Proportional)
}

/// `dashboard::card` hands its content a `Ui` with the caller's layout, so a
/// card placed in a row of cards lays its own content out sideways. Every card
/// on this screen is a column of paragraphs, so each one starts a fresh
/// top-down layout and clamps itself to the width the row gave it.
fn card_column(
    ui: &mut egui::Ui,
    width: f32,
    min_height: f32,
    content: impl FnOnce(&mut egui::Ui),
) {
    dashboard::card(ui, width, min_height, |ui| {
        let inner = (width - 36.0).max(1.0);
        // `with_layout` and not `allocate_ui_with_layout`: the latter would need
        // a height, and a height of zero leaves a scroll area inside the card
        // with nothing to scroll in.
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(inner);
            content(ui);
        });
    });
}

/// Windows hands out verbatim paths, and `\\?\C:\...` in front of a folder
/// name reads as corruption to everyone who has not met the prefix before.
fn readable_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix("\\\\?\\UNC\\") {
        Some(rest) => format!("\\\\{rest}"),
        None => text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string(),
    }
}

/// One numbered step: a small amber-outlined badge and the sentence beside it.
fn step_line(ui: &mut egui::Ui, num: u8, text: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(22.0), egui::Sense::hover());
        {
            let painter = ui.painter();
            let rounding = egui::Rounding::same(7.0);
            painter.rect_filled(rect, rounding, colors::NAV_ACTIVE_BG);
            painter.rect_stroke(
                rect,
                rounding,
                egui::Stroke::new(1.0, colors::BORDER_ACCENT),
            );
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                num.to_string(),
                font(11.0),
                colors::ACCENT,
            );
        }
        ui.label(
            egui::RichText::new(strip_step_number(text))
                .size(12.0)
                .color(colors::TEXT_MUTED),
        );
    });
    ui.add_space(9.0);
}

/// A paragraph with a coloured bar down its left edge, as tall as the paragraph
/// turned out to be.
fn note_line(ui: &mut egui::Ui, accent: egui::Color32, text: &str) {
    let full = ui.available_width();
    let text_w = (full - 13.0).max(60.0);
    let galley = ui.fonts(|f| f.layout(text.to_owned(), font(12.0), colors::TEXT_MUTED, text_w));
    let height = galley.rect.height();
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(full, height), egui::Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, egui::Vec2::new(3.0, height)),
        egui::Rounding::same(2.0),
        accent,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 13.0, rect.top()),
        galley,
        colors::TEXT_MUTED,
    );
    ui.add_space(11.0);
}

/// A path this build is really using: what it is, then where it is.
fn path_line(ui: &mut egui::Ui, label: &str, value: &str) {
    let label = label.trim().trim_end_matches([':', '\u{ff1a}']).trim();
    ui.label(
        egui::RichText::new(label.to_uppercase())
            .size(10.0)
            .color(colors::TEXT_DIM),
    );
    ui.add_space(3.0);
    ui.label(egui::RichText::new(value).size(11.5).color(colors::TEXT));
    ui.add_space(11.0);
}

fn bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 9.0;
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(6.0, 15.0), egui::Sense::hover());
        ui.painter().circle_filled(
            egui::pos2(rect.center().x, rect.top() + 7.5),
            2.0,
            colors::GOLD_DIM,
        );
        ui.label(
            egui::RichText::new(text)
                .size(11.5)
                .color(colors::TEXT_MUTED),
        );
    });
    ui.add_space(2.0);
}

impl MinerApp {
    pub(super) fn ui_help(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let l = crate::i18n::panel_labels(self.lang);

        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(t.tab_help)
                    .size(25.0)
                    .strong()
                    .color(colors::TEXT),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(l.help_sub)
                    .size(12.0)
                    .color(colors::TEXT_MUTED),
            );
        });
        ui.add_space(16.0);

        let total = ui.available_width();
        let half = ((total - GAP) * 0.5).floor().max(140.0);

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            card_column(ui, half, STEPS_ROW_H, |ui| {
                let sub = t
                    .help_title
                    .trim()
                    .trim_end_matches([':', '\u{ff1a}'])
                    .trim();
                dashboard::card_head(ui, t.help_hac_title, sub, |_ui| {});
                ui.add_space(13.0);
                step_line(ui, 1, t.help_step1);
                step_line(ui, 2, t.help_step2);
                step_line(ui, 3, t.help_step3);
            });
            card_column(ui, half, STEPS_ROW_H, |ui| {
                dashboard::card_head(ui, t.help_hacd_title, l.help_hacd_cpu_note, |_ui| {});
                ui.add_space(13.0);
                step_line(ui, 1, t.help_hacd_step1);
                step_line(ui, 2, t.help_hacd_step2);
                step_line(ui, 3, t.help_hacd_step3);
                step_line(ui, 4, t.help_hacd_step4);
                step_line(ui, 5, t.help_hacd_step5);
            });
        });

        ui.add_space(GAP);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            card_column(ui, half, NOTES_ROW_H, |ui| {
                self.help_notes(ui, &t, &l);
            });
            card_column(ui, half, NOTES_ROW_H, |ui| {
                self.help_paths(ui, &t, &l);
            });
        });

        ui.add_space(GAP);
        card_column(ui, total, 0.0, |ui| {
            dashboard::card_head(ui, t.help_options_title, l.help_reference_sub, |_ui| {});
            ui.add_space(13.0);
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for section in help_options::option_reference(self.lang) {
                        ui.label(
                            egui::RichText::new(section.title)
                                .size(12.5)
                                .strong()
                                .color(colors::TEXT),
                        );
                        ui.add_space(5.0);
                        for line in section.lines {
                            bullet(ui, line);
                        }
                        ui.add_space(13.0);
                    }
                });
        });
        ui.add_space(4.0);
    }

    /// The three things worth reading once, in the order they bite: the one
    /// that costs a restart, the one that decides which hardware is used, and
    /// the one that explains a GPU the panel cannot see.
    fn help_notes(&self, ui: &mut egui::Ui, t: &Strings, l: &PanelLabels) {
        dashboard::card_head(ui, l.help_notes_title, l.help_notes_sub, |_ui| {});
        ui.add_space(13.0);
        note_line(ui, colors::ACCENT, t.wallet_restart_hint);
        note_line(ui, colors::BORDER, t.help_hardware_note);
        note_line(
            ui,
            colors::BORDER,
            if self.mining_kind == MiningKind::Hacd {
                l.help_hacd_cpu_note
            } else {
                t.help_opencl_tip
            },
        );
    }

    /// The two paths this build is really using. Read from the running app, so
    /// a panel started from an unexpected folder says so instead of printing
    /// the folder it was supposed to be in.
    fn help_paths(&self, ui: &mut egui::Ui, t: &Strings, l: &PanelLabels) {
        dashboard::card_head(ui, l.help_paths_title, l.help_paths_sub, |_ui| {});
        ui.add_space(13.0);
        path_line(ui, t.help_work_dir_prefix, &readable_path(&self.work_dir));
        let worker = if self.mining_kind == MiningKind::Hacd {
            readable_path(&self.diaworker_path)
        } else {
            readable_path(&self.poworker_path)
        };
        path_line(ui, t.help_miner_prefix, &worker);
    }
}

#[cfg(test)]
mod tests {
    use super::{readable_path, strip_step_number};
    use std::path::Path;

    #[test]
    fn a_verbatim_windows_path_is_shown_the_way_a_person_would_type_it() {
        assert_eq!(
            readable_path(Path::new(r"\\?\C:\miner\poworker.exe")),
            r"C:\miner\poworker.exe"
        );
        assert_eq!(
            readable_path(Path::new(r"\\?\UNC\rig\share\poworker.exe")),
            r"\\rig\share\poworker.exe"
        );
        // A path that was never verbatim must come back untouched.
        assert_eq!(readable_path(Path::new(r"C:\miner")), r"C:\miner");
    }

    #[test]
    fn a_leading_list_number_is_dropped_once_the_badge_draws_it() {
        assert_eq!(
            strip_step_number("1. Run the fullnode (hacash.exe)."),
            "Run the fullnode (hacash.exe)."
        );
        assert_eq!(strip_step_number("  2.  Open this app"), "Open this app");
    }

    #[test]
    fn a_sentence_that_never_carried_a_number_is_left_alone() {
        // Every language writes its own steps, and not all of them number them.
        assert_eq!(strip_step_number("Run the fullnode"), "Run the fullnode");
        assert_eq!(strip_step_number("3 steps"), "3 steps");
        assert_eq!(strip_step_number(""), "");
        // A leading digit that is part of the sentence must survive.
        assert_eq!(strip_step_number("8081 is the port"), "8081 is the port");
    }
}
