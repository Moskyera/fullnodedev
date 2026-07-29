//! The dashboard: what this panel has actually measured, and the painting that
//! puts it on the screen.
//!
//! Three things live here.
//!
//! 1. `Live`, the small amount of history the panel keeps for itself. The
//!    worker writes one snapshot at a time and remembers nothing, so a chart of
//!    the last hour has to be accumulated on this side. Every sample carries the
//!    worker's own `updated_unix_ms`, not the wall clock at the moment the
//!    dashboard happened to redraw, so a stalled worker leaves a visible gap
//!    instead of a flat line that looks like a steady zero.
//! 2. The custom painting: the temperature-style ring, the sparkline and the
//!    area chart. egui has no widget for any of these, and a row of progress
//!    bars would not be the same screen.
//! 3. The payout rules, which decide what the panel is allowed to claim about
//!    money it has not been told about.
//!
//! The rule the whole file follows: a number that looks live and is not is
//! worse than a gap. Where the mockup shows a figure this panel cannot know,
//! the space is filled with the truth about why, not with a plausible number.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, MutexGuard};

use app::efficiency::MiningStatsSnapshot;
use eframe::egui::{
    self, Color32, FontFamily, FontId, Frame, Margin, Pos2, Rect, Rounding, Sense, Stroke, Ui, Vec2,
};
use field::Amount;

use crate::i18n::{DashLabels, Strings};
use crate::stats_poll::{
    PayoutView, PoolEarnings, PoolMoney, PoolTerms, format_age_secs, now_unix_ms,
};
use crate::theme;
use crate::theme::colors;

// ---------------------------------------------------------------------------
// 1. The history this panel keeps for itself.
// ---------------------------------------------------------------------------

/// Samples closer together than this are dropped. The stats file is re-read
/// about three times a second; keeping all of it would be 250,000 points for a
/// day and would tell you nothing a five second grid does not.
const SAMPLE_SPACING_MS: u64 = 5_000;
/// The longest window the chart offers, so nothing older is worth keeping.
const HISTORY_SPAN_MS: u64 = 24 * 60 * 60 * 1_000;
/// Hard ceiling on the buffer regardless of timestamps, so a worker with a
/// broken clock cannot make the panel grow without bound.
const MAX_SAMPLES: usize = 24_000;
/// A hole longer than this breaks the line instead of being drawn across. The
/// panel did not measure anything in that hole and must not imply it did.
const GAP_MS: u64 = 60_000;
/// How many worker lines the event feed keeps.
const MAX_EVENTS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub unix_ms: u64,
    pub hps: f32,
}

/// A bounded time series of hash-rate readings.
#[derive(Default)]
pub struct Series {
    samples: VecDeque<Sample>,
}

/// What a window of the series adds up to. `None` from `summary` means the
/// window is empty, which is a different statement from "zero hashes".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub avg: f64,
    pub peak: f64,
    pub low: f64,
    pub oldest_ms: u64,
    pub newest_ms: u64,
}

impl Series {
    /// Take one reading. Returns whether it was kept.
    ///
    /// A reading is refused when it is not newer than the last one held. The
    /// worker rewrites the same snapshot until it has something new to say, and
    /// re-recording it would invent a measurement that never happened.
    pub fn record(&mut self, unix_ms: u64, hps: f64) -> bool {
        if unix_ms == 0 || !hps.is_finite() || hps < 0.0 {
            return false;
        }
        if let Some(last) = self.samples.back() {
            if unix_ms <= last.unix_ms || unix_ms - last.unix_ms < SAMPLE_SPACING_MS {
                return false;
            }
        }
        self.samples.push_back(Sample {
            unix_ms,
            hps: hps as f32,
        });
        let cutoff = unix_ms.saturating_sub(HISTORY_SPAN_MS);
        while self.samples.front().is_some_and(|s| s.unix_ms < cutoff) {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
        true
    }

    /// Kept for the tests, which assert the trimming rules from the outside.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[allow(dead_code)]
    pub fn latest(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    /// Every sample inside `span_ms` of `now_ms`, oldest first.
    pub fn window(&self, now_ms: u64, span_ms: u64) -> Vec<Sample> {
        let from = now_ms.saturating_sub(span_ms);
        self.samples
            .iter()
            .filter(|s| s.unix_ms >= from && s.unix_ms <= now_ms)
            .copied()
            .collect()
    }

    pub fn summary(&self, now_ms: u64, span_ms: u64) -> Option<Summary> {
        let window = self.window(now_ms, span_ms);
        let first = window.first()?;
        let last = window.last()?;
        let mut peak = f64::MIN;
        let mut low = f64::MAX;
        let mut total = 0.0f64;
        for s in &window {
            let v = s.hps as f64;
            total += v;
            peak = peak.max(v);
            low = low.min(v);
        }
        Some(Summary {
            count: window.len(),
            avg: total / window.len() as f64,
            peak,
            low,
            oldest_ms: first.unix_ms,
            newest_ms: last.unix_ms,
        })
    }

    /// The window split at every hole longer than `GAP_MS`. Each run is drawn
    /// as its own polyline so an outage reads as an outage.
    pub fn segments(&self, now_ms: u64, span_ms: u64) -> Vec<Vec<Sample>> {
        let mut runs: Vec<Vec<Sample>> = Vec::new();
        let mut run: Vec<Sample> = Vec::new();
        for s in self.window(now_ms, span_ms) {
            if let Some(prev) = run.last() {
                if s.unix_ms - prev.unix_ms > GAP_MS {
                    runs.push(std::mem::take(&mut run));
                }
            }
            run.push(s);
        }
        if !run.is_empty() {
            runs.push(run);
        }
        runs
    }
}

/// The three spans the chart offers, exactly as the mockup's selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartWindow {
    H1,
    H6,
    H24,
}

impl ChartWindow {
    pub const ALL: [ChartWindow; 3] = [ChartWindow::H1, ChartWindow::H6, ChartWindow::H24];

    pub fn label(self) -> &'static str {
        match self {
            ChartWindow::H1 => "1H",
            ChartWindow::H6 => "6H",
            ChartWindow::H24 => "24H",
        }
    }

    pub fn span_ms(self) -> u64 {
        match self {
            ChartWindow::H1 => 60 * 60 * 1_000,
            ChartWindow::H6 => 6 * 60 * 60 * 1_000,
            ChartWindow::H24 => 24 * 60 * 60 * 1_000,
        }
    }

    /// Tick offsets from the left edge of the window, in minutes back from now.
    pub fn ticks(self) -> [u64; 5] {
        let span_min = self.span_ms() / 60_000;
        [span_min, span_min * 3 / 4, span_min / 2, span_min / 4, 0]
    }
}

// ---------------------------------------------------------------------------
// Worker events: the lines the worker really printed, classified.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A block this miner found was accepted.
    Success,
    /// A submission or a device failed.
    Failure,
    /// The GPU specifically failed. Counted separately because the mockup's
    /// "GPU errors" box is about the card, not about the network.
    GpuFailure,
    /// Hashing stopped on purpose.
    Paused,
    /// Hashing resumed.
    Resumed,
    /// Auto Tune, configuration echoes and other structured worker chatter.
    Notice,
}

impl EventKind {
    /// The glyph in the round badge at the left of an event row.
    pub fn badge(self) -> &'static str {
        match self {
            EventKind::Success => "+",
            EventKind::Failure | EventKind::GpuFailure => "!",
            EventKind::Paused => "||",
            EventKind::Resumed => ">",
            EventKind::Notice => "i",
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            EventKind::Failure | EventKind::GpuFailure => colors::RED,
            EventKind::Success | EventKind::Resumed => colors::ACCENT,
            _ => colors::TEXT_MUTED,
        }
    }

    pub fn is_gpu_failure(self) -> bool {
        self == EventKind::GpuFailure
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkerEvent {
    pub unix_ms: u64,
    pub kind: EventKind,
    /// The worker's own words, trimmed to one printable line.
    pub detail: String,
}

/// Decide what a worker line is, or that it is not an event at all.
///
/// Every pattern here is a string the workers in this repository really print.
/// Nothing is matched speculatively: a line the panel does not recognise is
/// dropped rather than filed under a guessed heading.
pub fn classify(line: &str) -> Option<EventKind> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // The workers frame important output with box-drawing rules. Those carry no
    // information of their own.
    if !trimmed.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if trimmed.contains("[MINING SUCCESS]") {
        return Some(EventKind::Success);
    }
    if trimmed.contains("OpenCL error")
        || trimmed.contains("GPU batch failed")
        || trimmed.contains("CL_OUT_OF")
    {
        return Some(EventKind::GpuFailure);
    }
    if trimmed.contains("[MINING SUBMIT FAILED]")
        || trimmed.contains("node rejected")
        || trimmed.starts_with("Error:")
        || trimmed.contains("[Fatal]")
    {
        return Some(EventKind::Failure);
    }
    if trimmed.contains("PAUSED") {
        return Some(EventKind::Paused);
    }
    if trimmed.contains("Fresh work received") {
        return Some(EventKind::Resumed);
    }
    if trimmed.starts_with('[') {
        return Some(EventKind::Notice);
    }
    None
}

