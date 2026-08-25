//! The Dashboard screen.
//!
//! The layout is the commissioned mockup: four headline cards, a wide
//! performance chart beside a hardware card, three cards along the bottom, and
//! a status strip under all of it.
//!
//! Every figure on this screen comes from something the panel has actually
//! measured: the worker's own stats snapshot, the lines the worker printed, the
//! payout pool's answer, or the history this panel accumulated from those. Where
//! the mockup shows a number no part of this build can know, the slot carries
//! the reason instead.
//!
//! The round gauge is the case worth spelling out. The worker owns the GPU
//! sensor and now publishes what it read, so the gauge is a real thermometer on
//! a machine whose GPU answers. On one that does not answer there is no reading
//! to draw, and the gauge shows the work-group load instead, captioned as such.
//! It is never filled with a zero, because a ring reading 0 degrees looks like a
//! working sensor on a cold card.

use eframe::egui;

use crate::MinerApp;
use crate::connect::ConnectMode;
use crate::currency::Currency;
use crate::dashboard::{self, ButtonStyle, ChartPlot, ChartWindow, Kpi};
use crate::i18n::{DashLabels, Strings};
use crate::mining_kind::MiningKind;
use crate::stats_poll::MAX_RESTART_ATTEMPTS;
use crate::theme;
use crate::theme::colors;

/// Wall clock the sparkline and the chart are measured against.
const SPARK_SPAN_MS: u64 = 15 * 60 * 1_000;

/// 14, not 12. Mockup 1 leaves 13px of background between two card borders, in
/// the headline row and between rows alike; at 12 ours measured 11.
const GAP: f32 = 14.0;
/// Both cards in the middle row are this tall, so the row squares off the way
/// the mockup's does.
///
/// Not the mockup's own 429px outer height, and the reason is worth writing
/// down. The mockup has no window-level status bar; this build does, because it
/// is the one place a sync or a stopped worker is visible from every screen, and
/// that band plus the header costs about 100px the mockup never spends. At 372
/// the page's own status strip was pushed off the bottom of a 1047px window
/// entirely. The middle row gives back what it can: the mockup's own chart plot
/// is 136px tall against ours, so there is room to take here and nowhere else.
const MID_ROW_H: f32 = 344.0;
const CHART_H: f32 = 196.0;
/// Mockup 1's bottom row runs y719..973, a 255px card, which is this content
/// height plus the 15px margins and the two border pixels.
const BOTTOM_ROW_H: f32 = 223.0;

/// The column split of each row, measured off the mockup rather than guessed.
///
/// The headline row is not four equal cards: the hash-rate card is half again
/// as wide as the other three, because it is the one carrying a sparkline.
/// 1,724px of content in the mockup is 524 + 3 x 388 with three 12px gutters.
const KPI_NARROW_FRAC: f32 = 0.2299;
/// Mining Performance against Hardware & Safety, 1,098 : 613.
const PERF_FRAC: f32 = 0.6413;
/// Pool Earnings : Recent Worker Events : Mining Control, 664 : 562 : 472.
const POOL_FRAC: f32 = 0.3906;
const EVENTS_FRAC: f32 = 0.3306;

/// The scale the temperature ring uses when no limit was configured. It is a
/// drawing scale and nothing else: the figure inside the ring is always the
/// measurement itself.
const GAUGE_TEMP_SCALE_C: f32 = 100.0;

/// The ring, for a temperature the worker really read.
///
/// The fill is measured against the operator's own limit where one is set,
/// because that is the number the thermal guard acts on and the one the mockup
/// prints beside the ring. With the guard off there is no limit to fill towards,
/// so the ring falls back to a fixed scale. Returns the fill, the figure for the
/// middle, and whether the card has reached the configured limit.
fn temperature_gauge(temp_c: f32, max_temp_c: u32) -> (f32, String, bool) {
    let scale = if max_temp_c > 0 {
        max_temp_c as f32
    } else {
        GAUGE_TEMP_SCALE_C
    };
    let over_limit = max_temp_c > 0 && temp_c >= max_temp_c as f32;
    (
        (temp_c / scale).clamp(0.0, 1.0),
        format!("{temp_c:.0}°"),
        over_limit,
    )
}

/// Whether the Hardware card must say, in words, that no sensor answered.
///
/// True exactly when the ring stopped being a thermometer while a worker was
/// still reporting: a live snapshot from a GPU miner that carries no
/// temperature is the machine telling us it has no sensor, and that is the case
/// the ring hides by quietly switching to work-group load.
///
/// Not said when the worker is HACD, which mines on the CPU and was never going
/// to fill a GPU thermometer, and not said when the snapshot is stale, because
/// then nothing at all is being reported and an absent temperature says nothing
/// about a sensor.
fn say_no_sensor_answered(is_hacd: bool, live: bool, measured_temp: Option<f32>) -> bool {
    !is_hacd && live && measured_temp.is_none()
}

/// Whether this snapshot proves that no HAC price is configured, so the money
/// figures in it are arithmetic rather than measurement.
///
/// The worker computes `daily_revenue_eur = hac_per_day * hac_price`. A rig that
/// is earning HAC and still reports no revenue at all can only be a rig whose
/// price is zero, and `daily_net_eur` is then exactly minus the power bill, on a
/// machine that may be earning perfectly well.
///
/// It is read from the snapshot, not from this panel's own price field, because
/// the snapshot is what produced the figures being drawn: a price typed into
/// Settings but not yet carried into a running worker would otherwise put the
/// invented loss straight back on the card.
///
/// A rig earning no HAC is not this case. There the revenue really is zero at
/// any price, so minus the power bill is the true net and is shown as such.
fn hac_price_unset(hac_per_day: f64, daily_revenue_eur: f64) -> bool {
    hac_per_day > 0.0 && daily_revenue_eur <= 0.0
}

