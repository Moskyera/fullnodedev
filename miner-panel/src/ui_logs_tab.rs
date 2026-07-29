//! The Logs screen.
//!
//! Mockup 1 puts "Logs" in the sidebar. This is the screen behind it, and it
//! shows exactly one thing: the lines the worker printed, in the worker's own
//! words, with the worker's own timestamp where the worker wrote one.
//!
//! Nothing on this screen is derived. There is no level filter, because the
//! workers do not print levels; there is no "cleared" or "archived" state,
//! because the panel does not own the file; and a line that arrived on a pipe
//! rather than out of the timestamped file is shown with an empty time column
//! rather than the moment the panel happened to read it. The panel's clock is
//! not a measurement of when the worker spoke.
//!
//! What the screen does claim, in words on the page: it holds the newest lines
//! only, and the whole run is in the file whose path it prints. Without that
//! sentence a short list reads as a quiet worker instead of a bounded ring.

use eframe::egui;

use crate::MinerApp;
use crate::dashboard::{self, EventKind};
use crate::i18n::LogLabels;
use crate::theme::colors;
use crate::worker_log_tail::{self, Line};

/// Height of one line row. Fixed, so the list can be virtualised: only the rows
/// actually on screen are laid out, whatever the ring is holding.
const ROW_H: f32 = 19.0;
/// Tallest the list gets before it scrolls inside itself.
const LIST_H: f32 = 520.0;
/// Width of the timestamp column, sized for `2026-07-29 14:03:11` in the
/// monospace face at `STAMP_SIZE`.
const STAMP_W: f32 = 124.0;
const STAMP_SIZE: f32 = 10.5;
const TEXT_SIZE: f32 = 12.0;

/// Whether the time column is drawn at all.
///
/// It is drawn when at least one line carries a time the worker wrote. On a
/// platform whose workers are piped rather than tailed no line carries one, and
/// a 124px column of nothing in front of every row looks like a rendering fault
/// rather than the absence of a measurement.
fn stamp_column_width(lines: &[Line]) -> f32 {
    if lines.iter().any(|line| !line.stamp.is_empty()) {
        STAMP_W
    } else {
        0.0
    }
}

/// The colour a line is drawn in.
///
/// It reuses the dashboard's classifier, which matches only strings the workers
/// in this repository really print. A line it does not recognise is drawn in the
/// ordinary body colour: unrecognised is not the same as unimportant, and
/// guessing a severity would be inventing one.
fn line_tint(text: &str) -> egui::Color32 {
    match dashboard::classify(text) {
        Some(EventKind::Failure | EventKind::GpuFailure) => colors::RED,
        Some(EventKind::Success) => colors::ACCENT,
        _ => colors::TEXT_MUTED,
    }
}

impl MinerApp {
    pub(super) fn ui_logs(&mut self, ui: &mut egui::Ui) {
        let l = crate::i18n::log_labels(self.lang);
        let lines = worker_log_tail::recent(worker_log_tail::MAX_LINES);

        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(l.title)
                    .size(25.0)
                    .strong()
                    .color(colors::TEXT),
            );
            ui.add_space(2.0);
            ui.label(egui::RichText::new(l.sub).size(12.0).color(colors::TEXT_MUTED));
        });
        ui.add_space(16.0);

        let width = ui.available_width();
        dashboard::card(ui, width, 0.0, |ui| {
            let inner = (width - 36.0).max(1.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(inner);
                self.logs_card(ui, &l, &lines);
            });
        });
    }

    fn logs_card(&self, ui: &mut egui::Ui, l: &LogLabels, lines: &[Line]) {
        // The card head carries the file rather than repeating the page
        // heading. The path is the one the panel computed when it launched the
        // worker, not a guess about where a log might live, so it is absent
        // until a worker has actually been started.
        let source = worker_log_tail::source_path();
        let subtitle = match &source {
            Some(path) => readable_path(path),
            None => l.file_unknown.to_string(),
        };
        dashboard::card_head(ui, l.file_label, &subtitle, |ui| {
            dashboard::chip(
                ui,
                &l.kept_display(lines.len(), worker_log_tail::MAX_LINES),
                None,
                false,
            );
        });
        ui.add_space(12.0);

        if lines.is_empty() {
            ui.label(
                egui::RichText::new(l.empty)
                    .size(12.5)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(l.empty_hint)
                    .size(11.5)
                    .color(colors::TEXT_DIM),
            );
            return;
        }

        let stamp_w = stamp_column_width(lines);
        // One row per line, never two: a wrapped line would break the fixed row
        // height the virtualised list is built on. The ring already clamps every
        // line to 400 characters, so nothing is lost that was not already cut.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        egui::ScrollArea::vertical()
            .id_salt("worker_log_lines")
            // Full width, but only as tall as there are lines: a worker that
            // has printed three of them must not leave a screen of empty box
            // under them looking like output that failed to arrive.
            .auto_shrink([false, true])
            .max_height(LIST_H)
            .show_rows(ui, ROW_H, lines.len(), |ui, range| {
                for line in &lines[range] {
                    log_row(ui, line, stamp_w);
                }
            });

        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(l.full_history)
                .size(11.0)
                .color(colors::TEXT_DIM),
        );
    }
}