/// One printable line, bounded, so a worker cannot push a megabyte into the UI.
fn one_line(text: &str, max: usize) -> String {
    let cleaned: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    cleaned.chars().take(max).collect::<String>() + "..."
}

#[derive(Default)]
pub struct EventLog {
    events: VecDeque<WorkerEvent>,
    gpu_errors: u32,
}

impl EventLog {
    /// Record one worker line. Returns whether it became an event.
    pub fn push(&mut self, unix_ms: u64, line: &str) -> bool {
        let Some(kind) = classify(line) else {
            return false;
        };
        let detail = one_line(line, 120);
        // A worker that repeats itself gets one row, not forty.
        if self.events.back().is_some_and(|e| e.detail == detail) {
            return false;
        }
        if kind.is_gpu_failure() {
            self.gpu_errors = self.gpu_errors.saturating_add(1);
        }
        self.events.push_back(WorkerEvent {
            unix_ms,
            kind,
            detail,
        });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        true
    }

    /// The newest `count` events, newest first.
    pub fn recent(&self, count: usize) -> Vec<&WorkerEvent> {
        self.events.iter().rev().take(count).collect()
    }

    /// GPU failures seen since this panel started. Not since the worker
    /// started: the panel is the only thing counting, so that is the only
    /// window it can honestly claim.
    pub fn gpu_errors(&self) -> u32 {
        self.gpu_errors
    }
}

/// Everything the dashboard accumulates that the rest of the panel does not.
///
/// It is a process-wide singleton rather than a field on `MinerApp` because the
/// worker log is drained in the poller and the chart is drawn in the view, and
/// there is exactly one panel window per process.
#[derive(Default)]
pub struct Live {
    pub series: Series,
    pub events: EventLog,
    pub window: ChartWindow,
    /// Whether the Mining Control card is showing the raw worker log.
    pub show_log: bool,
}

impl Default for ChartWindow {
    fn default() -> Self {
        ChartWindow::H1
    }
}

impl Live {
    /// Fold one worker snapshot into the history. Called once per frame; the
    /// spacing and freshness rules inside `Series::record` decide what is kept.
    pub fn observe(&mut self, stats: &MiningStatsSnapshot) {
        self.series
            .record(stats.updated_unix_ms, stats.hashrate_hps);
    }
}

static LIVE: LazyLock<Mutex<Live>> = LazyLock::new(|| Mutex::new(Live::default()));

/// The shared history. Poisoning is recovered from rather than propagated: a
/// panicking painter must not take the whole dashboard down with it.
pub fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called from the stats poller for every line the worker prints, which is the
/// only place those lines exist. Reading `last_worker_log` from the view would
/// silently miss any line replaced between two frames.
pub fn record_worker_line(line: &str) {
    live().events.push(now_unix_ms(), line);
}

// ---------------------------------------------------------------------------
// 2. Formatting. All of it is pure, so all of it is tested.
// ---------------------------------------------------------------------------

/// Split "4.82GH/s" into ("4.82", "GH/s") so the unit can be set in amber
/// beside the figure, the way every number in the mockup is set.
pub fn split_value_unit(text: &str) -> (String, String) {
    let text = text.trim();
    let split = text
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == ',' || *c == '-' || *c == '+'))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let (value, unit) = text.split_at(split);
    (value.trim().to_string(), unit.trim().to_string())
}

/// `value` unless it is blank, in which case `fallback`. Used where the worker
/// has not yet echoed a setting back and the panel's own copy is the best
/// available answer.
pub fn str_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

/// The chain's own decade steps, with the space the mockup sets them with.
/// Used only for figures this panel derives (average, peak, per-device); the
/// headline figure is the worker's own `hashrate_display`.
pub fn format_hashrate(hps: f64) -> String {
    if !hps.is_finite() || hps <= 0.0 {
        return "0.00 H/s".to_string();
    }
    const UNITS: [(f64, &str); 5] = [
        (1e15, "PH/s"),
        (1e12, "TH/s"),
        (1e9, "GH/s"),
        (1e6, "MH/s"),
        (1e3, "KH/s"),
    ];
    for (scale, unit) in UNITS {
        if hps >= scale {
            return format!("{:.2} {unit}", hps / scale);
        }
    }
    format!("{hps:.2} H/s")
}

/// "18h 42m", the way the mockup writes uptime. Seconds only appear below a
/// minute, where they are the only thing worth saying.
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {}s", secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// 765432 becomes "765,432". The mockup groups every height and count.
pub fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "-45m" and "now": the chart's x axis.
///
/// Deliberately relative. The panel has no time zone database, and a clock
/// label an hour out on a rolling chart is worse than no clock at all.
pub fn tick_label(minutes_ago: u64) -> String {
    if minutes_ago == 0 {
        return "now".to_string();
    }
    if minutes_ago % 60 == 0 {
        return format!("-{}h", minutes_ago / 60);
    }
    format!("-{minutes_ago}m")
}

/// The signed change of the newest reading against the window average, as a
/// percentage. `None` when the window is too thin for the comparison to mean
/// anything, so the card says it is still collecting instead of printing a
/// number derived from two samples.
pub fn delta_vs_average(summary: &Summary, latest: f64) -> Option<f64> {
    if summary.count < 4 || summary.avg <= 0.0 {
        return None;
    }
    Some((latest - summary.avg) / summary.avg * 100.0)
}

// ---------------------------------------------------------------------------
// 3. Painting. The parts egui has no widget for.
// ---------------------------------------------------------------------------

/// The soft vertical wash under the performance curve, and the sparkline's
/// smaller version of it. Built as a mesh because egui has no gradient fill:
/// each column is two triangles whose top vertices carry the amber and whose
/// bottom vertices are transparent.
fn fill_under(painter: &egui::Painter, points: &[Pos2], baseline: f32, top: Color32) {
    if points.len() < 2 {
        return;
    }
    let mut mesh = egui::epaint::Mesh::default();
    let clear = Color32::from_rgba_unmultiplied(top.r(), top.g(), top.b(), 0);
    for (i, p) in points.iter().enumerate() {
        // Fade the wash out towards the baseline in the column itself, not just
        // at the edges, so a flat line still reads as a filled area.
        mesh.colored_vertex(*p, top);
        mesh.colored_vertex(egui::pos2(p.x, baseline), clear);
        if i > 0 {
            let a = (i as u32 - 1) * 2;
            mesh.add_triangle(a, a + 1, a + 2);
            mesh.add_triangle(a + 1, a + 2, a + 3);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The little rising line in the corner of the hashrate card.
///
/// Draws only what it was given. Fewer than two readings is not a flat line at
/// zero, it is nothing to draw, and it draws nothing.
pub fn sparkline(painter: &egui::Painter, rect: Rect, values: &[f32]) {
    if values.len() < 2 {
        return;
    }
    let (mut low, mut high) = (f32::MAX, f32::MIN);
    for v in values {
        low = low.min(*v);
        high = high.max(*v);
    }
    // A dead-flat series would divide by zero; give it a band so it draws as a
    // line through the middle.
    let span = if high - low > f32::EPSILON {
        high - low
    } else {
        high.abs().max(1.0)
    };
    let points: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32;
            let y =
                rect.bottom() - (rect.height() - 4.0) * ((v - low) / span).clamp(0.0, 1.0) - 2.0;
            egui::pos2(x, y)
        })
        .collect();
    fill_under(
        painter,
        &points,
        rect.bottom(),
        Color32::from_rgba_unmultiplied(247, 175, 52, 46),
    );
    painter.add(egui::Shape::line(points, Stroke::new(1.6, colors::ACCENT)));
}

/// What the wide chart needs to draw itself. Passing it as a struct keeps the
/// call at the layout site readable.
pub struct ChartPlot<'a> {
    /// Runs of samples, already split at the gaps.
    pub segments: &'a [Vec<Sample>],
    pub now_ms: u64,
    pub window: ChartWindow,
    /// The message shown in place of the plot when there is nothing to plot.
    pub empty_note: &'a str,
}