/// Whether the Hardware ring is drawn in the alarm colour.
///
/// Two facts, and only two: the card is over the operator's own limit, or
/// something is holding it below the work groups it was told to run. Neither is
/// the mere presence of a work-group count, which is what used to paint this
/// ring red on a rig running all 48 of its 48. It is a named function so the
/// composition itself can be tested against a real snapshot rather than
/// re-derived in a test that would still pass if the drawing changed.
fn hardware_ring_is_red(
    too_hot: bool,
    stats: &app::efficiency::MiningStatsSnapshot,
    now_ms: u64,
) -> bool {
    too_hot || crate::stats_poll::work_groups_clamped(stats, now_ms)
}

fn abbreviated_wallet(wallet: &str) -> String {
    let wallet = wallet.trim();
    let chars: Vec<char> = wallet.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 20 {
        return wallet.to_string();
    }
    let prefix: String = chars.iter().take(8).collect();
    let suffix: String = chars[chars.len() - 6..].iter().collect();
    format!("{prefix}…{suffix}")
}

/// What a click on this screen asked for. Collected while drawing and acted on
/// afterwards, because the buttons live inside closures that already hold the
/// app immutably.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    AutoTune,
    CancelAutoTune,
    CancelOpenCl,
}

impl MinerApp {
    fn truncate_wallet(wallet: &str) -> String {
        abbreviated_wallet(wallet)
    }

