use app::efficiency::MiningStatsSnapshot;
use eframe::egui;
use field::Amount;

use crate::i18n::Strings;
use crate::mining_kind::MiningKind;
use crate::stats_poll::{
    PayoutView, PoolEarnings, PoolMoney, PoolTerms, format_age_secs, now_unix_ms,
};
use crate::theme;

pub struct DashboardDetails<'a> {
    pub t: &'a Strings,
    pub stats: &'a MiningStatsSnapshot,
    pub cpu_label: &'a str,
    pub gpu_label: &'a str,
    pub connect_display: String,
    pub wallet_display: String,
    pub opencl_display: String,
    pub tuning_display: String,
    pub stats_status: String,
    pub last_update: String,
    pub power_cost_display: String,
    pub max_temp_c: u32,
    pub mining_kind: MiningKind,
}

pub fn show_dashboard_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    ui.label(
        egui::RichText::new(d.t.dash_details_title)
            .size(14.0)
            .strong()
            .color(theme::colors::TEXT),
    );
    ui.add_space(10.0);

    if d.mining_kind == MiningKind::Hacd {
        show_hacd_details(ui, d);
    } else {
        show_hac_details(ui, d);
    }
}

fn show_hacd_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    egui::Grid::new("dash_details_hacd")
        .num_columns(2)
        .spacing([24.0, 8.0])
        .min_col_width(360.0)
        .show(ui, |ui| {
            theme::show_detail_row(ui, d.t.dash_detail_cpu, d.cpu_label);
            theme::show_detail_row(ui, "Backend", "CPU only • no OpenCL");
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_connect, &d.connect_display);
            theme::show_detail_row(ui, d.t.dash_detail_wallet, &d.wallet_display);
            ui.end_row();

            theme::show_detail_row(ui, "CPU configuration", &d.tuning_display);
            theme::show_detail_row(ui, d.t.dash_detail_power_cost, &d.power_cost_display);
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_last_update, &d.last_update);
            theme::show_detail_row(ui, d.t.dash_detail_stats_status, &d.stats_status);
            ui.end_row();

            if d.stats.diamond_number > 0 || !d.stats.diamond_best.is_empty() {
                let number = if d.stats.diamond_number > 0 {
                    format!("#{}", d.stats.diamond_number)
                } else {
                    d.t.dash_no_data.to_string()
                };
                theme::show_detail_row(ui, d.t.dash_detail_diamond, &number);
                theme::show_detail_row(
                    ui,
                    d.t.stat_diamond_best,
                    if d.stats.diamond_best.is_empty() {
                        d.t.dash_no_data
                    } else {
                        &d.stats.diamond_best
                    },
                );
                ui.end_row();
            }
        });
}

fn show_hac_details(ui: &mut egui::Ui, d: &DashboardDetails<'_>) {
    egui::Grid::new("dash_details_hac")
        .num_columns(2)
        .spacing([24.0, 8.0])
        .min_col_width(360.0)
        .show(ui, |ui| {
            theme::show_detail_row(ui, d.t.dash_detail_cpu, d.cpu_label);
            theme::show_detail_row(ui, d.t.dash_detail_gpu, d.gpu_label);
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_connect, &d.connect_display);
            theme::show_detail_row(ui, d.t.dash_detail_wallet, &d.wallet_display);
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_opencl, &d.opencl_display);
            theme::show_detail_row(ui, d.t.dash_detail_tuning, &d.tuning_display);
            ui.end_row();

            if d.stats.gpu_hashrate_hps > 0.0 || d.stats.effective_work_groups > 0 {
                theme::show_detail_row(
                    ui,
                    d.t.dash_detail_gpu_hashrate,
                    if d.stats.gpu_hashrate_hps > 0.0 {
                        &d.stats.gpu_hashrate_display
                    } else {
                        d.t.dash_no_data
                    },
                );
                let wg_detail = if d.stats.effective_work_groups > 0 {
                    d.t.wg_breakdown_display(
                        d.stats.effective_work_groups,
                        d.stats.configured_work_groups,
                        d.stats.oom_work_groups,
                        d.stats.thermal_cap_work_groups,
                    )
                } else {
                    d.t.dash_no_data.to_string()
                };
                theme::show_detail_row(ui, d.t.dash_detail_effective_wg, &wg_detail);
                ui.end_row();
            }

            theme::show_detail_row(ui, d.t.dash_detail_power_cost, &d.power_cost_display);
            theme::show_detail_row(
                ui,
                d.t.dash_detail_max_temp,
                if d.max_temp_c == 0 {
                    "Off".to_string()
                } else {
                    format!("{}°C", d.max_temp_c)
                }
                .as_str(),
            );
            ui.end_row();

            theme::show_detail_row(ui, d.t.dash_detail_last_update, &d.last_update);
            theme::show_detail_row(ui, d.t.dash_detail_stats_status, &d.stats_status);
            ui.end_row();
        });
}

// ---------------------------------------------------------------------------
// Pool payouts: what this miner is owed, and what it has already been paid.
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
        PayoutView::Unreachable { was_pool: false, .. } => None,
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
    pub money: &'a PoolMoney,
    /// Abbreviated payout address, empty when none is announced.
    pub worker_display: String,
}