/// The wide Mining Performance chart: four gridlines, a y axis in the chain's
/// own hash-rate units, the amber curve and its wash, and a relative x axis.
pub fn area_chart(ui: &mut Ui, rect: Rect, plot: &ChartPlot<'_>) {
    let painter = ui.painter_at(rect);
    let axis_h = 22.0;
    // The axis column is measured, not assumed. It used to be a flat 46px, and
    // the painter is clipped to `rect`, so "4.26 GH/s" set at 10.5px lost its
    // leading digit off the left edge and printed ".26 GH/s": a hash-rate floor
    // wrong by an order of magnitude. The widest label the axis will actually
    // draw decides the width, so no number can be cut whatever the unit or the
    // language.
    let axis_font = FontId::new(10.5, FontFamily::Proportional);
    // A 22px inset at the top for the same reason in the other direction: the
    // first gridline sat exactly on `rect.top()` and its centred label lost its
    // upper half.
    let head_room = 9.0;

    let mut low = f64::MAX;
    let mut high = f64::MIN;
    let mut count = 0usize;
    for run in plot.segments {
        for s in run {
            low = low.min(s.hps as f64);
            high = high.max(s.hps as f64);
            count += 1;
        }
    }
    if count == 0 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            plot.empty_note,
            FontId::new(12.5, FontFamily::Proportional),
            colors::TEXT_DIM,
        );
        return;
    }
    // Pad the band so the curve never touches the frame, and so a perfectly
    // steady rate still has somewhere to sit.
    let band = (high - low).max(high * 0.04).max(1.0);
    let axis_low = (low - band * 0.25).max(0.0);
    let axis_high = high + band * 0.25;
    let axis_span = (axis_high - axis_low).max(f64::MIN_POSITIVE);

    // The four labels, and the column they need. The bottom one carries the
    // unit, as in the mockup; the others are bare numbers so the column stays
    // quiet.
    let labels: Vec<String> = (0..4)
        .map(|i| {
            let value = axis_high - (axis_high - axis_low) * i as f64 / 3.0;
            if i == 3 {
                format_hashrate(value)
            } else {
                split_value_unit(&format_hashrate(value)).0
            }
        })
        .collect();
    let widest = labels
        .iter()
        .map(|text| {
            ui.fonts(|f| f.layout_no_wrap(text.clone(), axis_font.clone(), colors::TEXT_DIM))
                .rect
                .width()
        })
        .fold(0.0f32, f32::max);
    let axis_w = widest.ceil() + 10.0;

    let plot_rect = Rect::from_min_max(
        egui::pos2(rect.left() + axis_w, rect.top() + head_room),
        egui::pos2(rect.right(), rect.bottom() - axis_h),
    );
    // A card too narrow for an axis has nowhere to put a chart. Bail rather
    // than draw an inside-out rectangle.
    if plot_rect.width() < 8.0 || plot_rect.height() < 8.0 {
        return;
    }

    let y_of = |v: f64| -> f32 {
        plot_rect.bottom() - (plot_rect.height() as f64 * (v - axis_low) / axis_span) as f32
    };
    let x_of = |ms: u64| -> f32 {
        let span = plot.window.span_ms() as f64;
        let from = plot.now_ms.saturating_sub(plot.window.span_ms());
        let t = ((ms.saturating_sub(from)) as f64 / span).clamp(0.0, 1.0);
        plot_rect.left() + plot_rect.width() * t as f32
    };

    for (i, text) in labels.into_iter().enumerate() {
        let value = axis_high - (axis_high - axis_low) * i as f64 / 3.0;
        let y = y_of(value);
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            Stroke::new(1.0, Color32::from_rgb(24, 24, 24)),
        );
        painter.text(
            egui::pos2(plot_rect.left() - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            text,
            axis_font.clone(),
            colors::TEXT_DIM,
        );
    }

    for run in plot.segments {
        let points: Vec<Pos2> = run
            .iter()
            .map(|s| egui::pos2(x_of(s.unix_ms), y_of(s.hps as f64)))
            .collect();
        // A reading with no neighbour is still a reading. It has no line to be
        // part of, so it is drawn as the point it is rather than skipped: an
        // invisible measurement is the same mistake as an invented one, and the
        // Average and Peak boxes below are already counting it.
        if points.len() < 2 {
            if let Some(point) = points.first() {
                painter.circle_filled(*point, 2.0, colors::ACCENT);
            }
            continue;
        }
        fill_under(
            &painter,
            &points,
            plot_rect.bottom(),
            Color32::from_rgba_unmultiplied(247, 175, 52, 54),
        );
        // A wider, dimmer pass under the line stands in for the glow in the
        // mockup. egui cannot blur, and this is the honest approximation.
        painter.add(egui::Shape::line(
            points.clone(),
            Stroke::new(5.0, Color32::from_rgba_unmultiplied(247, 175, 52, 26)),
        ));
        painter.add(egui::Shape::line(points, Stroke::new(2.0, colors::ACCENT)));
    }

    for minutes in plot.window.ticks() {
        let ms = plot.now_ms.saturating_sub(minutes * 60_000);
        let x = x_of(ms);
        let align = if minutes == 0 {
            egui::Align2::RIGHT_TOP
        } else if x <= plot_rect.left() + 1.0 {
            egui::Align2::LEFT_TOP
        } else {
            egui::Align2::CENTER_TOP
        };
        painter.text(
            egui::pos2(x, plot_rect.bottom() + 6.0),
            align,
            tick_label(minutes),
            FontId::new(10.5, FontFamily::Proportional),
            colors::TEXT_DIM,
        );
    }
}

/// The circular gauge in the Hardware card.
///
/// `fraction` is `None` when the panel has nothing to fill it with, and then
/// only the dark track is drawn: an empty ring says "not measured" without
/// pretending to a reading.
pub fn gauge_ring(
    painter: &egui::Painter,
    rect: Rect,
    fraction: Option<f32>,
    center_text: &str,
    caption: &str,
    color: Color32,
) {
    let c = rect.center();
    let side = rect.width().min(rect.height());
    if side < 24.0 {
        return;
    }
    // Measured off mockup 1, which is worth stating plainly because the comment
    // this replaces said the opposite: the mockup's track is a CLOSED circle
    // starting at twelve o'clock, and the amber runs clockwise from there. It is
    // not open at the bottom left. Column x=1455 of the mockup crosses amber at
    // both the top of the ring and the bottom, 12px thick, on a 100px outer
    // diameter. Ours was an open 280 degree arc from 135 degrees with an 80
    // degree hole across the bottom, 110px across.
    let width = (side * 0.12).min(12.0);
    let radius = side * 0.5 - width * 0.5;
    let start = -std::f32::consts::FRAC_PI_2;
    let full = std::f32::consts::PI * 2.0;
    let point_at = |a: f32| egui::pos2(c.x + radius * a.cos(), c.y + radius * a.sin());
    let arc = |from: f32, to: f32, stroke: Stroke| {
        let steps = 128;
        let points: Vec<Pos2> = (0..=steps)
            .map(|i| point_at(from + (to - from) * i as f32 / steps as f32))
            .collect();
        painter.add(egui::Shape::line(points, stroke));
    };

    arc(start, start + full, Stroke::new(width, TRACK));
    if let Some(fraction) = fraction {
        let f = fraction.clamp(0.0, 1.0);
        if f > 0.0 {
            let end = start + full * f;
            arc(start, end, Stroke::new(width, color));
            // egui's polyline has butt ends. The mockup's sweep is round at both
            // ends, and two dots of the stroke's own radius are what that is.
            painter.circle_filled(point_at(start), width * 0.5, color);
            painter.circle_filled(point_at(end), width * 0.5, color);
        }
    }

    // The figure and its caption are centred as a block, the way the mockup sets
    // them: the number sits above the middle and the caption below it.
    painter.text(
        c - Vec2::new(0.0, radius * 0.48),
        egui::Align2::CENTER_CENTER,
        center_text,
        FontId::new(22.0, FontFamily::Proportional),
        if fraction.is_some() {
            colors::TEXT
        } else {
            colors::TEXT_MUTED
        },
    );
    painter.text(
        c + Vec2::new(0.0, radius * 0.52),
        egui::Align2::CENTER_CENTER,
        caption,
        FontId::new(9.5, FontFamily::Proportional),
        colors::TEXT_DIM,
    );
}

/// The unfilled part of the gauge ring.
const TRACK: Color32 = Color32::from_rgb(31, 31, 31);

/// The small chip icon beside the device name.
pub fn device_glyph(painter: &egui::Painter, rect: Rect) {
    let stroke = Stroke::new(1.4, colors::ACCENT);
    let body = Rect::from_center_size(rect.center(), Vec2::splat(rect.width() * 0.44));
    painter.rect_stroke(body, Rounding::same(3.0), stroke);
    painter.rect_filled(
        Rect::from_center_size(rect.center(), Vec2::splat(rect.width() * 0.18)),
        Rounding::same(1.5),
        colors::ACCENT,
    );
    for i in 0..3 {
        let t = -0.28 + i as f32 * 0.28;
        let x = rect.center().x + rect.width() * t;
        let y = rect.center().y + rect.height() * t;
        painter.line_segment(
            [
                egui::pos2(x, body.top() - rect.height() * 0.11),
                egui::pos2(x, body.top()),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(x, body.bottom()),
                egui::pos2(x, body.bottom() + rect.height() * 0.11),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(body.left() - rect.width() * 0.11, y),
                egui::pos2(body.left(), y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(body.right(), y),
                egui::pos2(body.right() + rect.width() * 0.11, y),
            ],
            stroke,
        );
    }
}

// ---------------------------------------------------------------------------
// Small reusable pieces of the screen.
// ---------------------------------------------------------------------------

/// A card at a known width and a known minimum height, so a row of them lines
/// up the way the mockup's grid does.
///
/// The explicit column layout is not decoration. `Frame` gives its contents the
/// layout of whatever `Ui` it was opened in, and every card on this screen is
/// opened inside a `horizontal_top` row, so without this the card's own contents
/// would be dealt out left to right: labels beside their values, and
/// `available_width` shrinking to nothing part way down the card.
pub fn card(ui: &mut Ui, width: f32, min_height: f32, content: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(colors::BG_CARD)
        .stroke(Stroke::new(1.0, colors::BORDER_SOFT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(18.0, 16.0))
        .show(ui, |ui| {
            ui.set_width((width - 36.0).max(1.0));
            ui.set_min_height(min_height);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), content);
        });
}