    pub(super) fn ui_dashboard(&mut self, ui: &mut egui::Ui) {
        let t = self.t();
        let d = crate::i18n::dash_labels(self.lang);
        let now_ms = crate::stats_poll::now_unix_ms();

        // Fold this frame's snapshot into the history before anything reads it.
        // The guard is dropped immediately: the worker log is appended from the
        // poller through the same lock.
        let (spark, chart_window, summary, gpu_errors, show_log, events) = {
            let mut live = dashboard::live();
            live.observe(&self.stats);
            let window = live.window;
            let spark: Vec<f32> = live
                .series
                .window(now_ms, SPARK_SPAN_MS)
                .iter()
                .map(|s| s.hps)
                .collect();
            let summary = live.series.summary(now_ms, window.span_ms());
            let events: Vec<_> = live
                .events
                .recent(if live.show_log { 24 } else { 4 })
                .into_iter()
                .cloned()
                .collect();
            (
                spark,
                window,
                summary,
                live.events.gpu_errors(),
                live.show_log,
                events,
            )
        };

        let mut action: Option<Action> = None;
        let mut new_window: Option<ChartWindow> = None;
        let mut toggle_log = false;

        // The page owns its own vertical rhythm. egui's paragraph spacing is
        // 12px and would be added to every `add_space` below, which is how a
        // 12px gutter turns into 24 and a 6px one into 18: every gap on the
        // screen was double what it reads as in the mockup.
        ui.spacing_mut().item_spacing.y = 0.0;

        self.dash_page_header(ui, &t, &d);
        ui.add_space(11.0);

        let total = ui.available_width();
        let narrow = ((total - GAP * 3.0) * KPI_NARROW_FRAC).floor();
        let wide = total - GAP * 3.0 - narrow * 3.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            self.dash_kpis(
                ui,
                &t,
                &d,
                wide,
                narrow,
                &spark,
                summary.as_ref(),
                chart_window,
            );
        });

        ui.add_space(GAP);
        let perf_w = ((total - GAP) * PERF_FRAC).floor();
        let hw_w = total - GAP - perf_w;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            dashboard::card(ui, perf_w, MID_ROW_H, |ui| {
                new_window = self.dash_performance(
                    ui,
                    &d,
                    chart_window,
                    now_ms,
                    summary.as_ref(),
                    gpu_errors,
                );
            });
            dashboard::card(ui, hw_w, MID_ROW_H, |ui| {
                self.dash_hardware(ui, &t, &d);
            });
        });

        ui.add_space(GAP);
        let pool_w = ((total - GAP * 2.0) * POOL_FRAC).floor();
        let events_w = ((total - GAP * 2.0) * EVENTS_FRAC).floor();
        let control_w = total - GAP * 2.0 - pool_w - events_w;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            dashboard::card(ui, pool_w, BOTTOM_ROW_H, |ui| {
                self.dash_pool(ui, &t, &d);
            });
            dashboard::card(ui, events_w, BOTTOM_ROW_H, |ui| {
                self.dash_events(ui, &d, &events, now_ms, show_log);
            });
            dashboard::card(ui, control_w, BOTTOM_ROW_H, |ui| {
                let (a, toggle) = self.dash_control(ui, &t, &d, show_log);
                action = a;
                toggle_log = toggle;
            });
        });

        ui.add_space(GAP);
        self.dash_footer(ui, &t, &d);

        if new_window.is_some() || toggle_log {
            let mut live = dashboard::live();
            if let Some(window) = new_window {
                live.window = window;
            }
            if toggle_log {
                live.show_log = !live.show_log;
            }
        }

        match action {
            Some(Action::Start) => self.start_mining(),
            Some(Action::Stop) => self.stop_mining(),
            Some(Action::AutoTune) => self.run_benchmark(),
            Some(Action::CancelAutoTune) => self.stop_benchmark(),
            Some(Action::CancelOpenCl) => self.cancel_opencl_probe(),
            None => {}
        }
    }

    // -- page header --------------------------------------------------------

    fn dash_page_header(&self, ui: &mut egui::Ui, t: &Strings, d: &DashLabels) {
        let is_hacd = self.mining_kind == MiningKind::Hacd;
        let kind = if is_hacd { t.mining_hacd } else { t.mining_hac };
        let backend = if is_hacd {
            "CPU"
        } else if self.use_cuda {
            "CUDA GPU"
        } else {
            "OpenCL GPU"
        };
        let profile = if is_hacd {
            self.mode_label(self.mode_idx).to_string()
        } else {
            self.gpu_profile.clone()
        };
        let subtitle = format!("{kind} · {backend} · {profile} · {}", d.live_telemetry);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                // 23 and 11, not 25 and 12: measured against the mockup, where
                // "Mining Overview" is 172px wide and its caption 5.0px a
                // character. At 25 the title ran 12px long and the whole header
                // block sat a line lower than it should.
                ui.label(
                    egui::RichText::new(d.overview_title)
                        .size(23.0)
                        .strong()
                        .color(colors::TEXT),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(subtitle)
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                dashboard::chip(
                    ui,
                    &format!("{} · {}", self.lang.code().to_uppercase(), {
                        let name = self.currency.name();
                        name.split(' ').next().unwrap_or(name).to_string()
                    }),
                    None,
                    false,
                );
                dashboard::chip(ui, &self.dash_connection_chip(t), None, false);
                if self.stats.height > 0 {
                    dashboard::chip(
                        ui,
                        &format!(
                            "{} {}",
                            t.stat_block_height,
                            dashboard::group_thousands(self.stats.height)
                        ),
                        None,
                        false,
                    );
                }
                let mining = self.miner_badge_state() == theme::MinerBadgeState::Mining;
                dashboard::chip(
                    ui,
                    self.miner_status_label(),
                    Some(if mining {
                        colors::ACCENT
                    } else {
                        colors::TEXT_DIM
                    }),
                    mining,
                );
            });
        });
    }

    /// The endpoint chip. It says where the worker points, which the panel
    /// knows for certain; it does not say a latency, because nothing in this
    /// build measures one continuously.
    fn dash_connection_chip(&self, t: &Strings) -> String {
        let mode = match self.connect_mode {
            ConnectMode::Solo => t.connect_solo,
            ConnectMode::Pool => t.connect_pool,
        };
        format!("{mode} · {}", self.connect)
    }

    // -- the four headline cards -------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn dash_kpis(
        &self,
        ui: &mut egui::Ui,
        t: &Strings,
        d: &DashLabels,
        // The headline row is not four equal cards. Mockup 1 gives the hash rate
        // card the room its sparkline needs and squeezes the other three; the
        // caller does that arithmetic once, against the real available width.
        wide: f32,
        narrow: f32,
        spark: &[f32],
        summary: Option<&dashboard::Summary>,
        window: ChartWindow,
    ) {
        let s = &self.stats;
        let is_hacd = self.mining_kind == MiningKind::Hacd;
        let mining = self.mining;

        // 1. Hash rate, straight from the worker, plus the trend the panel has
        //    measured for itself.
        let (value, unit) = if s.hashrate_display.trim().is_empty() {
            (t.dash_no_data.to_string(), String::new())
        } else {
            dashboard::split_value_unit(&s.hashrate_display)
        };
        let devices = if is_hacd {
            format!("CPU {}", dashboard::format_hashrate(s.cpu_hashrate_hps))
        } else if s.cpu_hashrate_hps > 0.0 {
            format!(
                "GPU {} · CPU {}",
                dashboard::format_hashrate(s.gpu_hashrate_hps),
                dashboard::format_hashrate(s.cpu_hashrate_hps)
            )
        } else {
            format!("GPU {}", dashboard::format_hashrate(s.gpu_hashrate_hps))
        };
        // The trend line is amber only when there is a trend. "Collecting
        // samples" set in the same accent as a measurement would read as one.
        let delta = summary.and_then(|sum| dashboard::delta_vs_average(sum, s.hashrate_hps));
        let trend = match delta {
            Some(delta) => format!(
                "{}{delta:.1}% {}",
                if delta >= 0.0 { "+" } else { "" },
                d.vs_average_display(window.label())
            ),
            None => d.collecting.to_string(),
        };
        dashboard::kpi_card(
            ui,
            wide,
            &Kpi {
                label: t.stat_hashrate,
                value: &value,
                unit: &unit,
                sub: &devices,
                foot: &trend,
                foot_accent: delta.is_some(),
                spark,
                highlight: mining,
            },
        );

        // 2. What the worker expects to earn, or the diamond it is chasing.
        if is_hacd {
            let number = if s.diamond_number > 0 {
                dashboard::group_thousands(s.diamond_number as u64)
            } else {
                t.dash_no_data.to_string()
            };
            let best = if s.diamond_best.is_empty() {
                t.dash_no_data.to_string()
            } else {
                s.diamond_best.clone()
            };
            dashboard::kpi_card(
                ui,
                narrow,
                &Kpi {
                    label: t.dash_detail_diamond,
                    value: &number,
                    unit: "",
                    sub: &format!("{}: {best}", t.stat_diamond_best),
                    // The CPU miner skips the x16rs rounds for any nonce whose
                    // sha3 already fails the difficulty check, so `best` is now
                    // sampled from ~11% of nonces and reads weaker than it used
                    // to. Without this line an operator sees the regression and
                    // not the reason. It is display only: the diamonds actually
                    // found and submitted are unchanged.
                    foot: t.stat_diamond_best_hint,
                    foot_accent: false,
                    spark: &[],
                    highlight: false,
                },
            );
        } else {
            let share = if s.network_pct > 0.0 {
                format!("{} {:.6}%", d.network_share, s.network_pct)
            } else {
                d.not_measured.to_string()
            };
            dashboard::kpi_card(
                ui,
                narrow,
                &Kpi {
                    label: t.stat_hac_day,
                    value: &format!("{:.4}", s.hac_per_day),
                    unit: "HAC",
                    sub: &share,
                    foot: "",
                    foot_accent: false,
                    spark: &[],
                    highlight: false,
                },
            );
        }

        // 3. Net per day, with the two halves it is made of underneath.
        let money = |eur: f64| {
            self.currency
                .format_amount(Currency::convert(eur, Currency::Eur, self.currency))
        };
        // The worker computes revenue as `hac_per_day * hac_price`, so a rig
        // that is earning HAC and still reports no revenue at all is a rig
        // whose price nobody has set. Net is then exactly minus the power bill,
        // and this card used to print that as a confident "-0.71 / day" on a
        // machine genuinely earning 3.28 HAC a day: an operator cannot tell
        // "you are losing money" from "nobody told me what HAC is worth".
        //
        // The power cost IS measured and stays on the card. The money it would
        // be subtracted from is not knowable, so net is not drawn as a number,
        // and the card says what would make it one. It is read from the
        // snapshot rather than from this panel's own price field because the
        // snapshot is what produced the figures being drawn; a price typed but
        // not yet carried into a running worker would otherwise put the
        // invented loss straight back.
        //
        // Only the display changes. `should_pause_for_profit` already refuses
        // to pause mining while the price is unset, and must keep doing so.
        let price_unset = hac_price_unset(s.hac_per_day, s.daily_revenue_eur);
        let net_value = if price_unset {
            t.dash_no_data.to_string()
        } else {
            money(s.daily_net_eur)
        };
        let net_sub = if price_unset {
            format!("{}: -{}", d.net_cost_only, money(s.daily_cost_eur))
        } else {
            format!(
                "+{} · -{}",
                money(s.daily_revenue_eur),
                money(s.daily_cost_eur)
            )
        };
        dashboard::kpi_card(
            ui,
            narrow,
            &Kpi {
                label: t.stat_net_day,
                value: &net_value,
                unit: "",
                sub: &net_sub,
                foot: if price_unset {
                    d.net_price_unset
                } else {
                    self.mode_label(self.mode_idx)
                },
                foot_accent: false,
                spark: &[],
                highlight: false,
            },
        );

        // 4. Efficiency, and the draw it is divided by - which since the card's
        //    own power sensor exists is a measurement on a rig that has one and
        //    a configured estimate on a rig that does not. The label says which,
        //    every time, because a 256 W reading and a 350 W guess divided into
        //    the same hash rate give two different efficiencies and only one of
        //    them is true.
        dashboard::kpi_card(
            ui,
            narrow,
            &Kpi {
                label: t.stat_efficiency,
                value: &format!("{:.1}", s.kh_per_j),
                unit: "kH/J",
                sub: &format!("{:.0} W · {}", s.watts, self.power_label(t).to_lowercase()),
                foot: if is_hacd {
                    ""
                } else {
                    dashboard::str_or(&s.gpu_profile, &self.gpu_profile)
                },
                foot_accent: false,
                spark: &[],
                highlight: false,
            },
        );
    }

    // -- the wide chart -----------------------------------------------------

    fn dash_performance(
        &self,
        ui: &mut egui::Ui,
        d: &DashLabels,
        window: ChartWindow,
        now_ms: u64,
        summary: Option<&dashboard::Summary>,
        gpu_errors: u32,
    ) -> Option<ChartWindow> {
        let mut picked = None;
        dashboard::card_head(ui, d.perf_title, d.perf_sub, |ui| {
            picked = dashboard::window_selector(ui, window);
        });
        ui.add_space(10.0);

        let segments = {
            let live = dashboard::live();
            live.series.segments(now_ms, window.span_ms())
        };
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, CHART_H), egui::Sense::hover());
        dashboard::area_chart(
            ui,
            rect,
            &ChartPlot {
                segments: &segments,
                now_ms,
                window,
                empty_note: d.collecting,
            },
        );

        ui.add_space(10.0);
        let cell = ((width - GAP * 3.0) / 4.0).floor();
        // With nothing measured yet these read "-", the panel's own mark for
        // "no data". The chart above already says why in full words, and a
        // sentence in a box sized for "4.66 GH/s" would wrap into two lines.
        let t = self.t();
        let (average, peak) = match summary {
            Some(s) => (
                dashboard::format_hashrate(s.avg),
                dashboard::format_hashrate(s.peak),
            ),
            None => (t.dash_no_data.to_string(), t.dash_no_data.to_string()),
        };
        let uptime = match self.worker_started_at {
            Some(started) => dashboard::format_uptime(started.elapsed().as_secs()),
            None => t.stopped_status.to_string(),
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            dashboard::mini_stat(ui, cell, d.stat_average, &average, colors::TEXT);
            dashboard::mini_stat(ui, cell, d.stat_peak, &peak, colors::TEXT);
            dashboard::mini_stat(
                ui,
                cell,
                d.stat_gpu_errors,
                &gpu_errors.to_string(),
                if gpu_errors > 0 {
                    colors::RED
                } else {
                    colors::TEXT
                },
            );
            dashboard::mini_stat(ui, cell, d.stat_uptime, &uptime, colors::TEXT);
        });
        picked
    }

    // -- hardware and safety ------------------------------------------------

    fn dash_hardware(&self, ui: &mut egui::Ui, t: &Strings, d: &DashLabels) {
        let s = &self.stats;
        let is_hacd = self.mining_kind == MiningKind::Hacd;
        let top = ui.cursor().top();

        dashboard::card_head(ui, d.hw_title, d.hw_sub, |_ui| {});
        ui.add_space(12.0);

        // Device identity.
        let (device, bus) = if is_hacd {
            (
                self.cpu_label(self.cpu_idx).to_string(),
                format!("CPU · {}", t.mining_hacd),
            )
        } else {
            (
                self.gpu_label(self.gpu_idx).to_string(),
                format!(
                    "{} · {} {} · Device {}",
                    if self.use_cuda { "CUDA" } else { "OpenCL" },
                    t.platform,
                    self.platform_id,
                    self.device_id
                ),
            )
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(11.0), colors::NAV_ACTIVE_BG);
            ui.painter().rect_stroke(
                rect,
                egui::Rounding::same(11.0),
                egui::Stroke::new(1.0, colors::BORDER_ACCENT),
            );
            dashboard::device_glyph(ui.painter(), rect);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(device)
                        .size(14.0)
                        .strong()
                        .color(colors::TEXT),
                );
                ui.add_space(1.0);
                ui.label(egui::RichText::new(bus).size(11.5).color(colors::TEXT_DIM));
            });
        });
        ui.add_space(10.0);

        // The ring, and the limits beside it.
        //
        // It is a thermometer whenever there is a temperature to put in it. The
        // sensor belongs to the worker, and the worker's snapshot now carries
        // what it read; an absent reading means no sensor answered, and then
        // the ring shows the work-group load the same snapshot reports instead.
        // Neither is ever drawn as a zero, and the empty ring on a machine with
        // no sensor says "not measured" rather than "0 degrees".
        //
        // A changed caption is not enough on its own, though: the ring keeps
        // its shape and its scale while the quantity inside it silently becomes
        // a work-group percentage, which an operator can read as a temperature
        // for hours. So whenever the instrument changes, the card says in words
        // that no sensor answered.
        //
        // Everything the ring can show comes out of one snapshot, and a snapshot
        // stops arriving long before a worker is declared gone: a thermal pause
        // returns from the mining item without writing any stats, and so does a
        // worker that hung. So the snapshot is aged first, and a stale one draws
        // the same empty ring an absent reading does. A frozen number under a
        // live-looking gauge is the failure this guards against, and on the
        // thermal path it would freeze at exactly the moment the reading matters.
        let now_ms = crate::stats_poll::now_unix_ms();
        let live = crate::stats_poll::snapshot_is_live(s, now_ms);
        let measured_temp = if is_hacd {
            None
        } else {
            crate::stats_poll::live_gpu_temp_c(s, now_ms)
        };
        let (fraction, centre, caption, too_hot) = if let Some(temp) = measured_temp {
            let (filled, centre, over_limit) = temperature_gauge(temp, self.max_temp_c);
            (Some(filled), centre, d.gauge_temp, over_limit)
        } else if is_hacd {
            let configured = self.configured_cpu_threads();
            // The configured count is the panel's own and always true. The
            // active count is the worker's, so it is used only while its
            // snapshot is current.
            let active = if live && s.active_cpu_threads > 0 {
                s.active_cpu_threads
            } else {
                configured
            };
            if configured > 0 {
                (
                    Some(active as f32 / configured as f32),
                    format!("{active}"),
                    d.gauge_threads,
                    false,
                )
            } else {
                (None, t.dash_no_data.to_string(), d.gauge_threads, false)
            }
        } else if live && s.effective_work_groups > 0 && s.configured_work_groups > 0 {
            let f = s.effective_work_groups as f32 / s.configured_work_groups as f32;
            (
                Some(f),
                format!("{:.0}%", f.min(1.0) * 100.0),
                d.gauge_load,
                false,
            )
        } else {
            (None, t.dash_no_data.to_string(), d.gauge_load, false)
        };
        // A cap that a stale snapshot reported may have been lifted an hour ago,
        // so only a current one turns the ring red. And a clamp is a SHORTFALL
        // against the configured count, never the mere presence of a work-group
        // number: this line used to read `oom_work_groups > 0`, which is true on
        // every healthy GPU and painted the ring red on a rig running all 48 of
        // its 48 work groups.
        let capped = hardware_ring_is_red(too_hot, s, now_ms);

        ui.horizontal_top(|ui| {
            // 100, the mockup's outer diameter, measured from the amber at the
            // top of its ring (y384) to the amber at the bottom (y485). Ours
            // was 110.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(100.0, 100.0), egui::Sense::hover());
            dashboard::gauge_ring(
                ui.painter(),
                rect,
                fraction,
                &centre,
                caption,
                if capped { colors::RED } else { colors::ACCENT },
            );
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let rows = self.dash_limit_rows(t, d);
                let last = rows.len().saturating_sub(1);
                for (i, (label, value)) in rows.iter().enumerate() {
                    dashboard::limit_row(ui, label, value, i == last);
                }
            });
        });

        if say_no_sensor_answered(is_hacd, live, measured_temp) {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(d.temp_no_sensor)
                    .size(11.0)
                    .color(colors::TEXT_DIM),
            );
        }

        // The four guards, pinned to the foot of the card as in the mockup.
        let used = ui.cursor().top() - top;
        let footer = 54.0;
        let rest = MID_ROW_H - used - footer;
        if rest > 0.0 {
            ui.add_space(rest);
        }
        let (line, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter().rect_filled(
            line,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(24, 24, 24),
        );
        ui.add_space(9.0);
        let half = (ui.available_width() - 10.0) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.spacing_mut().item_spacing.y = 6.0;
                dashboard::guard_dot(ui, self.max_temp_c > 0, d.guard_thermal);
                dashboard::guard_dot(
                    ui,
                    self.restart_attempts < MAX_RESTART_ATTEMPTS
                        && !self.worker_stop_needs_restart(),
                    d.guard_restart,
                );
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.spacing_mut().item_spacing.y = 6.0;
                // The panel always writes oom_fallback = true into the worker
                // config, so this dot reports a setting it controls.
                dashboard::guard_dot(ui, !is_hacd, d.guard_oom);
                dashboard::guard_dot(ui, self.pause_unprofitable && !is_hacd, d.guard_profit);
            });
        });
    }

    /// The label for the `watts` figure: "measured" only where the worker says
    /// the WHOLE total came from sensors, "estimate" everywhere else.
    ///
    /// A rig with a measured card and CPU assist threads reads "estimate" here,
    /// which is not a demotion of the measurement but the truth about the total:
    /// the CPU part of it can only ever be `cpu_watts_per_thread` from the ini.
    /// The measured card is shown on its own in the detail rows, so nothing real
    /// is hidden by the honest label on the sum.
    fn power_label(&self, t: &Strings) -> &'static str {
        let now_ms = crate::stats_poll::now_unix_ms();
        if crate::stats_poll::watts_are_measured(&self.stats, now_ms) {
            t.stat_power_measured
        } else {
            t.stat_power
        }
    }

    /// The detail row for a measured GPU board draw, or nothing at all where no
    /// card measured one. Absent means absent all the way to the pixels: there
    /// is no "0 W" row and no greyed-out placeholder, because either the sensor
    /// answered or the operator has only the estimate above.
    fn measured_gpu_power_row(&self, t: &Strings) -> Option<(String, String)> {
        let now_ms = crate::stats_poll::now_unix_ms();
        let watts = crate::stats_poll::live_gpu_board_power_w(&self.stats, now_ms)?;
        Some((t.stat_gpu_board_power.to_string(), format!("{watts:.0} W")))
    }

    fn dash_limit_rows(&self, t: &Strings, d: &DashLabels) -> Vec<(String, String)> {
        let s = &self.stats;
        let is_hacd = self.mining_kind == MiningKind::Hacd;
        let mut rows = Vec::new();
        if is_hacd {
            let configured = self.configured_cpu_threads();
            let active = if s.active_cpu_threads > 0 {
                s.active_cpu_threads
            } else {
                configured
            };
            rows.push((
                t.stat_cpu_threads.to_string(),
                format!("{active} / {configured}"),
            ));
            // HACD is CPU-only: there is no GPU board to measure, so this row
            // is always the configured per-thread estimate and says so.
            rows.push((t.stat_power.to_string(), format!("{:.0} W", s.watts)));
            rows.push((
                t.dash_detail_wallet.to_string(),
                if self.wallet.trim().is_empty() {
                    t.dash_no_data.to_string()
                } else {
                    Self::truncate_wallet(&self.wallet)
                },
            ));
            return rows;
        }
        rows.push((
            t.dash_detail_max_temp.to_string(),
            if self.max_temp_c == 0 {
                d.limit_off.to_string()
            } else {
                format!("{}°C", self.max_temp_c)
            },
        ));
        rows.push((self.power_label(t).to_string(), format!("{:.0} W", s.watts)));
        // The card's own reading, on its own row, whenever one exists. On a rig
        // with CPU assist the total above is honestly labelled an estimate, and
        // this is where the part that really was measured stays visible.
        rows.extend(self.measured_gpu_power_row(t));
        // When a cap has actually bitten, the row says which one. "1,536 /
        // 1,536" and "768 / 1,536 because the card got hot" are different
        // facts, and only the second explains a hash rate that dropped. Which
        // means the breakdown may only appear when a cap really bit: naming an
        // OOM cap of 48 on a rig configured for 48 invents the very explanation
        // this row exists to give honestly.
        let oom_clamp = crate::stats_poll::oom_clamp(s);
        let thermal_clamp = crate::stats_poll::thermal_clamp(s);
        rows.push((
            t.dash_detail_effective_wg.to_string(),
            if s.configured_work_groups == 0 {
                dashboard::group_thousands(self.work_groups as u64)
            } else if oom_clamp.is_some() || thermal_clamp.is_some() {
                t.wg_breakdown_display(
                    s.effective_work_groups,
                    s.configured_work_groups,
                    oom_clamp,
                    thermal_clamp,
                )
            } else {
                format!(
                    "{} / {}",
                    dashboard::group_thousands(s.effective_work_groups as u64),
                    dashboard::group_thousands(s.configured_work_groups as u64)
                )
            },
        ));
        rows.push((
            d.hw_unit_size.to_string(),
            dashboard::group_thousands(self.unit_size as u64),
        ));
        // "4 / 0" is not a ratio, it is two facts printed as one. The panel
        // configures the second number, so when it is zero there is no
        // denominator to divide by: either CPU assist is off and nothing is
        // running, which the row says in words, or the worker is running threads
        // this panel did not ask for, and then the count it reported is the
        // whole of what is known.
        //
        // The picker's number and the written number are not always the same for
        // a GPU rig: CPU assist is capped so the card's feed thread keeps a core.
        // This row shows what was written, because that is what is running.
        let configured = self.configured_cpu_threads();
        rows.push((
            t.stat_cpu_threads.to_string(),
            if configured > 0 {
                format!("{} / {configured}", s.active_cpu_threads)
            } else if s.active_cpu_threads > 0 {
                s.active_cpu_threads.to_string()
            } else {
                d.limit_off.to_string()
            },
        ));
        rows
    }

    // -- bottom row ---------------------------------------------------------

    fn dash_pool(&self, ui: &mut egui::Ui, t: &Strings, d: &DashLabels) {
        let money = self.pool_money();
        if !dashboard::shows_anything(&money.view, t) {
            dashboard::card_head(ui, d.pool_title, d.pool_sub, |_ui| {});
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(d.pool_none)
                    .size(12.5)
                    .color(colors::TEXT_MUTED),
            );
            return;
        }
        let connected = matches!(money.view, crate::stats_poll::PayoutView::Earnings(_));
        let (endpoint, worker) = match self.connect_mode {
            ConnectMode::Pool => (
                self.pool_display_name(),
                Self::truncate_wallet(&self.wallet),
            ),
            ConnectMode::Solo => (String::new(), String::new()),
        };
        dashboard::show_pool_money(
            ui,
            &dashboard::PoolMoneyPanel {
                t,
                d,
                money: &money,
                endpoint: &endpoint,
                connected,
                worker_display: &worker,
            },
        );
    }

    /// The pool's directory name when the endpoint matches one, otherwise the
    /// endpoint itself. Never a claim the panel cannot back.
    fn pool_display_name(&self) -> String {
        let connect = self.connect.trim();
        for pool in &self.pool_directory {
            if pool.connect.trim() == connect && !pool.name.trim().is_empty() {
                return pool.name.trim().to_string();
            }
        }
        connect.to_string()
    }

    fn dash_events(
        &self,
        ui: &mut egui::Ui,
        d: &DashLabels,
        events: &[dashboard::WorkerEvent],
        now_ms: u64,
        show_log: bool,
    ) {
        dashboard::card_head(ui, d.events_title, "", |_ui| {});
        ui.add_space(12.0);
        if events.is_empty() {
            ui.label(
                egui::RichText::new(d.events_empty)
                    .size(12.5)
                    .color(colors::TEXT_MUTED),
            );
            return;
        }
        let body = |ui: &mut egui::Ui| {
            for event in events {
                dashboard::event_row(ui, event, dashboard::event_title(d, event.kind), now_ms);
            }
        };
        if show_log {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(BOTTOM_ROW_H - 56.0)
                .show(ui, body);
        } else {
            body(ui);
        }
    }

    /// Returns the action the operator asked for, and whether the worker log
    /// should be toggled.
    fn dash_control(
        &self,
        ui: &mut egui::Ui,
        t: &Strings,
        d: &DashLabels,
        show_log: bool,
    ) -> (Option<Action>, bool) {
        let mut action = None;
        let mut toggle = false;
        dashboard::card_head(ui, d.control_title, d.control_sub, |_ui| {});
        ui.add_space(12.0);

        let worker_active = self.worker_operation_active();
        let benchmarking = self.benchmark_operation_active();
        let probing = self.opencl_probe_active();

        // The one obvious action, whatever it is right now.
        if self.worker_stopping() {
            ui.add_enabled(
                false,
                egui::Button::new(d.btn_busy).min_size(egui::vec2(ui.available_width(), 38.0)),
            );
        } else if self.worker_stop_needs_restart() {
            // The stop was never confirmed, so this panel genuinely cannot stop
            // the worker any more. The button stays in place and greyed rather
            // than disappearing, and the status line under it says what to do.
            ui.add_enabled(
                false,
                egui::Button::new(d.btn_stop_safely)
                    .min_size(egui::vec2(ui.available_width(), 38.0)),
            );
        } else if worker_active {
            let starting = self.pending_start.is_some() || self.restart_worker.is_some();
            let label = if starting {
                d.btn_cancel
            } else {
                d.btn_stop_safely
            };
            if dashboard::stack_button(ui, label, ButtonStyle::Primary).clicked() {
                action = Some(Action::Stop);
            }
        } else if !benchmarking
            && !probing
            && dashboard::stack_button(ui, t.btn_start, ButtonStyle::Primary).clicked()
        {
            action = Some(Action::Start);
        }
        ui.add_space(8.0);

        // Auto Tune, and the cancels that take its place while it runs.
        if probing {
            if dashboard::stack_button(ui, d.btn_cancel, ButtonStyle::Danger).clicked() {
                action = Some(Action::CancelOpenCl);
            }
        } else if benchmarking {
            let stopping = self.benchmark_stopping();
            if stopping {
                ui.add_enabled(
                    false,
                    egui::Button::new(d.btn_busy).min_size(egui::vec2(ui.available_width(), 38.0)),
                );
            } else {
                let label = if self.benchmarking {
                    d.btn_cancel
                } else {
                    d.btn_restore
                };
                if dashboard::stack_button(ui, label, ButtonStyle::Danger).clicked() {
                    action = Some(Action::CancelAutoTune);
                }
            }
        } else {
            let enabled = !worker_active && self.mining_kind == MiningKind::Hac;
            ui.add_enabled_ui(enabled, |ui| {
                if dashboard::stack_button(ui, d.btn_auto_tune, ButtonStyle::Secondary).clicked() {
                    action = Some(Action::AutoTune);
                }
            });
        }
        ui.add_space(8.0);

        if dashboard::stack_button(
            ui,
            if show_log {
                d.btn_hide_log
            } else {
                d.btn_worker_log
            },
            ButtonStyle::Secondary,
        )
        .clicked()
        {
            toggle = true;
        }

        ui.add_space(10.0);
        if self.mining_settings_locked() {
            ui.label(
                egui::RichText::new(d.control_locked)
                    .size(10.5)
                    .color(colors::TEXT_DIM),
            );
        }
        // `status_msg` is not repeated here. The window's own status bar carries
        // it on every screen, and a long one, such as the OpenCL diagnostic with
        // a full path in it, was being printed twice on the same page: once in
        // this card and once eighty pixels below it.
        (action, toggle)
    }

    // -- the strip under everything ----------------------------------------

    fn dash_footer(&self, ui: &mut egui::Ui, t: &Strings, d: &DashLabels) {
        egui::Frame::none()
            .fill(colors::BG_CARD)
            .stroke(egui::Stroke::new(1.0, colors::BORDER_SOFT))
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::symmetric(18.0, 11.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    // Gold while the node is catching up, amber while mining,
                    // dim otherwise. The strip is the one thing visible from
                    // the bottom of the page, so its dot has to mean something.
                    let colour = if self.sync_status.is_some() {
                        colors::GOLD
                    } else if self.miner_badge_state() == theme::MinerBadgeState::Mining {
                        colors::ACCENT
                    } else {
                        colors::TEXT_DIM
                    };
                    ui.painter().circle_filled(dot.center(), 3.5, colour);

                    // The node row exists only where the panel has actually
                    // asked the node where it is. It probes during a solo start
                    // and at no other time, and inventing "synced" out of
                    // silence is exactly the failure that probe was added for.
                    match &self.sync_status {
                        Some(sync) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}: {}",
                                    t.sync_title,
                                    dashboard::group_thousands(sync.height)
                                ))
                                .size(11.5)
                                .color(colors::TEXT_MUTED),
                            );
                            ui.add(
                                egui::ProgressBar::new(sync.progress)
                                    .desired_width(160.0)
                                    .desired_height(6.0),
                            );
                            ui.label(
                                egui::RichText::new(format!("{:.0}%", sync.progress * 100.0))
                                    .size(11.5)
                                    .strong()
                                    .color(colors::TEXT),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "· {} {}",
                                    dashboard::group_thousands(sync.blocks_behind()),
                                    t.sync_behind
                                ))
                                .size(11.5)
                                .color(colors::TEXT_DIM),
                            );
                        }
                        None if self.stats.height > 0 => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    t.stat_block_height,
                                    dashboard::group_thousands(self.stats.height)
                                ))
                                .size(11.5)
                                .color(colors::TEXT_MUTED),
                            );
                            ui.label(
                                egui::RichText::new(d.node_not_probed)
                                    .size(11.5)
                                    .color(colors::TEXT_DIM),
                            );
                        }
                        None => {
                            ui.label(
                                egui::RichText::new(d.node_not_probed)
                                    .size(11.5)
                                    .color(colors::TEXT_DIM),
                            );
                        }
                    }

                    ui.label(
                        egui::RichText::new(format!(
                            "· {}",
                            if self.stats.updated_unix_ms == 0 {
                                d.telemetry_never.to_string()
                            } else {
                                d.telemetry_age_display(&Self::format_stats_age(
                                    self.stats.updated_unix_ms,
                                ))
                            }
                        ))
                        .size(11.5)
                        .color(colors::TEXT_DIM),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let retrying = self.restart_attempts > 0 || self.restart_worker.is_some();
                        let text = format!(
                            "{} · {}",
                            t.status_recovery,
                            if retrying {
                                d.retries_display(self.restart_attempts, MAX_RESTART_ATTEMPTS)
                            } else {
                                t.status_ready.to_string()
                            }
                        );
                        ui.label(egui::RichText::new(text).size(11.5).color(if retrying {
                            colors::RED
                        } else {
                            colors::ACCENT
                        }));
                    });
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GAUGE_TEMP_SCALE_C, abbreviated_wallet, hac_price_unset, hardware_ring_is_red,
        say_no_sensor_answered, temperature_gauge,
    };
    use app::efficiency::MiningStatsSnapshot;

    /// The operator's rig, exactly as their miner-stats.json reads this minute.
    fn operators_snapshot() -> MiningStatsSnapshot {
        MiningStatsSnapshot {
            configured_work_groups: 48,
            oom_allowed_work_groups: 48,
            thermal_cap_work_groups: 0,
            effective_work_groups: 48,
            gpu_temp_c: None,
            updated_unix_ms: crate::stats_poll::now_unix_ms(),
            ..Default::default()
        }
    }

    #[test]
    fn a_healthy_rig_running_every_work_group_never_paints_the_ring_red() {
        let now_ms = crate::stats_poll::now_unix_ms();
        let healthy = operators_snapshot();

        // The card publishes no temperature, so there is no reading to be over a
        // limit: the ring is the work-group gauge and the thermal half of the
        // colour decision is false by construction, not by assumption.
        assert_eq!(crate::stats_poll::live_gpu_temp_c(&healthy, now_ms), None);
        let too_hot = crate::stats_poll::live_gpu_temp_c(&healthy, now_ms)
            .is_some_and(|temp| temperature_gauge(temp, 82).2);
        assert!(!too_hot);

        assert!(
            !hardware_ring_is_red(too_hot, &healthy, now_ms),
            "48 of 48 work groups on a card that never OOM'd is a healthy rig"
        );

        // And a genuine shortfall still is an alarm: the fix removed a false
        // positive, not the warning.
        let mut clamped = operators_snapshot();
        clamped.oom_allowed_work_groups = 16;
        clamped.effective_work_groups = 16;
        assert_eq!(crate::stats_poll::oom_clamp(&clamped), Some(16));
        assert!(hardware_ring_is_red(false, &clamped, now_ms));

        // A measured over-limit temperature turns it red whatever the work
        // groups say, which is the other half of the same expression.
        assert!(hardware_ring_is_red(true, &healthy, now_ms));
    }

    #[test]
    fn a_rig_earning_hac_with_no_price_set_is_not_shown_as_a_loss() {
        // The operator's rig this minute: 3.2802 HAC/day, hac_price = 0, so the
        // worker computes revenue 0.00 and net -0.7056. That net is the power
        // bill wearing a revenue figure nobody supplied, and the card must not
        // print it as money lost.
        assert!(hac_price_unset(3.280_277_997_716_101_6, 0.0));

        // With a price set the whole figure is real and is shown.
        assert!(!hac_price_unset(3.28, 1.4));
        // Earning nothing is a measurement, not a missing price: revenue is
        // zero at any price, so minus the power bill is the true net. This is
        // also every HACD snapshot, which publishes no HAC per day at all and
        // keeps the card it has always had.
        assert!(!hac_price_unset(0.0, 0.0));
    }

    #[test]
    fn a_ring_that_stopped_being_a_thermometer_says_so() {
        // The case this exists for: a GPU worker reporting normally, with no
        // temperature in the snapshot. The ring switches to work-group load and
        // the card has to admit it.
        assert!(say_no_sensor_answered(false, true, None));

        // A measurement needs no explanation.
        assert!(!say_no_sensor_answered(false, true, Some(61.0)));
        // A stale snapshot is silence about everything, sensors included, and
        // claiming "no sensor answered" there would be inventing a fact.
        assert!(!say_no_sensor_answered(false, false, None));
        // HACD mines on the CPU: the ring is a thread gauge by design, not by
        // a failure worth reporting.
        assert!(!say_no_sensor_answered(true, true, None));
        assert!(!say_no_sensor_answered(true, false, None));
    }

    #[test]
    fn the_temperature_ring_fills_towards_the_operators_own_limit() {
        let (fill, centre, over) = temperature_gauge(67.0, 82);
        assert!((fill - 67.0 / 82.0).abs() < 1e-6, "{fill}");
        assert_eq!(centre, "67°");
        assert!(!over);

        // At the limit the guard acts on, and the ring turns with it.
        let (fill, _, over) = temperature_gauge(82.0, 82);
        assert_eq!(fill, 1.0);
        assert!(over);
        assert!(temperature_gauge(95.0, 82).2);
    }

    #[test]
    fn with_no_limit_configured_the_ring_still_shows_the_measurement() {
        let (fill, centre, over) = temperature_gauge(50.0, 0);
        assert!((fill - 50.0 / GAUGE_TEMP_SCALE_C).abs() < 1e-6);
        assert_eq!(centre, "50°");
        assert!(
            !over,
            "a limit that was never set cannot have been exceeded"
        );
    }

    #[test]
    fn the_ring_never_overflows_whatever_the_sensor_says() {
        for temp in [0.5f32, 40.0, 119.0] {
            let (fill, _, _) = temperature_gauge(temp, 60);
            assert!((0.0..=1.0).contains(&fill), "{temp} gave {fill}");
        }
    }

    #[test]
    fn wallet_abbreviation_is_unicode_safe() {
        let wallet = "παράδειγμα-πορτοφόλι-δοκιμής";
        let abbreviated = abbreviated_wallet(wallet);
        assert!(abbreviated.contains('…'));
        assert!(abbreviated.starts_with("παράδειγ"));
        assert!(abbreviated.ends_with("οκιμής"));
    }

    #[test]
    fn short_wallet_is_preserved() {
        assert_eq!(abbreviated_wallet("  1ShortWallet  "), "1ShortWallet");
    }
}