pub fn show_pool_money(ui: &mut egui::Ui, p: &PoolMoneyPanel<'_>) {
    let t = p.t;
    let view = &p.money.view;
    ui.label(
        egui::RichText::new(t.pool_pay_title)
            .size(11.0)
            .strong()
            .color(theme::colors::ACCENT),
    );
    ui.add_space(8.0);

    let summary = PoolMoneySummary::for_view(view);
    if has_ledger(view) {
        // The number the owner asked for, large enough that nobody can miss it.
        ui.label(
            egui::RichText::new(t.pool_pay_total_paid)
                .size(12.5)
                .color(theme::colors::TEXT_MUTED),
        );
        ui.label(
            egui::RichText::new(summary.total_paid.display(t, t.pool_pay_nothing_paid))
                .size(30.0)
                .strong()
                .color(theme::colors::GOLD),
        );
        ui.add_space(10.0);

        egui::Grid::new("dash_pool_money")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .min_col_width(190.0)
            .show(ui, |ui| {
                theme::show_stat_card(
                    ui,
                    theme::colors::ACCENT,
                    t.pool_pay_owed,
                    summary.owed.display(t, t.pool_pay_nothing_owed),
                );
                theme::show_stat_card(
                    ui,
                    theme::colors::GOLD,
                    t.pool_pay_in_flight,
                    summary.in_flight.display(t, t.pool_pay_nothing_owed),
                );
                ui.end_row();
            });
        ui.add_space(10.0);
    }

    if let Some(explanation) = payout_explanation(view, t) {
        ui.label(
            egui::RichText::new(explanation)
                .size(12.5)
                .color(theme::colors::TEXT),
        );
        ui.add_space(8.0);
    }

    if let PayoutView::Earnings(earnings) = view {
        show_earnings_detail(ui, t, earnings, &p.worker_display);
    }

    if has_ledger(view) {
        ui.label(
            egui::RichText::new(t.pool_pay_estimate_note)
                .size(11.5)
                .color(theme::colors::TEXT_MUTED),
        );
        ui.label(
            egui::RichText::new(t.pool_pay_amount_note)
                .size(11.5)
                .color(theme::colors::TEXT_MUTED),
        );
    }

    if p.money.checked_unix_ms > 0 {
        let age = format_age_secs(now_unix_ms().saturating_sub(p.money.checked_unix_ms) / 1000);
        ui.label(
            egui::RichText::new(t.pool_pay_checked_display(&age))
                .size(11.5)
                .color(theme::colors::TEXT_MUTED),
        );
    }

    show_pool_terms(ui, t, p.money.terms.as_ref(), view);
}

fn show_earnings_detail(ui: &mut egui::Ui, t: &Strings, e: &PoolEarnings, worker_display: &str) {
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
    theme::show_detail_row(ui, t.pool_pay_last, &last);
    if let Some(tx) = e.last_payout.as_ref().map(|l| l.tx.as_str()).filter(|tx| !tx.is_empty()) {
        theme::show_detail_row(ui, t.pool_pay_last_tx, tx);
    }
    theme::show_detail_row(
        ui,
        t.pool_pay_shares,
        &match e.shares_in_window {
            Some(count) => count.to_string(),
            None => t.pool_pay_not_reported.to_string(),
        },
    );
    if !worker_display.is_empty() {
        theme::show_detail_row(ui, t.dash_detail_wallet, worker_display);
    }
    ui.add_space(6.0);
}

/// Terms are only worth a heading where a pool exists to have them. A full node
/// and a relay have no payout terms, and saying "terms unavailable" about them
/// would imply they should.
fn show_pool_terms(
    ui: &mut egui::Ui,
    t: &Strings,
    terms: Option<&PoolTerms>,
    view: &PayoutView,
) {
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

    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(t.pool_terms_title)
            .size(11.0)
            .strong()
            .color(theme::colors::ACCENT),
    );
    ui.add_space(6.0);

    let Some(terms) = terms else {
        ui.label(
            egui::RichText::new(t.pool_terms_unavailable)
                .size(12.5)
                .color(theme::colors::TEXT),
        );
        return;
    };

    for (label, value) in terms_rows(t, terms) {
        theme::show_detail_row(ui, label, &value);
    }
    ui.label(
        egui::RichText::new(t.pool_terms_source)
            .size(11.5)
            .color(theme::colors::TEXT_MUTED),
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
        assert_eq!(zero.display(&t, t.pool_pay_nothing_paid), t.pool_pay_nothing_paid);
        assert!(!zero.display(&t, t.pool_pay_nothing_paid).contains('0'));
    }

    #[test]
    fn the_same_zero_reads_differently_under_a_total_and_under_a_balance() {
        let t = en();
        let zero = money_cell(Some(&units_to_amount(0)));
        assert_eq!(zero.display(&t, t.pool_pay_nothing_paid), t.pool_pay_nothing_paid);
        assert_eq!(zero.display(&t, t.pool_pay_nothing_owed), t.pool_pay_nothing_owed);
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
        assert_eq!(summary.total_paid.display(&t, t.pool_pay_nothing_paid), "25:248");
        assert_eq!(summary.owed.display(&t, t.pool_pay_nothing_owed), "3:247");
        assert_eq!(
            summary.in_flight.display(&t, t.pool_pay_nothing_owed),
            t.pool_pay_not_reported
        );
        assert_eq!(payout_explanation(&view, &t), None);
        assert!(has_ledger(&view));
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