/// A card title with its quiet subtitle, and whatever belongs at the far right
/// of the same line.
pub fn card_head(ui: &mut Ui, title: &str, sub: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(15.0)
                    .strong()
                    .color(colors::TEXT),
            );
            if !sub.is_empty() {
                ui.add_space(1.0);
                ui.label(
                    egui::RichText::new(sub)
                        .size(11.5)
                        .color(colors::TEXT_MUTED),
                );
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), right);
    });
}

/// The small outlined pill the mockup puts in card corners and the page header.
pub fn chip(ui: &mut Ui, text: &str, dot: Option<Color32>, accent: bool) {
    let (border, fg) = if accent {
        (colors::BORDER_ACCENT, colors::ACCENT)
    } else {
        (colors::BORDER_SOFT, colors::TEXT_MUTED)
    };
    Frame::none()
        .fill(if accent {
            colors::NAV_ACTIVE_BG
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(1.0, border))
        .rounding(Rounding::same(9.0))
        .inner_margin(Margin::symmetric(10.0, 5.0))
        .show(ui, |ui| {
            // Chips live in the page header, which is a right-to-left row, and
            // `ui.horizontal` inherits that preference. Stated explicitly so the
            // dot stays on the left of its word, as in the mockup, instead of
            // silently swapping sides depending on where the chip was placed.
            // Sized from the text rather than from the space available, because
            // that space is the whole rest of the header row.
            let font = FontId::new(11.5, FontFamily::Proportional);
            let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
            let size = Vec2::new(
                galley.rect.width() + if dot.is_some() { 13.0 } else { 0.0 },
                galley.rect.height().max(14.0),
            );
            ui.allocate_ui_with_layout(
                size,
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(dot) = dot {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, dot);
                    }
                    ui.label(egui::RichText::new(text).size(11.5).color(fg));
                },
            );
        });
}

/// One of the four boxes under the chart, and the three under the payout total.
pub fn mini_stat(ui: &mut Ui, width: f32, label: &str, value: &str, tint: Color32) {
    Frame::none()
        .fill(colors::BG_INPUT)
        .stroke(Stroke::new(1.0, colors::BORDER_SOFT))
        .rounding(Rounding::same(10.0))
        // 7 and not 10: mockup 1's boxes under the chart are 52px tall, ours
        // were 59.
        .inner_margin(Margin::symmetric(13.0, 7.0))
        .show(ui, |ui| {
            ui.set_width((width - 26.0).max(1.0));
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(label.to_uppercase())
                        .size(10.0)
                        .color(colors::TEXT_DIM),
                );
                ui.add_space(2.0);
                ui.label(egui::RichText::new(value).size(15.5).color(tint));
            });
        });
}

/// A row of the limits list: label left, value right, hairline under it.
pub fn limit_row(ui: &mut Ui, label: &str, value: &str, last: bool) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .color(colors::TEXT_MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).size(12.0).color(colors::TEXT));
        });
    });
    if !last {
        // 3 either side of the hairline, not 5: mockup 1 puts its limit
        // hairlines at y393, 418, 443, 468 and 493, a 25px pitch. Ours was 29.
        ui.add_space(3.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, Rounding::ZERO, Color32::from_rgb(24, 24, 24));
        ui.add_space(3.0);
    }
}

/// One of the four guard indicators at the foot of the Hardware card. Amber
/// when the protection is on, dim when it is off; never green, because the
/// panel cannot confirm a guard fired, only that it is configured.
pub fn guard_dot(ui: &mut Ui, on: bool, label: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            3.5,
            if on { colors::ACCENT } else { colors::TEXT_DIM },
        );
        ui.label(egui::RichText::new(label).size(11.5).color(if on {
            colors::TEXT_MUTED
        } else {
            colors::TEXT_DIM
        }));
    });
}

/// The compact 1H / 6H / 24H selector. `theme::segmented` is the page-level
/// control at 108px a side; this is the one that fits in a card corner.
pub fn window_selector(ui: &mut Ui, current: ChartWindow) -> Option<ChartWindow> {
    let mut picked = None;
    Frame::none()
        .fill(colors::BG_INPUT)
        .stroke(Stroke::new(1.0, colors::BORDER_SOFT))
        .rounding(Rounding::same(9.0))
        .inner_margin(Margin::same(3.0))
        .show(ui, |ui| {
            // Stated, not inherited: this selector sits at the right of a card
            // head, which is a right-to-left row, and `ui.horizontal` would take
            // that preference and deal the spans out as 24H, 6H, 1H.
            let count = ChartWindow::ALL.len() as f32;
            ui.allocate_ui_with_layout(
                Vec2::new(38.0 * count + 2.0 * (count - 1.0), 21.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for option in ChartWindow::ALL {
                        let on = option == current;
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(38.0, 21.0), Sense::click());
                        if ui.is_rect_visible(rect) {
                            if on {
                                ui.painter().rect_filled(
                                    rect,
                                    Rounding::same(6.0),
                                    colors::NAV_ACTIVE_BG,
                                );
                                ui.painter().rect_stroke(
                                    rect,
                                    Rounding::same(6.0),
                                    Stroke::new(1.0, colors::BORDER_ACCENT),
                                );
                            } else if resp.hovered() {
                                ui.painter().rect_filled(
                                    rect,
                                    Rounding::same(6.0),
                                    colors::BG_RAISED,
                                );
                            }
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                option.label(),
                                FontId::new(11.0, FontFamily::Proportional),
                                if on {
                                    colors::ACCENT
                                } else {
                                    colors::TEXT_MUTED
                                },
                            );
                        }
                        if resp.clicked() {
                            picked = Some(option);
                        }
                    }
                },
            );
        });
    picked
}

/// The heading above a worker line, in the operator's language.
pub fn event_title(d: &DashLabels, kind: EventKind) -> &'static str {
    match kind {
        EventKind::Success => d.ev_success,
        EventKind::Failure => d.ev_failure,
        EventKind::GpuFailure => d.ev_gpu,
        EventKind::Paused => d.ev_paused,
        EventKind::Resumed => d.ev_resumed,
        EventKind::Notice => d.ev_notice,
    }
}

/// One event row: round badge, the worker's line, and its age at the right.
pub fn event_row(ui: &mut Ui, event: &WorkerEvent, title: &str, now_ms: u64) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 13.0, colors::BG_INPUT);
        ui.painter()
            .circle_stroke(rect.center(), 13.0, Stroke::new(1.0, colors::BORDER_SOFT));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            event.kind.badge(),
            FontId::new(11.0, FontFamily::Proportional),
            event.kind.color(),
        );
        ui.add_space(8.0);
        let age_width = 44.0;
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - age_width).max(60.0));
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                egui::RichText::new(title)
                    .size(12.5)
                    .strong()
                    .color(colors::TEXT),
            );
            ui.label(
                egui::RichText::new(&event.detail)
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let age = format_age_secs(now_ms.saturating_sub(event.unix_ms) / 1000);
            ui.label(egui::RichText::new(age).size(10.5).color(colors::TEXT_DIM));
        });
    });
    ui.add_space(9.0);
}

/// A full-width button in the Mining Control stack.
pub fn stack_button(ui: &mut Ui, label: &str, style: ButtonStyle) -> egui::Response {
    let width = ui.available_width();
    let (fill, stroke, text) = match style {
        ButtonStyle::Primary => (
            colors::GREEN,
            Stroke::new(1.0, colors::GREEN_DIM),
            colors::BG_DEEP,
        ),
        ButtonStyle::Secondary => (
            colors::BG_INPUT,
            Stroke::new(1.0, colors::BORDER_SOFT),
            colors::TEXT,
        ),
        ButtonStyle::Danger => (
            colors::RED_DIM,
            Stroke::new(1.0, Color32::from_rgb(90, 42, 40)),
            colors::RED,
        ),
    };
    ui.add_sized(
        [width, 38.0],
        egui::Button::new(egui::RichText::new(label).color(text).strong().size(13.0))
            .fill(fill)
            .stroke(stroke)
            .rounding(Rounding::same(9.0)),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    Danger,
}

// ---------------------------------------------------------------------------
// The four cards across the top.
// ---------------------------------------------------------------------------

/// One headline card. Every string is supplied by the caller so this file never
/// has to decide what is true.
pub struct Kpi<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub unit: &'a str,
    /// The quiet line under the number. Empty leaves the row out.
    pub sub: &'a str,
    /// The third line. Amber when `foot_accent`, otherwise muted.
    pub foot: &'a str,
    pub foot_accent: bool,
    /// Readings for the corner sparkline, oldest first.
    pub spark: &'a [f32],
    /// Draws the card with the amber border the mockup gives the live one.
    pub highlight: bool,
}