/// One line: the worker's time, then the worker's words.
fn log_row(ui: &mut egui::Ui, line: &Line, stamp_w: f32) {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.spacing_mut().item_spacing.x = 8.0;
        if stamp_w > 0.0 {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(stamp_w, ROW_H), egui::Sense::hover());
            if !line.stamp.is_empty() {
                ui.painter().text(
                    egui::pos2(rect.left(), rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &line.stamp,
                    egui::FontId::new(STAMP_SIZE, egui::FontFamily::Monospace),
                    colors::TEXT_DIM,
                );
            }
        }
        ui.label(
            egui::RichText::new(&line.text)
                .size(TEXT_SIZE)
                .color(line_tint(&line.text)),
        );
    });
}

/// Windows hands out verbatim paths, and `\\?\C:\...` in front of a folder name
/// reads as corruption to everyone who has not met the prefix before.
fn readable_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix("\\\\?\\UNC\\") {
        Some(rest) => format!("\\\\{rest}"),
        None => text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(stamp: &str, text: &str) -> Line {
        Line {
            stamp: stamp.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn the_time_column_appears_only_when_a_time_was_measured() {
        // Tailed lines carry the worker's own stamp.
        let tailed = [line("2026-07-29 14:03:11", "[Mining] hashing")];
        assert_eq!(stamp_column_width(&tailed), STAMP_W);

        // Piped lines carry none, and the panel does not put its clock on them.
        let piped = [line("", "[Mining] hashing"), line("", "[Mining] more")];
        assert_eq!(stamp_column_width(&piped), 0.0);

        // One stamped line among many is enough to line the rest up under it.
        let mixed = [line("", "banner"), line("2026-07-29 14:03:11", "[Mining] x")];
        assert_eq!(stamp_column_width(&mixed), STAMP_W);

        assert_eq!(stamp_column_width(&[]), 0.0);
    }

    #[test]
    fn a_failure_is_the_only_thing_drawn_in_red() {
        assert_eq!(line_tint("Error: node rejected the block"), colors::RED);
        assert_eq!(line_tint("OpenCL error -4 on device 0"), colors::RED);
        assert_eq!(line_tint("[MINING SUCCESS] height 765432"), colors::ACCENT);
        // Ordinary chatter, and anything the classifier does not know, is body
        // text. An unrecognised line is not a quiet line.
        assert_eq!(line_tint("[Mining] hashrate 4.82 GH/s"), colors::TEXT_MUTED);
        assert_eq!(line_tint("some line nothing matches"), colors::TEXT_MUTED);
    }

    #[test]
    fn the_whole_ring_fits_on_the_screen_that_shows_it() {
        // The screen asks for every line the ring can hold. If it asked for
        // fewer, the count in the chip would disagree with the list under it.
        assert_eq!(
            worker_log_tail::MAX_LINES,
            500,
            "the chip prints this ceiling, so a change here is a change on screen"
        );
    }

    #[test]
    fn a_verbatim_windows_path_is_shown_the_way_it_is_typed() {
        assert_eq!(
            readable_path(std::path::Path::new("\\\\?\\C:\\miner\\poworker.log")),
            "C:\\miner\\poworker.log"
        );
        assert_eq!(
            readable_path(std::path::Path::new("/miner/poworker.log")),
            "/miner/poworker.log"
        );
    }
}