/// The content height every headline card is held to.
///
/// The four cards in mockup 1 all run y140..262, a flat 122px row. Ours were
/// 134 / 119 / 134 / 134, because a `Frame` sizes to its own content and the
/// four cards do not have the same content: one carries a sparkline, two carry
/// a foot line, one carries neither. `set_min_height` alone is only a floor, so
/// this is a floor the tallest card also fits under: 122 outer, less the 15px
/// top and bottom margins and the two border pixels.
const KPI_CONTENT_H: f32 = 90.0;

pub fn kpi_card(ui: &mut Ui, width: f32, k: &Kpi<'_>) {
    Frame::none()
        .fill(colors::BG_CARD)
        .stroke(Stroke::new(
            1.0,
            if k.highlight {
                colors::BORDER_ACCENT
            } else {
                colors::BORDER_SOFT
            },
        ))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(18.0, 15.0))
        .show(ui, |ui| {
            ui.set_width((width - 36.0).max(1.0));
            ui.set_min_height(KPI_CONTENT_H);
            // Same reason as `card`: these are drawn in a horizontal row, and a
            // frame inherits the layout of the row it sits in.
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                // 10, one under the shared LABEL size: mockup 1 sets these four
                // caps 7px tall where ours were 8, and the four lines have to
                // add up to KPI_CONTENT_H or the row goes ragged again.
                ui.label(
                    egui::RichText::new(k.label.to_uppercase())
                        .size(10.0)
                        .color(colors::TEXT_DIM),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        egui::RichText::new(k.value)
                            .size(theme::typo::VALUE)
                            .color(colors::TEXT),
                    );
                    if !k.unit.is_empty() {
                        ui.label(
                            egui::RichText::new(k.unit)
                                .size(theme::typo::UNIT)
                                .strong()
                                .color(colors::ACCENT),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    // The text column gives the sparkline its width back before
                    // it starts. Without this a long translation would truncate
                    // at the full card width and leave the corner chart nothing
                    // to draw in.
                    let text_w = if k.spark.len() >= 2 {
                        (ui.available_width() - 120.0).max(60.0)
                    } else {
                        ui.available_width()
                    };
                    ui.vertical(|ui| {
                        ui.set_max_width(text_w);
                        // Truncate rather than wrap. A card that wraps its sub
                        // line is a card that grows, and one grown card puts the
                        // ragged bottom edge straight back; the ellipsis at
                        // least says out loud that there is more text.
                        if !k.sub.is_empty() {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(k.sub)
                                        .size(11.5)
                                        .color(colors::TEXT_MUTED),
                                )
                                .truncate(),
                            );
                        }
                        if !k.foot.is_empty() {
                            ui.add(
                                egui::Label::new(egui::RichText::new(k.foot).size(10.5).color(
                                    if k.foot_accent {
                                        colors::ACCENT
                                    } else {
                                        colors::TEXT_DIM
                                    },
                                ))
                                .truncate(),
                            );
                        }
                    });
                    if k.spark.len() >= 2 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::new(112.0, 32.0), Sense::hover());
                            sparkline(ui.painter(), rect, k.spark);
                        });
                    }
                });
            });
        });
}

// ---------------------------------------------------------------------------
// 4. Pool payouts: what this miner is owed, and what it has already been paid.
// ---------------------------------------------------------------------------

/// A money value the panel is willing to print. Never a bare "0": a real zero
/// the pool confirmed, a field the pool never sent, and an answer the panel
/// could not get are three different sentences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoneyCell {
    /// A chain amount, formatted by the chain.
    Amount(String),
    /// The pool answered, and the answer is zero.
    Zero,
    /// The pool answered, but not about this field.
    NotReported,
    /// The panel has no answer at all.
    Unknown,
}

impl MoneyCell {
    /// `zero_text` is what a confirmed zero should read as in this slot:
    /// "nothing paid yet" under a total, "nothing owed right now" under a
    /// balance. They are not interchangeable.
    pub fn display<'a>(&'a self, t: &'a Strings, zero_text: &'a str) -> &'a str {
        match self {
            MoneyCell::Amount(text) => text,
            MoneyCell::Zero => zero_text,
            MoneyCell::NotReported => t.pool_pay_not_reported,
            MoneyCell::Unknown => t.pool_pay_unknown,
        }
    }
}

/// Turn one reported money field into a cell. `None` means the pool never sent
/// it, which is NOT zero.
pub fn money_cell(amount: Option<&Amount>) -> MoneyCell {
    match amount {
        None => MoneyCell::NotReported,
        Some(amount) if amount.is_zero() => MoneyCell::Zero,
        Some(amount) => MoneyCell::Amount(amount.to_fin_string()),
    }
}

/// What the earnings block is showing. Split out from the drawing so the rules
/// about what may be claimed are testable without a window.
pub struct PoolMoneySummary {
    pub total_paid: MoneyCell,
    pub owed: MoneyCell,
    pub in_flight: MoneyCell,
}

impl PoolMoneySummary {
    /// Every state other than a real answer leaves all three cells unknown. A
    /// relay, a full node and an unreachable pool owe nothing because there is
    /// no ledger, not because the balance is zero.
    pub fn for_view(view: &PayoutView) -> PoolMoneySummary {
        match view {
            PayoutView::Earnings(e) => PoolMoneySummary {
                total_paid: money_cell(e.paid.as_ref()),
                owed: money_cell(e.pending.as_ref()),
                in_flight: money_cell(e.in_flight.as_ref()),
            },
            _ => PoolMoneySummary {
                total_paid: MoneyCell::Unknown,
                owed: MoneyCell::Unknown,
                in_flight: MoneyCell::Unknown,
            },
        }
    }
}

/// The one line that explains the current state in plain words, or `None` when
/// the pool answered properly and the numbers speak for themselves.
pub fn payout_explanation<'a>(view: &'a PayoutView, t: &'a Strings) -> Option<&'a str> {
    match view {
        PayoutView::Earnings(_) => None,
        PayoutView::NotApplicable => None,
        PayoutView::Waiting => Some(t.pool_pay_waiting),
        PayoutView::FullNode { local: true } => Some(t.pool_pay_solo),
        PayoutView::FullNode { local: false } => Some(t.pool_pay_node),
        PayoutView::Relay => Some(t.pool_pay_relay),
        PayoutView::PoolWithoutEarnings => Some(t.pool_pay_no_earnings),
        PayoutView::Unidentified => Some(t.pool_pay_unidentified),
        PayoutView::NoPayoutAddress { invalid: true } => Some(t.pool_pay_bad_address),
        PayoutView::NoPayoutAddress { invalid: false } => Some(t.pool_pay_no_address),
        PayoutView::PoolRefused(message) => Some(message),
        PayoutView::Unreachable { was_pool: true, .. } => Some(t.pool_pay_unreachable),
        // Never confirmed to be a pool and not answering now: it could be a
        // stopped full node. Say nothing rather than call it a pool that owes
        // money.
        PayoutView::Unreachable {
            was_pool: false, ..
        } => None,
    }
}

/// True only where a payout ledger exists to report on. Everywhere else the
/// money cards are hidden entirely rather than filled with "unknown".
fn has_ledger(view: &PayoutView) -> bool {
    matches!(
        view,
        PayoutView::Earnings(_) | PayoutView::Unreachable { was_pool: true, .. }
    )
}

/// Whether the payout section has anything true to put on the screen.
pub fn shows_anything(view: &PayoutView, t: &Strings) -> bool {
    has_ledger(view) || payout_explanation(view, t).is_some()
}

pub struct PoolMoneyPanel<'a> {
    pub t: &'a Strings,
    pub d: &'a DashLabels,
    pub money: &'a PoolMoney,
    /// The pool's short name or address, for the corner chip.
    pub endpoint: &'a str,
    pub connected: bool,
    /// The abbreviated payout address this miner announced, empty in solo.
    /// It is the one thing on the money card the operator has to be able to
    /// check without opening Setup: a payout to the wrong address is the
    /// failure this screen exists to make visible.
    pub worker_display: &'a str,
}

/// The Pool Earnings card: the total in the big amber figure, the three boxes,
/// and the published terms in two quiet columns, as in the mockup.
pub fn show_pool_money(ui: &mut Ui, p: &PoolMoneyPanel<'_>) {
    let t = p.t;
    let view = &p.money.view;
    let endpoint = p.endpoint.to_string();
    let connected = p.connected;
    card_head(ui, p.d.pool_title, p.d.pool_sub, |ui| {
        if !endpoint.is_empty() {
            chip(
                ui,
                &endpoint,
                Some(if connected {
                    colors::ACCENT
                } else {
                    colors::TEXT_DIM
                }),
                connected,
            );
        }
    });
    ui.add_space(12.0);

    let summary = PoolMoneySummary::for_view(view);
    if has_ledger(view) {
        let paid = summary.total_paid.display(t, t.pool_pay_nothing_paid);
        let (value, unit) = split_value_unit(paid);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            ui.label(
                egui::RichText::new(if value.is_empty() { paid } else { &value })
                    .size(27.0)
                    .color(colors::TEXT),
            );
            ui.label(
                egui::RichText::new(if unit.is_empty() {
                    t.pool_pay_total_paid.to_string()
                } else {
                    format!("{unit} {}", t.pool_pay_total_paid)
                })
                .size(12.5)
                .strong()
                .color(colors::ACCENT),
            );
        });
        ui.add_space(12.0);

        let width = ui.available_width();
        let cell = (width - 24.0) / 3.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            mini_stat(
                ui,
                cell,
                t.pool_pay_owed,
                summary.owed.display(t, t.pool_pay_nothing_owed),
                colors::TEXT,
            );
            mini_stat(
                ui,
                cell,
                t.pool_pay_in_flight,
                summary.in_flight.display(t, t.pool_pay_nothing_owed),
                colors::TEXT,
            );
            mini_stat(
                ui,
                cell,
                t.pool_pay_shares,
                &shares_text(view, t),
                colors::TEXT,
            );
        });
        ui.add_space(12.0);
    }

    if let Some(explanation) = payout_explanation(view, t) {
        ui.label(
            egui::RichText::new(explanation)
                .size(12.5)
                .color(colors::TEXT),
        );
        ui.add_space(8.0);
    }

    if let PayoutView::Earnings(earnings) = view {
        show_last_payout(ui, t, earnings);
    }
    if !p.worker_display.is_empty() {
        limit_row(ui, t.dash_detail_wallet, p.worker_display, true);
        ui.add_space(4.0);
    }

    show_pool_terms(ui, t, p.money.terms.as_ref(), view);

    if p.money.checked_unix_ms > 0 {
        ui.add_space(6.0);
        let age = format_age_secs(now_unix_ms().saturating_sub(p.money.checked_unix_ms) / 1000);
        ui.label(
            egui::RichText::new(t.pool_pay_checked_display(&age))
                .size(10.5)
                .color(colors::TEXT_DIM),
        );
    }
}

fn shares_text(view: &PayoutView, t: &Strings) -> String {
    match view {
        PayoutView::Earnings(e) => match e.shares_in_window {
            Some(count) => group_thousands(count),
            None => t.pool_pay_not_reported.to_string(),
        },
        _ => t.pool_pay_unknown.to_string(),
    }
}

fn show_last_payout(ui: &mut Ui, t: &Strings, e: &PoolEarnings) {
    let last = match &e.last_payout {
        None => t.pool_pay_never.to_string(),
        Some(last) => {
            let amount = money_cell(last.amount.as_ref());
            let amount = amount.display(t, t.pool_pay_nothing_paid).to_string();
            if last.unix_ms == 0 {
                amount
            } else {
                let age = format_age_secs(now_unix_ms().saturating_sub(last.unix_ms) / 1000);
                t.pool_pay_last_display(&amount, &age)
            }
        }
    };
    limit_row(ui, t.pool_pay_last, &last, true);
    ui.add_space(4.0);
}

/// Terms are only worth a heading where a pool exists to have them. A full node
/// and a relay have no payout terms, and saying "terms unavailable" about them
/// would imply they should.
fn show_pool_terms(ui: &mut Ui, t: &Strings, terms: Option<&PoolTerms>, view: &PayoutView) {
    let is_pool = matches!(
        view,
        PayoutView::Earnings(_)
            | PayoutView::PoolWithoutEarnings
            | PayoutView::PoolRefused(_)
            | PayoutView::NoPayoutAddress { .. }
    ) || terms.is_some();
    if !is_pool {
        return;
    }

    ui.add_space(8.0);
    let Some(terms) = terms else {
        ui.label(
            egui::RichText::new(t.pool_terms_unavailable)
                .size(11.5)
                .color(colors::TEXT_MUTED),
        );
        return;
    };

    // Two quiet columns of "label value", the way the mockup sets the terms
    // under the payout boxes.
    let rows = terms_rows(t, terms);
    let half = rows.len().div_ceil(2);
    let column = (ui.available_width() - 14.0) / 2.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        for chunk in rows.chunks(half.max(1)) {
            ui.vertical(|ui| {
                ui.set_width(column);
                ui.spacing_mut().item_spacing.y = 4.0;
                for (label, value) in chunk {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(
                            egui::RichText::new(*label)
                                .size(11.0)
                                .color(colors::TEXT_DIM),
                        );
                        ui.label(
                            egui::RichText::new(value)
                                .size(11.0)
                                .strong()
                                .color(colors::TEXT_MUTED),
                        );
                    });
                }
            });
        }
    });
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(t.pool_terms_source)
            .size(10.5)
            .color(colors::TEXT_DIM),
    );
}

/// The terms table. A field the pool did not publish is left out entirely
/// rather than shown as a blank or a guess.
pub fn terms_rows(t: &Strings, terms: &PoolTerms) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = Vec::new();
    if !terms.scheme.trim().is_empty() {
        rows.push((t.pool_terms_scheme, terms.scheme.clone()));
    }
    if let Some(window) = terms.window {
        rows.push((t.pool_terms_window, window.to_string()));
    }
    if let Some(factor) = terms.share_factor {
        rows.push((t.pool_terms_share_factor, format!("2^{factor}")));
    }
    if !terms.fee.trim().is_empty() {
        rows.push((t.pool_terms_fee, terms.fee.clone()));
    }
    if let Some(minimum) = &terms.minimum_payout {
        rows.push((t.pool_terms_minimum, minimum.to_fin_string()));
    }
    if let Some(depth) = terms.maturity_depth {
        rows.push((t.pool_terms_maturity, depth.to_string()));
    }
    if let Some(secs) = terms.settle_interval_secs {
        rows.push((t.pool_terms_interval, format_age_secs(secs)));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, strings};
    use crate::stats_poll::units_to_amount;

    fn en() -> Strings {
        strings(Lang::En)
    }

    // -- history -----------------------------------------------------------

    #[test]
    fn a_snapshot_the_worker_has_not_refreshed_is_never_recorded_twice() {
        // The stats file is re-read three times a second and rewritten far less
        // often. Counting the same snapshot again would manufacture readings.
        let mut s = Series::default();
        assert!(s.record(1_000_000, 4.0e9));
        assert!(
            !s.record(1_000_000, 4.0e9),
            "same timestamp is not new data"
        );
        assert!(!s.record(999_000, 4.0e9), "time never runs backwards");
        assert!(!s.record(1_002_000, 4.0e9), "closer than the sample grid");
        assert!(s.record(1_006_000, 4.1e9));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn an_unusable_reading_is_dropped_rather_than_charted() {
        let mut s = Series::default();
        assert!(
            !s.record(0, 4.0e9),
            "a worker with no clock has no timestamp"
        );
        assert!(!s.record(1_000_000, f64::NAN));
        assert!(!s.record(1_000_000, -1.0));
        assert_eq!(s.len(), 0);
        assert_eq!(s.summary(1_000_000, 60_000), None);
    }

    #[test]
    fn the_buffer_forgets_anything_older_than_the_longest_window() {
        let mut s = Series::default();
        s.record(1_000, 1.0);
        s.record(HISTORY_SPAN_MS + 500_000, 2.0);
        assert_eq!(s.len(), 1, "the day-old sample is gone");
        assert_eq!(s.latest().map(|x| x.hps), Some(2.0));
    }

    #[test]
    fn average_and_peak_are_taken_over_the_window_the_user_picked() {
        let mut s = Series::default();
        let now = 10_000_000u64;
        // One sample well outside the hour, three inside it.
        s.record(now - 7_200_000, 1.0e9);
        s.record(now - 1_800_000, 2.0e9);
        s.record(now - 900_000, 4.0e9);
        s.record(now, 6.0e9);
        let hour = s.summary(now, ChartWindow::H1.span_ms()).unwrap();
        assert_eq!(hour.count, 3);
        assert_eq!(hour.peak, 6.0e9);
        assert_eq!(hour.low, 2.0e9);
        assert_eq!(hour.avg, 4.0e9);
        let day = s.summary(now, ChartWindow::H24.span_ms()).unwrap();
        assert_eq!(day.count, 4, "the wider window keeps the older sample");
        assert_eq!(day.low, 1.0e9);
    }

    #[test]
    fn a_gap_in_the_measurements_breaks_the_line_instead_of_being_drawn_across() {
        // This is the whole point of the segments: while the worker was down
        // the panel measured nothing, and a line joining the two ends would
        // claim it was mining the entire time.
        let mut s = Series::default();
        let now = 10_000_000u64;
        for i in 0..4 {
            s.record(now - 3_000_000 + i * SAMPLE_SPACING_MS, 1.0e9);
        }
        for i in 0..3 {
            s.record(now - 600_000 + i * SAMPLE_SPACING_MS, 2.0e9);
        }
        let runs = s.segments(now, ChartWindow::H1.span_ms());
        assert_eq!(runs.len(), 2, "two runs, not one line across the outage");
        assert_eq!(runs[0].len(), 4);
        assert_eq!(runs[1].len(), 3);
    }

    #[test]
    fn a_thin_window_refuses_to_state_a_trend() {
        let summary = Summary {
            count: 3,
            avg: 4.0e9,
            peak: 4.0e9,
            low: 4.0e9,
            oldest_ms: 0,
            newest_ms: 0,
        };
        assert_eq!(delta_vs_average(&summary, 5.0e9), None);
        let wide = Summary {
            count: 40,
            ..summary
        };
        let delta = delta_vs_average(&wide, 5.0e9).unwrap();
        assert!((delta - 25.0).abs() < 1e-9, "{delta}");
        assert!(delta_vs_average(&Summary { avg: 0.0, ..wide }, 1.0).is_none());
    }

    // -- worker events -----------------------------------------------------

    #[test]
    fn only_lines_the_workers_really_print_become_events() {
        assert_eq!(
            classify("████ [MINING SUCCESS] Find a block height 765432,"),
            Some(EventKind::Success)
        );
        assert_eq!(
            classify("[efficiency] OpenCL error - reducing work_groups 64 -> 32"),
            Some(EventKind::GpuFailure)
        );
        assert_eq!(
            classify("[efficiency] GPU batch failed: CL_OUT_OF_RESOURCES"),
            Some(EventKind::GpuFailure)
        );
        assert_eq!(
            classify("Error: cannot get block data at http://127.0.0.1:8081"),
            Some(EventKind::Failure)
        );
        assert_eq!(
            classify("[submit] node rejected height 5: stale"),
            Some(EventKind::Failure)
        );
        assert_eq!(
            classify("[Mining] PAUSED: pool reports stale work"),
            Some(EventKind::Paused)
        );
        assert_eq!(
            classify("[Mining] Fresh work received, mining resumes."),
            Some(EventKind::Resumed)
        );
        assert_eq!(
            classify("[efficiency] mode=profit profile=amd_profit"),
            Some(EventKind::Notice)
        );
        // Chatter with no tag and no known phrase is dropped, not filed under a
        // heading nobody can check.
        assert_eq!(classify("hashing along nicely"), None);
        assert_eq!(classify("   "), None);
        assert_eq!(classify("▔▔▔▔▔▔▔▔▔▔▔▔▔▔"), None);
    }

    #[test]
    fn the_gpu_error_box_counts_gpu_failures_and_nothing_else() {
        let mut log = EventLog::default();
        assert!(log.push(1_000, "[efficiency] GPU batch failed: CL_OUT_OF_RESOURCES"));
        assert!(log.push(2_000, "Error: cannot get block data at http://x"));
        assert!(log.push(3_000, "[efficiency] OpenCL error - reducing work_groups"));
        assert_eq!(
            log.gpu_errors(),
            2,
            "a node error is not a GPU error and must not inflate the count"
        );
        assert_eq!(log.recent(2).len(), 2);
        assert_eq!(log.recent(9).len(), 3);
    }

    #[test]
    fn a_worker_repeating_itself_gets_one_row_not_forty() {
        let mut log = EventLog::default();
        assert!(log.push(1_000, "[efficiency] GPU batch failed: CL_OUT_OF_RESOURCES"));
        assert!(!log.push(2_000, "[efficiency] GPU batch failed: CL_OUT_OF_RESOURCES"));
        assert_eq!(log.gpu_errors(), 1, "the count follows the rows");
        assert_eq!(log.recent(9).len(), 1);
    }

    #[test]
    fn the_event_feed_is_bounded_and_printable() {
        let mut log = EventLog::default();
        for i in 0..(MAX_EVENTS * 2) {
            log.push(i as u64 * 1_000, &format!("[bench] step {i}"));
        }
        assert_eq!(log.recent(1_000).len(), MAX_EVENTS);
        let mut long = EventLog::default();
        long.push(1, &format!("[bench] {}", "x".repeat(4_000)));
        let event = long.recent(1)[0].clone();
        assert!(
            event.detail.chars().count() <= 123,
            "{}",
            event.detail.len()
        );
        let mut noisy = EventLog::default();
        noisy.push(1, "[bench]\tone\ntwo\u{0007}three");
        assert_eq!(noisy.recent(1)[0].detail, "[bench] one two three");
    }

    // -- formatting --------------------------------------------------------

    #[test]
    fn the_unit_is_split_off_so_it_can_be_set_in_amber() {
        assert_eq!(
            split_value_unit("4.82GH/s"),
            ("4.82".to_string(), "GH/s".to_string())
        );
        assert_eq!(
            split_value_unit("18.4000 HAC"),
            ("18.4000".to_string(), "HAC".to_string())
        );
        assert_eq!(
            split_value_unit("0.00H/s"),
            ("0.00".to_string(), "H/s".to_string())
        );
        // A chain amount has no unit to split off, and must survive whole.
        assert_eq!(
            split_value_unit("25:248"),
            ("25".to_string(), ":248".to_string())
        );
        assert_eq!(split_value_unit(""), (String::new(), String::new()));
        assert_eq!(
            split_value_unit("unknown"),
            (String::new(), "unknown".to_string())
        );
    }

    #[test]
    fn derived_hashrates_use_the_same_decades_as_the_chain() {
        assert_eq!(format_hashrate(4_820_000_000.0), "4.82 GH/s");
        assert_eq!(format_hashrate(260_000_000.0), "260.00 MH/s");
        assert_eq!(format_hashrate(1_500.0), "1.50 KH/s");
        assert_eq!(format_hashrate(12.0), "12.00 H/s");
        assert_eq!(format_hashrate(0.0), "0.00 H/s");
        assert_eq!(format_hashrate(f64::NAN), "0.00 H/s");
    }

    #[test]
    fn uptime_reads_the_way_the_mockup_writes_it() {
        assert_eq!(format_uptime(67_320), "18h 42m");
        assert_eq!(format_uptime(90_061), "1d 1h");
        assert_eq!(format_uptime(305), "5m 5s");
        assert_eq!(format_uptime(9), "9s");
        assert_eq!(format_uptime(0), "0s");
    }

    #[test]
    fn heights_and_counts_are_grouped() {
        assert_eq!(group_thousands(765_432), "765,432");
        assert_eq!(group_thousands(1_284), "1,284");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn the_time_axis_is_relative_because_the_panel_has_no_time_zone() {
        assert_eq!(tick_label(0), "now");
        assert_eq!(tick_label(15), "-15m");
        assert_eq!(tick_label(60), "-1h");
        assert_eq!(tick_label(360), "-6h");
        assert_eq!(ChartWindow::H1.ticks(), [60, 45, 30, 15, 0]);
        assert_eq!(ChartWindow::H24.ticks(), [1440, 1080, 720, 360, 0]);
    }

    // -- the custom painting -----------------------------------------------

    /// The ring, the sparkline and the chart are hand-painted, so the only way
    /// to know they survive an empty series, a single sample, a dead-flat line
    /// or a card too narrow to hold an axis is to actually run them.
    #[test]
    fn the_hand_painted_parts_survive_every_degenerate_input() {
        let now = 10_000_000u64;
        let flat: Vec<Sample> = (0..6)
            .map(|i| Sample {
                unix_ms: now - 30_000 + i * SAMPLE_SPACING_MS,
                hps: 4.0e9,
            })
            .collect();
        let one = vec![flat[0]];

        egui::__run_test_ui(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(420.0, 200.0), Sense::hover());

            // Sparklines: nothing, one point, a flat line, real motion.
            sparkline(ui.painter(), rect, &[]);
            sparkline(ui.painter(), rect, &[1.0]);
            sparkline(ui.painter(), rect, &[2.0, 2.0, 2.0]);
            sparkline(ui.painter(), rect, &[1.0, 5.0, 3.0, 9.0]);
            // A zero series must not divide by a zero span.
            sparkline(ui.painter(), rect, &[0.0, 0.0]);

            // Charts: empty, one run of one, one run that is dead flat, two
            // runs with a hole between them, and a rectangle with no room.
            for segments in [
                vec![],
                vec![one.clone()],
                vec![flat.clone()],
                vec![flat.clone(), flat.clone()],
            ] {
                for r in [rect, Rect::from_min_size(rect.min, Vec2::new(20.0, 200.0))] {
                    area_chart(
                        ui,
                        r,
                        &ChartPlot {
                            segments: &segments,
                            now_ms: now,
                            window: ChartWindow::H24,
                            empty_note: "nothing yet",
                        },
                    );
                }
            }

            // Rings: unknown, empty, full, and out of range in both directions.
            for fraction in [
                None,
                Some(0.0),
                Some(0.42),
                Some(1.0),
                Some(-3.0),
                Some(9.0),
            ] {
                gauge_ring(
                    ui.painter(),
                    Rect::from_min_size(rect.min, Vec2::splat(112.0)),
                    fraction,
                    "42%",
                    "WORK GROUPS",
                    colors::ACCENT,
                );
            }
            device_glyph(
                ui.painter(),
                Rect::from_min_size(rect.min, Vec2::splat(40.0)),
            );
        });
    }

    #[test]
    fn the_cards_lay_out_with_nothing_in_them() {
        let event = WorkerEvent {
            unix_ms: 1_000,
            kind: EventKind::GpuFailure,
            detail: "[efficiency] GPU batch failed".to_string(),
        };
        egui::__run_test_ui(|ui| {
            card(ui, 320.0, 200.0, |ui| {
                card_head(ui, "Title", "", |_ui| {});
                card_head(ui, "Title", "Subtitle", |ui| {
                    chip(ui, "chip", Some(colors::ACCENT), true);
                });
                mini_stat(ui, 140.0, "peak", "", colors::TEXT);
                limit_row(ui, "label", "value", false);
                limit_row(ui, "label", "value", true);
                guard_dot(ui, true, "Thermal guard");
                guard_dot(ui, false, "Profit pause");
                window_selector(ui, ChartWindow::H6);
                event_row(ui, &event, "GPU error", 61_000);
                stack_button(ui, "Cancel", ButtonStyle::Danger);
                kpi_card(
                    ui,
                    280.0,
                    &Kpi {
                        label: "",
                        value: "",
                        unit: "",
                        sub: "",
                        foot: "",
                        foot_accent: false,
                        spark: &[],
                        highlight: true,
                    },
                );
            });
        });
    }

    // -- payouts -----------------------------------------------------------

    #[test]
    fn a_reported_amount_is_printed_by_the_chain_not_by_us() {
        let t = en();
        let cell = money_cell(Some(&units_to_amount(250)));
        assert_eq!(cell, MoneyCell::Amount("25:248".to_string()));
        assert_eq!(cell.display(&t, t.pool_pay_nothing_paid), "25:248");
    }

    #[test]
    fn a_confirmed_zero_and_a_missing_field_never_read_the_same() {
        let t = en();
        let zero = money_cell(Some(&units_to_amount(0)));
        let absent = money_cell(None);
        assert_eq!(zero, MoneyCell::Zero);
        assert_eq!(absent, MoneyCell::NotReported);
        assert_ne!(
            zero.display(&t, t.pool_pay_nothing_paid),
            absent.display(&t, t.pool_pay_nothing_paid)
        );
        // And a confirmed zero never renders as a bare number.
        assert_eq!(
            zero.display(&t, t.pool_pay_nothing_paid),
            t.pool_pay_nothing_paid
        );
        assert!(!zero.display(&t, t.pool_pay_nothing_paid).contains('0'));
    }

    #[test]
    fn the_same_zero_reads_differently_under_a_total_and_under_a_balance() {
        let t = en();
        let zero = money_cell(Some(&units_to_amount(0)));
        assert_eq!(
            zero.display(&t, t.pool_pay_nothing_paid),
            t.pool_pay_nothing_paid
        );
        assert_eq!(
            zero.display(&t, t.pool_pay_nothing_owed),
            t.pool_pay_nothing_owed
        );
        assert_ne!(t.pool_pay_nothing_paid, t.pool_pay_nothing_owed);
    }

    #[test]
    fn an_unreachable_pool_is_unknown_and_is_never_shown_as_zero() {
        let t = en();
        let view = PayoutView::Unreachable {
            reason: "connection refused".to_string(),
            was_pool: true,
        };
        let summary = PoolMoneySummary::for_view(&view);
        assert_eq!(summary.total_paid, MoneyCell::Unknown);
        assert_eq!(summary.owed, MoneyCell::Unknown);
        assert_eq!(summary.in_flight, MoneyCell::Unknown);
        let shown = summary.total_paid.display(&t, t.pool_pay_nothing_paid);
        assert_eq!(shown, t.pool_pay_unknown);
        assert_ne!(shown, t.pool_pay_nothing_paid);
        assert_eq!(payout_explanation(&view, &t), Some(t.pool_pay_unreachable));
        // The shares box follows the same rule as the money boxes.
        assert_eq!(shares_text(&view, &t), t.pool_pay_unknown);
    }

    #[test]
    fn a_relay_and_a_full_node_show_no_money_cards_at_all() {
        let t = en();
        // Showing "0 HAC paid" here would be the lie: there is no ledger, so the
        // whole block of numbers is withheld and only the explanation is shown.
        for view in [
            PayoutView::Relay,
            PayoutView::FullNode { local: true },
            PayoutView::FullNode { local: false },
            PayoutView::PoolWithoutEarnings,
            PayoutView::Unidentified,
            PayoutView::NoPayoutAddress { invalid: false },
            PayoutView::NoPayoutAddress { invalid: true },
            PayoutView::Waiting,
        ] {
            assert!(!has_ledger(&view), "{view:?} must not show money cards");
            assert!(
                payout_explanation(&view, &t).is_some(),
                "{view:?} must explain itself in words"
            );
        }
    }

    #[test]
    fn solo_relay_and_remote_node_each_get_their_own_wording() {
        let t = en();
        let solo = payout_explanation(&PayoutView::FullNode { local: true }, &t).unwrap();
        let node = payout_explanation(&PayoutView::FullNode { local: false }, &t).unwrap();
        let relay = payout_explanation(&PayoutView::Relay, &t).unwrap();
        assert_ne!(solo, node);
        assert_ne!(node, relay);
        assert_ne!(solo, relay);
    }

    #[test]
    fn an_address_that_never_answered_is_not_called_a_pool() {
        // A stopped local full node must not turn into "your pool cannot be
        // reached": the panel has no evidence it was ever a pool.
        let t = en();
        let view = PayoutView::Unreachable {
            reason: "connection refused".to_string(),
            was_pool: false,
        };
        assert_eq!(payout_explanation(&view, &t), None);
        assert!(!has_ledger(&view));
        assert!(!shows_anything(&view, &t), "the section stays off screen");
        assert!(!shows_anything(&PayoutView::NotApplicable, &t));
        assert!(shows_anything(&PayoutView::Relay, &t));
    }

    #[test]
    fn a_pool_refusal_is_shown_to_the_miner_verbatim() {
        let t = en();
        let view = PayoutView::PoolRefused("set pool_worker so the pool can pay you".to_string());
        assert_eq!(
            payout_explanation(&view, &t),
            Some("set pool_worker so the pool can pay you")
        );
    }

    #[test]
    fn a_real_answer_shows_numbers_and_no_excuse_line() {
        let t = en();
        let view = PayoutView::Earnings(PoolEarnings {
            worker: "1abc".to_string(),
            paid: Some(units_to_amount(250)),
            pending: Some(units_to_amount(3)),
            in_flight: None,
            last_payout: None,
            shares_in_window: Some(17),
        });
        let summary = PoolMoneySummary::for_view(&view);
        assert_eq!(
            summary.total_paid.display(&t, t.pool_pay_nothing_paid),
            "25:248"
        );
        assert_eq!(summary.owed.display(&t, t.pool_pay_nothing_owed), "3:247");
        assert_eq!(
            summary.in_flight.display(&t, t.pool_pay_nothing_owed),
            t.pool_pay_not_reported
        );
        assert_eq!(payout_explanation(&view, &t), None);
        assert!(has_ledger(&view));
        assert_eq!(shares_text(&view, &t), "17");
    }

    #[test]
    fn terms_show_only_what_the_pool_actually_published() {
        let t = en();
        let terms = PoolTerms {
            scheme: "PPLNS".to_string(),
            window: Some(4096),
            share_factor: Some(24),
            fee: "0%".to_string(),
            minimum_payout: Some(units_to_amount(1)),
            maturity_depth: Some(16),
            settle_interval_secs: Some(300),
        };
        let rows = terms_rows(&t, &terms);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0], (t.pool_terms_scheme, "PPLNS".to_string()));
        assert_eq!(rows[4], (t.pool_terms_minimum, "1:247".to_string()));
        assert_eq!(rows[6], (t.pool_terms_interval, "5m".to_string()));

        // A pool that publishes only its scheme gets exactly one row, not six
        // rows of invented defaults.
        let sparse = PoolTerms {
            scheme: "PPLNS".to_string(),
            ..PoolTerms::default()
        };
        assert_eq!(terms_rows(&t, &sparse).len(), 1);
        assert!(terms_rows(&t, &PoolTerms::default()).is_empty());
    }
}
