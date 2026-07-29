use eframe::egui::{
    self, Color32, FontFamily, FontId, Frame, Margin, Rect, Rounding, Sense, Stroke, TextStyle, Ui,
    Vec2,
};

/// The palette, sampled from the commissioned mockups rather than invented.
///
/// The rule the mockups follow, and the one the old palette broke: structure is
/// NEUTRAL. Card borders, dividers and secondary text are warm greys, and amber
/// is spent only on the active nav item, the key numbers and their units, and
/// the single primary button on a screen. When every border was orange nothing
/// could stand out, because everything already did.
pub mod colors {
    use super::Color32;

    /// App background and sidebar. Not pure black: the cards sit on it at
    /// #0c0c0c and need something to separate from.
    pub const BG_DEEP: Color32 = Color32::from_rgb(5, 5, 5);
    pub const BG_PANEL: Color32 = Color32::from_rgb(5, 5, 5);
    pub const BG_HEADER: Color32 = Color32::from_rgb(7, 7, 7);
    pub const BG_CARD: Color32 = Color32::from_rgb(12, 12, 12);
    pub const BG_INPUT: Color32 = Color32::from_rgb(9, 9, 9);
    /// Hover / raised surface, one step above a card.
    pub const BG_RAISED: Color32 = Color32::from_rgb(20, 20, 20);
    /// Structural border, the one used on nearly everything.
    pub const BORDER_SOFT: Color32 = Color32::from_rgb(36, 36, 36);
    /// The same border, one step brighter: hover and emphasis.
    pub const BORDER: Color32 = Color32::from_rgb(58, 58, 58);
    /// Border of a card that is switched on. Amber, but heavily muted.
    pub const BORDER_ACCENT: Color32 = Color32::from_rgb(85, 62, 23);
    pub const TEXT: Color32 = Color32::from_rgb(245, 245, 245);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(139, 139, 139);
    /// Small uppercase labels and other text that must recede.
    pub const TEXT_DIM: Color32 = Color32::from_rgb(104, 104, 104);
    pub const GOLD: Color32 = Color32::from_rgb(247, 175, 52);
    pub const GOLD_DIM: Color32 = Color32::from_rgb(168, 132, 47);
    pub const ACCENT: Color32 = Color32::from_rgb(247, 175, 52);
    #[allow(dead_code)] // reserved for the screens
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(107, 77, 25);
    pub const SLIDER_TRACK: Color32 = Color32::from_rgb(38, 30, 14);
    /// Fill of the one primary button on a screen. Named GREEN for history; the
    /// mockups have no green anywhere.
    pub const GREEN: Color32 = Color32::from_rgb(242, 170, 48);
    pub const GREEN_DIM: Color32 = Color32::from_rgb(183, 124, 30);
    pub const RED: Color32 = Color32::from_rgb(232, 99, 95);
    /// Danger fill: nearly black with a red cast, the way the mockup sets a
    /// destructive button. It is worn by the cancels, not by an emergency stop:
    /// there is no such control, because `stop_mining` already terminates the
    /// worker immediately and a second, harsher-looking button beside it would
    /// promise a shutdown path this panel does not have.
    pub const RED_DIM: Color32 = Color32::from_rgb(30, 18, 17);
    pub const BLUE: Color32 = Color32::from_rgb(229, 163, 58);
    /// Fill behind the selected sidebar item.
    pub const NAV_ACTIVE_BG: Color32 = Color32::from_rgb(26, 20, 8);
}

use colors::*;

pub fn setup_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;

    *v = egui::Visuals::dark();
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_DEEP;
    v.extreme_bg_color = BG_INPUT;
    v.faint_bg_color = Color32::from_rgba_premultiplied(255, 255, 255, 10);
    v.hyperlink_color = BLUE;
    v.warn_fg_color = GOLD;

    let round = Rounding::same(8.0);
    v.widgets.noninteractive.rounding = round;
    v.widgets.inactive.rounding = round;
    v.widgets.hovered.rounding = round;
    v.widgets.active.rounding = round;
    v.widgets.open.rounding = round;

    v.widgets.inactive.bg_fill = SLIDER_TRACK; // visible slider rail on cards
    v.widgets.noninteractive.bg_fill = BG_CARD;
    v.widgets.hovered.bg_fill = BG_RAISED;
    v.widgets.active.bg_fill = Color32::from_rgb(38, 30, 14);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_SOFT);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SOFT);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.active.bg_stroke = Stroke::new(1.2, BORDER_ACCENT);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_MUTED);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, ACCENT);

    // `bg_fill` is NOT what a ComboBox or a plain Button paints itself with.
    // Both take their background from `weak_bg_fill`, and leaving that unset
    // left every select and every unstyled button on `Visuals::dark()`'s
    // from_gray(60): a #3C3C3C grey-plastic widget on a #050505 page. One field.
    v.widgets.noninteractive.weak_bg_fill = BG_INPUT;
    v.widgets.inactive.weak_bg_fill = BG_INPUT;
    v.widgets.hovered.weak_bg_fill = BG_RAISED;
    v.widgets.active.weak_bg_fill = Color32::from_rgb(38, 30, 14);
    // `open` is the state a ComboBox wears while its list is down, and it
    // inherits nothing from the four above.
    v.widgets.open.weak_bg_fill = BG_INPUT;
    v.widgets.open.bg_fill = BG_INPUT;
    v.widgets.open.bg_stroke = Stroke::new(1.0, BORDER_ACCENT);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);


    v.slider_trailing_fill = true;
    // `selectable_label` paints the fill from `selection.bg_fill` and the TEXT
    // from `selection.stroke`. A mid-amber fill under mid-amber text was the
    // muddiest thing on the old screens, so this is the mockup's active state
    // instead: near-black amber-tinted fill, full amber text.
    v.selection.bg_fill = NAV_ACTIVE_BG;
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    v.window_rounding = Rounding::same(14.0);
    v.window_stroke = Stroke::new(1.0, BORDER_SOFT);

    style.spacing.item_spacing = Vec2::new(14.0, 12.0);
    // 8, not 10: a 13.5px label in a button is an 18px row, so 8 gives the 34px
    // control the mockups draw. At 10 every select and every plain button stood
    // 40px tall, and the header strip they sit in stood 71px against the
    // mockup's 64.
    style.spacing.button_padding = Vec2::new(16.0, 8.0);
    // The scroll handle used to be the brightest non-amber thing on screen:
    // egui's floating bar paints from `fg_stroke.color`, which this theme sets
    // to near-white, at 0.6 opacity whenever the pointer is anywhere over the
    // page. The mockup has no scrollbar at all; a page that really does scroll
    // still needs one, so it is dimmed to the weight of a card border and only
    // brightens when it is actually grabbed.
    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 8.0;
    scroll.active_handle_opacity = 0.08;
    scroll.interact_handle_opacity = 0.75;
    scroll.active_background_opacity = 0.0;
    scroll.interact_background_opacity = 0.35;
    style.spacing.scroll = scroll;
    style.spacing.slider_width = 280.0;
    style.spacing.slider_rail_height = 8.0;
    style.spacing.indent = 18.0;
    style.spacing.window_margin = Margin::same(16.0);

    // The type scale the mockups use. Body drops from 14 to 13.5 and Small from
    // 12 to 11.5 so the small uppercase labels read as labels rather than as a
    // second body size, which is what made the old screens look flat.
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(21.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(13.5, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(13.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.5, FontFamily::Proportional),
    );

    ctx.set_style(style);
}

/// Type scale, one place, so a screen never has to guess a size.
///
/// These are the four roles the mockups actually use. Anything not on this list
/// is body text.
#[allow(dead_code)] // the screens are restyled separately; this is the scale they draw from
pub mod typo {
    /// 11px uppercase, dimmest grey. The label above a number or a field.
    pub const LABEL: f32 = 11.0;
    /// 30px light. The one number a card exists to show. 30 and not 34: the
    /// digits of "4.82" in mockup 1 stand 22px, and at 34 ours stood 25.
    pub const VALUE: f32 = 30.0;
    /// 15px amber, trailing a value on its baseline ("GH/s", "HAC").
    pub const UNIT: f32 = 15.0;
    /// 15px, the title of a card or a step.
    pub const CARD_TITLE: f32 = 15.0;
    /// 12.5px muted, the quiet line under a title or a value.
    pub const SUB: f32 = 12.5;
}

/// The drop-down marker the mockups draw: a 1px chevron, not egui's solid
/// triangle. Pass it to `ComboBox::icon` so every select on every screen carries
/// the same mark.
pub fn combo_chevron(
    ui: &Ui,
    rect: Rect,
    visuals: &egui::style::WidgetVisuals,
    _open: bool,
    above_or_below: egui::AboveOrBelow,
) {
    let c = rect.center();
    let (half_w, half_h) = (4.0, 2.5);
    let stroke = Stroke::new(1.2, visuals.fg_stroke.color.gamma_multiply(0.75));
    let tip_y = match above_or_below {
        egui::AboveOrBelow::Above => c.y - half_h,
        egui::AboveOrBelow::Below => c.y + half_h,
    };
    let painter = ui.painter();
    painter.line_segment(
        [
            egui::pos2(c.x - half_w, 2.0 * c.y - tip_y),
            egui::pos2(c.x, tip_y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(c.x + half_w, 2.0 * c.y - tip_y),
            egui::pos2(c.x, tip_y),
        ],
        stroke,
    );
}

/// The small uppercase label the mockups put above every number and field.
#[allow(dead_code)] // used by the screens, which are restyled separately
pub fn label_caps(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(typo::LABEL)
            .color(TEXT_DIM),
    );
}

/// A card's title line, with an optional quiet subtitle under it.
#[allow(dead_code)] // used by the screens, which are restyled separately
pub fn card_title(ui: &mut Ui, title: &str, sub: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(typo::CARD_TITLE)
            .strong()
            .color(TEXT),
    );
    if !sub.is_empty() {
        ui.add_space(2.0);
        ui.label(egui::RichText::new(sub).size(typo::SUB).color(TEXT_MUTED));
    }
}

/// A big number with its unit trailing on the same baseline, the way every KPI
/// in mockup 1 is set. The unit is amber, the number is white.
#[allow(dead_code)] // used by the screens, which are restyled separately
pub fn value_with_unit(ui: &mut Ui, value: &str, unit: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(egui::RichText::new(value).size(typo::VALUE).color(TEXT));
        if !unit.is_empty() {
            ui.label(egui::RichText::new(unit).size(typo::UNIT).color(ACCENT));
        }
    });
}

pub fn header_frame() -> Frame {
    Frame::none()
        .fill(BG_HEADER)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .inner_margin(Margin::symmetric(22.0, 12.0))
}

/// The window's own status bar. The mockups have no such band, so every pixel
/// it takes is a pixel the page below the fold pays for; it is kept because it
/// is the one place a status is visible from every screen, and trimmed to the
/// height of the chip inside it.
pub fn footer_frame() -> Frame {
    Frame::none()
        .fill(BG_HEADER)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .inner_margin(Margin::symmetric(22.0, 5.0))
}

pub fn content_frame() -> Frame {
    Frame::none()
        .fill(BG_DEEP)
        .inner_margin(Margin::symmetric(26.0, 22.0))
}

/// The left navigation column. One hairline on its right edge and nothing else,
/// so the page beside it is what the eye lands on.
pub fn sidebar_frame() -> Frame {
    Frame::none()
        .fill(BG_DEEP)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .inner_margin(Margin {
            left: 16.0,
            right: 16.0,
            top: 18.0,
            bottom: 16.0,
        })
}

pub fn section_card() -> Frame {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(20.0, 18.0))
}

/// A card that is switched on: same card, amber-tinted border. Used for the
/// selected option in a pair of choices.
#[allow(dead_code)] // used by the screens, which are restyled separately
pub fn section_card_active() -> Frame {
    Frame::none()
        .fill(NAV_ACTIVE_BG)
        .stroke(Stroke::new(1.0, BORDER_ACCENT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(20.0, 18.0))
}

pub fn show_detail_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(12.5).color(TEXT_MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).size(13.0).color(TEXT));
        });
    });
    ui.add_space(4.0);
}

pub fn show_stat_card(ui: &mut Ui, accent: Color32, title: &str, value: &str) {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(16.0, 15.0))
        .show(ui, |ui| {
            ui.set_min_width(160.0);
            ui.set_min_height(74.0);
            ui.horizontal(|ui| {
                let (stripe, _) =
                    ui.allocate_exact_size(Vec2::new(3.0, 40.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(stripe, Rounding::same(2.0), accent);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(title.to_uppercase())
                            .size(typo::LABEL)
                            .color(TEXT_DIM),
                    );
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(value).size(21.0).color(TEXT));
                });
            });
        });
}

pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(TEXT).size(13.5));
}

/// The quiet caption the setup mockup puts ABOVE a control, not beside it.
///
/// Sentence case, not caps: the mockup reserves uppercase for the labels over
/// the dashboard's big numbers. The existing label strings all end in a colon
/// because they used to sit to the left of their control, so strip it; a colon
/// on a line of its own is a leftover, in every one of the nine languages.
pub fn field_caption(ui: &mut Ui, text: &str) {
    let text = text.trim().trim_end_matches([':', '\u{ff1a}']).trim();
    ui.label(egui::RichText::new(text).size(11.5).color(TEXT_DIM));
}

/// One column of the two column field grid: caption on top, control under it,
/// both clamped to `width` so the two columns stay aligned whatever the control
/// wants to be. The content closure gets the width it has to work with.
pub fn field_col(ui: &mut Ui, width: f32, label: &str, content: impl FnOnce(&mut Ui, f32)) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(width);
            field_caption(ui, label);
            ui.add_space(5.0);
            content(ui, width);
        },
    );
}

/// A value the panel computes and the user cannot type into. It looks like a
/// field because it sits in a row of fields, but it is deliberately not one:
/// drawing an editable box around a derived number would be a lie.
pub fn readonly_field(ui: &mut Ui, width: f32, text: &str, tint: Color32) {
    Frame::none()
        .fill(BG_INPUT)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(8.0))
        .inner_margin(Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_width((width - 22.0).max(40.0));
            ui.label(egui::RichText::new(text).size(13.0).color(tint));
        });
}

/// A pick-one card: short badge, title, one line under it. Two of these side by
/// side are how the mockup asks the two questions that decide everything else.
pub struct OptionCard<'a> {
    /// Short badge text, e.g. "HAC". Empty draws no badge.
    pub badge: &'a str,
    pub title: &'a str,
    pub sub: &'a str,
    pub selected: bool,
    /// False greys the card out and stops it responding to clicks. Use `sub` to
    /// say why: an option a user cannot pick must state its reason on the card,
    /// not after they have picked it.
    pub enabled: bool,
}

pub fn option_card(ui: &mut Ui, width: f32, card: OptionCard<'_>) -> egui::Response {
    let title_color = if card.enabled { TEXT } else { TEXT_DIM };
    let sub_color = if card.enabled { TEXT_MUTED } else { GOLD_DIM };
    let badge_x = if card.badge.is_empty() { 16.0 } else { 60.0 };
    let text_w = (width - badge_x - 14.0).max(40.0);

    // Laid out before allocation so the card is exactly as tall as its text.
    // Only the fill reacts to hover, so nothing here depends on the response.
    let title = ui.fonts(|f| {
        f.layout(
            card.title.to_owned(),
            FontId::new(13.5, FontFamily::Proportional),
            title_color,
            text_w,
        )
    });
    let sub = ui.fonts(|f| {
        f.layout(
            card.sub.to_owned(),
            FontId::new(11.5, FontFamily::Proportional),
            sub_color,
            text_w,
        )
    });
    let text_h = title.rect.height() + 4.0 + sub.rect.height();
    let height = (text_h + 30.0).max(62.0);

    let sense = if card.enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    if ui.is_rect_visible(rect) {
        let rounding = Rounding::same(12.0);
        let hovered = card.enabled && response.hovered();
        let fill = if card.selected {
            NAV_ACTIVE_BG
        } else if hovered {
            BG_RAISED
        } else {
            BG_CARD
        };
        let stroke = if card.selected {
            Stroke::new(1.0, BORDER_ACCENT)
        } else {
            Stroke::new(1.0, BORDER_SOFT)
        };
        ui.painter().rect_filled(rect, rounding, fill);
        ui.painter().rect_stroke(rect, rounding, stroke);

        if !card.badge.is_empty() {
            let badge = Rect::from_center_size(
                egui::pos2(rect.left() + 34.0, rect.center().y),
                Vec2::splat(32.0),
            );
            let badge_color = if card.enabled { ACCENT } else { TEXT_DIM };
            ui.painter()
                .rect_filled(badge, Rounding::same(8.0), NAV_ACTIVE_BG);
            ui.painter().rect_stroke(
                badge,
                Rounding::same(8.0),
                Stroke::new(
                    1.0,
                    if card.enabled {
                        BORDER_ACCENT
                    } else {
                        BORDER_SOFT
                    },
                ),
            );
            ui.painter().text(
                badge.center(),
                egui::Align2::CENTER_CENTER,
                card.badge,
                FontId::new(
                    if card.badge.len() > 3 { 9.5 } else { 11.0 },
                    FontFamily::Proportional,
                ),
                badge_color,
            );
        }

        let top = rect.center().y - text_h * 0.5;
        let x = rect.left() + badge_x;
        ui.painter().galley(egui::pos2(x, top), title, title_color);
        ui.painter().galley(
            egui::pos2(x, top + text_h - sub.rect.height()),
            sub,
            sub_color,
        );
    }
    response
}

/// A numbered step: gold badge, title, a quiet one line explanation, then the
/// controls for that step. Used to turn the settings page into a short,
/// readable sequence for people who have never mined before.
pub fn step_card(ui: &mut Ui, num: u8, title: &str, hint: &str, content: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(20.0, 18.0))
        .show(ui, |ui| {
            // A `Frame` shrinks to its content, so a step whose only control is
            // one half-width field drew a half-width card: on Simple, "Where
            // should your coins go?" was 748px wide in a column where steps 1
            // and 2 were 1,429. Every step card spans the column.
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                let c = rect.center();
                ui.painter().circle_filled(c, 14.0, NAV_ACTIVE_BG);
                ui.painter()
                    .circle_stroke(c, 14.0, Stroke::new(1.0, BORDER_ACCENT));
                // Light weight, not bold: in the mockup the numeral is quiet and
                // the step title is what you read.
                ui.painter().text(
                    c,
                    egui::Align2::CENTER_CENTER,
                    num.to_string(),
                    FontId::new(14.0, FontFamily::Proportional),
                    ACCENT,
                );
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .strong()
                            .color(TEXT)
                            .size(typo::CARD_TITLE + 1.0),
                    );
                    if !hint.is_empty() {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(hint).color(TEXT_MUTED).size(typo::SUB));
                    }
                });
            });
            ui.add_space(14.0);
            content(ui);
        });
    ui.add_space(12.0);
}

/// Compact segmented control, e.g. "Simple | Advanced". Returns the index the
/// user picked, if any.
pub fn segmented(ui: &mut Ui, options: &[&str], selected: usize) -> Option<usize> {
    segmented_impl(ui, options, selected, Some(108.0))
}

/// The same control stretched across the card, which is how the mockup sets the
/// Eco / Profit / Max row: three equal segments, no leftover gutter.
pub fn segmented_wide(ui: &mut Ui, options: &[&str], selected: usize) -> Option<usize> {
    segmented_impl(ui, options, selected, None)
}

fn segmented_impl(
    ui: &mut Ui,
    options: &[&str],
    selected: usize,
    item_width: Option<f32>,
) -> Option<usize> {
    let mut picked = None;
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(11.0))
        .inner_margin(Margin::same(4.0))
        .show(ui, |ui| {
            let count = options.len().max(1) as f32;
            let width = item_width.unwrap_or_else(|| {
                ((ui.available_width() - 4.0 * (count - 1.0)) / count).max(56.0)
            });
            // Explicitly left to right, and exactly as wide as its segments.
            // `ui.horizontal` would inherit the parent layout, and this control
            // is normally placed inside a right-to-left row, which silently
            // reversed the segments: the header read "Advanced | Simple" with
            // Simple on the right. Plain `with_layout` fixes the order but
            // claims the whole remaining width, which turns the pill into a
            // wide empty box, so the size is stated here.
            let total = Vec2::new(width * count + 4.0 * (count - 1.0), 30.0);
            ui.allocate_ui_with_layout(
                total,
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, label) in options.iter().enumerate() {
                        let on = i == selected;
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
                        if ui.is_rect_visible(rect) {
                            let hover = resp.hovered();
                            let fill = if on {
                                NAV_ACTIVE_BG
                            } else if hover {
                                BG_RAISED
                            } else {
                                Color32::TRANSPARENT
                            };
                            let text = if on {
                                ACCENT
                            } else if hover {
                                TEXT
                            } else {
                                TEXT_MUTED
                            };
                            ui.painter().rect_filled(rect, Rounding::same(8.0), fill);
                            if on {
                                ui.painter().rect_stroke(
                                    rect,
                                    Rounding::same(8.0),
                                    Stroke::new(1.0, BORDER_ACCENT),
                                );
                            }
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                *label,
                                FontId::new(13.0, FontFamily::Proportional),
                                text,
                            );
                        }
                        if resp.clicked() {
                            picked = Some(i);
                        }
                    }
                },
            );
        });
    picked
}

/// The one obvious action on a page.
pub fn btn_primary_large(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [210.0, 46.0],
        egui::Button::new(
            egui::RichText::new(label)
                .color(BG_DEEP)
                .strong()
                .size(15.5),
        )
        .fill(GREEN)
        .stroke(Stroke::new(1.0, GREEN_DIM))
        .rounding(Rounding::same(12.0)),
    )
}

/// A quiet framed note, for the one thing a beginner must understand on a page.
pub fn note(ui: &mut Ui, accent: Color32, text: &str) {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 11.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let (bar, _) = ui.allocate_exact_size(Vec2::new(3.0, 16.0), Sense::hover());
                ui.painter().rect_filled(bar, Rounding::same(2.0), accent);
                ui.add_space(8.0);
                ui.label(egui::RichText::new(text).color(TEXT_MUTED).size(typo::SUB));
            });
        });
}

/// One icon per screen the panel actually has. There are five screens, so there
/// are five icons: the mockup's sidebar shows nine entries, and four of them
/// have no screen behind them in this build.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TabIcon {
    /// Live numbers: a small bar chart.
    Overview,
    /// Configuration: a sheet with lines on it.
    Setup,
    /// The fleet table: several workers.
    Fleet,
    /// The worker log: lines of text, ragged the way printed output is.
    Logs,
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MinerBadgeState {
    Mining,
    Stopped,
    Paused,
}

/// Thin line icons at a single stroke weight, which is what the mockups use.
/// Filled shapes read as heavier than the 13px label beside them and pull the
/// eye away from the selected item.
fn paint_tab_icon(painter: &egui::Painter, rect: Rect, icon: TabIcon, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.5;
    let stroke = Stroke::new(1.5, color);
    match icon {
        TabIcon::Overview => {
            // Three bars of rising height on a baseline.
            let base = c.y + s * 0.72;
            for (i, h) in [0.55_f32, 0.95, 0.75].iter().enumerate() {
                let x = c.x + (i as f32 - 1.0) * s * 0.62;
                painter.line_segment(
                    [
                        egui::pos2(x, base),
                        egui::pos2(x, base - s * 2.0 * h * 0.72),
                    ],
                    stroke,
                );
            }
        }
        TabIcon::Setup => {
            let sheet = Rect::from_center_size(c, Vec2::new(s * 1.35, s * 1.75));
            painter.rect_stroke(sheet, Rounding::same(2.5), stroke);
            for i in 0..2 {
                let y = sheet.top() + sheet.height() * (0.34 + i as f32 * 0.3);
                painter.line_segment(
                    [
                        egui::pos2(sheet.left() + s * 0.32, y),
                        egui::pos2(sheet.right() - s * 0.32, y),
                    ],
                    stroke,
                );
            }
        }
        TabIcon::Fleet => {
            // Three machines stacked: the fleet table is a list of workers, and
            // a list of workers is what this has to look like at 16 pixels.
            for i in 0..3 {
                let y = c.y + (i as f32 - 1.0) * s * 0.68;
                painter.rect_stroke(
                    Rect::from_center_size(egui::pos2(c.x, y), Vec2::new(s * 1.7, s * 0.44)),
                    Rounding::same(1.5),
                    stroke,
                );
            }
        }
        TabIcon::Logs => {
            // Four lines of unequal length. The Setup sheet is a framed page;
            // this is the text without the frame, which is what a log is.
            for (i, w) in [1.0_f32, 0.66, 0.88, 0.5].iter().enumerate() {
                let y = c.y + (i as f32 - 1.5) * s * 0.52;
                let left = c.x - s * 0.85;
                painter.line_segment(
                    [egui::pos2(left, y), egui::pos2(left + s * 1.7 * w, y)],
                    stroke,
                );
            }
        }
        TabIcon::Help => {
            painter.circle_stroke(c, s * 0.82, stroke);
            painter.text(
                c + Vec2::new(0.0, s * 0.04),
                egui::Align2::CENTER_CENTER,
                "?",
                FontId::new(s * 1.1, FontFamily::Proportional),
                color,
            );
        }
    }
}

/// Sidebar brand block: the logo, the product name, and one line of small
/// uppercase attribution under it.
pub fn sidebar_brand(ui: &mut Ui, logo: Option<&egui::TextureHandle>, name: &str, tagline: &str) {
    ui.horizontal(|ui| {
        match logo {
            Some(tex) => show_logo(ui, tex, 34.0),
            None => logo_fallback(ui),
        }
        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(name).size(16.0).strong().color(TEXT));
            ui.add_space(1.0);
            ui.label(
                egui::RichText::new(tagline.to_uppercase())
                    .size(9.5)
                    .color(TEXT_DIM),
            );
        });
    });
}

/// The small uppercase heading that groups sidebar items ("MINING", "CONTROL").
pub fn nav_section_label(ui: &mut Ui, text: &str) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(text.to_uppercase())
                .size(typo::LABEL - 0.5)
                .color(TEXT_DIM),
        );
    });
    ui.add_space(6.0);
}

/// One sidebar entry. Returns true on the frame it was clicked.
///
/// Selected state is a dark amber fill, a muted amber border and a 3px amber bar
/// down the left edge, exactly as in mockup 1. No glow: egui cannot blur, and a
/// stack of translucent rounded rects pretending to be one is worse than the
/// honest flat shape.
pub fn nav_item(
    ui: &mut Ui,
    selected: bool,
    icon: TabIcon,
    label: &str,
    badge: Option<&str>,
) -> bool {
    // 36, measured off mockup 1: its active pill runs y247..282 and the pitch
    // from one label baseline to the next is 40, which the caller's 4px item
    // spacing completes. At 40 the pitch was 44 and the whole column ran a
    // nav item too long by the time it reached Help.
    let height = 36.0;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = response.hovered();
        let rounding = Rounding::same(10.0);
        if selected {
            ui.painter().rect_filled(rect, rounding, NAV_ACTIVE_BG);
            ui.painter()
                .rect_stroke(rect, rounding, Stroke::new(1.0, BORDER_ACCENT));
            // Full pill height, 1px inset. The old bar was `height - 18`, which
            // left 9px of dead pill above and below a 22px stub.
            let bar = Rect::from_min_size(
                egui::pos2(rect.left() + 1.0, rect.top() + 1.0),
                Vec2::new(3.0, height - 2.0),
            );
            ui.painter().rect_filled(bar, Rounding::same(2.0), ACCENT);
        } else if hover {
            ui.painter().rect_filled(rect, rounding, BG_RAISED);
        }

        let text_color = if selected {
            TEXT
        } else if hover {
            TEXT
        } else {
            TEXT_MUTED
        };
        let icon_color = if selected { ACCENT } else { text_color };

        let icon_rect = Rect::from_center_size(
            egui::pos2(rect.left() + 24.0, rect.center().y),
            Vec2::splat(16.0),
        );
        paint_tab_icon(ui.painter(), icon_rect, icon, icon_color);

        ui.painter().text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::new(13.5, FontFamily::Proportional),
            text_color,
        );

        if let Some(badge) = badge {
            let font = FontId::new(9.5, FontFamily::Proportional);
            let galley = ui.painter().layout_no_wrap(
                badge.to_uppercase(),
                font.clone(),
                if selected { ACCENT } else { TEXT_DIM },
            );
            let pill = Rect::from_center_size(
                egui::pos2(
                    rect.right() - 12.0 - galley.rect.width() * 0.5,
                    rect.center().y,
                ),
                Vec2::new(galley.rect.width() + 14.0, 18.0),
            );
            ui.painter().rect_stroke(
                pill,
                Rounding::same(9.0),
                Stroke::new(1.0, if selected { BORDER_ACCENT } else { BORDER_SOFT }),
            );
            ui.painter()
                .galley(pill.center() - galley.rect.size() * 0.5, galley, TEXT_DIM);
        }
    }
    if response.clicked() {
        ui.ctx().request_repaint();
    }
    response.clicked()
}

/// One line of the sidebar status block: an optional state dot, a muted label,
/// and the value pushed to the right.
pub struct SidebarStatus<'a> {
    /// A filled dot before the label, for the row whose state is the point.
    pub dot: Option<Color32>,
    pub label: &'a str,
    pub value: &'a str,
    pub tint: Color32,
}

/// The live status block that sits at the bottom of the sidebar in mockup 1.
///
/// Rows are passed in reading order and drawn in reverse, because the caller
/// pins this card to the bottom with a bottom-up layout and a bottom-up ui
/// deals its children out from the bottom. Do not "fix" that by nesting a
/// top-down layout inside the frame: the nested ui takes the whole free height
/// of the sidebar as its own, and the card becomes a tall empty box with three
/// lines at the top of it.
pub fn sidebar_status_card(ui: &mut Ui, rows: &[SidebarStatus<'_>]) {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 7.0;
            for row in rows.iter().rev() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(dot) = row.dot {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(7.0, 7.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 3.5, dot);
                    }
                    ui.label(egui::RichText::new(row.label).size(11.5).color(TEXT_MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(row.value)
                                .size(11.5)
                                .strong()
                                .color(row.tint),
                        );
                    });
                });
            }
        });
}

pub fn btn_primary(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(BG_DEEP).strong())
            .fill(GREEN)
            .stroke(Stroke::new(1.0, GREEN_DIM))
            .rounding(Rounding::same(10.0)),
    )
}

pub fn btn_secondary(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(TEXT))
            .fill(BG_CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .rounding(Rounding::same(10.0)),
    )
}

pub fn power_cost_slider(ui: &mut Ui, value: &mut f32, currency: crate::currency::Currency) {
    let (min, max) = currency.power_cost_range();
    let step = currency.slider_step();
    let decimals = if currency == crate::currency::Currency::Jpy {
        0
    } else {
        2
    };
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(format!("{value:.decimals$} {}/kWh", currency.symbol()))
                .color(ACCENT)
                .size(13.0),
        );
        ui.add_space(4.0);
        // The rail takes whatever the column gives it. It used to demand 280px,
        // which pushed the right hand column off the card on a narrow window.
        ui.spacing_mut().slider_width = ui.available_width().clamp(120.0, 280.0);
        ui.add(
            egui::Slider::new(value, min..=max)
                .step_by(step as f64)
                .trailing_fill(true)
                .show_value(false),
        );
    });
}

/// Destructive action. Dark fill, red text, red hairline: quiet until you are
/// looking for it, which is right, because a solid red slab next to the primary
/// button competes with it for the eye.
pub fn btn_danger(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(RED).strong())
            .fill(RED_DIM)
            .stroke(Stroke::new(1.0, Color32::from_rgb(90, 42, 40)))
            .rounding(Rounding::same(10.0)),
    )
}

pub fn miner_badge_state(mining: bool, paused: bool) -> MinerBadgeState {
    if mining && paused {
        MinerBadgeState::Paused
    } else if mining {
        MinerBadgeState::Mining
    } else {
        MinerBadgeState::Stopped
    }
}

pub fn status_badge(ui: &mut Ui, state: MinerBadgeState, label: &str) {
    let time = ui.input(|i| i.time);
    let (fill, stroke, dot, glow, text) = match state {
        MinerBadgeState::Mining => {
            // The dot breathes. That is the only motion in the theme, and it is
            // the one fact worth animating: the worker is alive right now.
            let pulse = ((time * 3.2).sin() * 0.5 + 0.5) as f32;
            (
                NAV_ACTIVE_BG,
                Stroke::new(1.0, BORDER_ACCENT),
                ACCENT,
                Some((
                    7.0 + pulse * 3.0,
                    ACCENT.linear_multiply(0.18 + pulse * 0.2),
                )),
                ACCENT,
            )
        }
        MinerBadgeState::Paused => (BG_CARD, Stroke::new(1.0, GOLD_DIM), GOLD_DIM, None, GOLD),
        MinerBadgeState::Stopped => (
            BG_CARD,
            Stroke::new(1.0, BORDER_SOFT),
            TEXT_DIM,
            None,
            TEXT_MUTED,
        ),
    };

    Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(22.0))
        .inner_margin(Margin::symmetric(18.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), egui::Sense::hover());
                let center = rect.center();
                if let Some((radius, color)) = glow {
                    ui.painter().circle_filled(center, radius, color);
                }
                ui.painter().circle_filled(center, 4.5, dot);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(label).strong().color(text).size(14.0));
            });
        });
    if state == MinerBadgeState::Mining {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(80));
    }
}

pub fn footer_status_chip(ui: &mut Ui, state: MinerBadgeState, label: &str) {
    let (fill, stroke, dot, text) = match state {
        MinerBadgeState::Mining => (
            NAV_ACTIVE_BG,
            Stroke::new(1.0, BORDER_ACCENT),
            ACCENT,
            ACCENT,
        ),
        MinerBadgeState::Paused => (BG_CARD, Stroke::new(1.0, GOLD_DIM), GOLD_DIM, GOLD),
        MinerBadgeState::Stopped => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, BORDER_SOFT),
            TEXT_DIM,
            TEXT_MUTED,
        ),
    };

    Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(16.0))
        .inner_margin(Margin::symmetric(10.0, 5.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, dot);
                ui.label(egui::RichText::new(label).color(text).size(13.0));
            });
        });
}

pub fn help_step(ui: &mut Ui, num: u8, text: &str) {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                Frame::none()
                    .fill(NAV_ACTIVE_BG)
                    .stroke(Stroke::new(1.0, BORDER_ACCENT))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(Margin::same(7.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(num.to_string())
                                .color(ACCENT)
                                .size(13.0),
                        );
                    });
                ui.add_space(8.0);
                ui.label(egui::RichText::new(text).color(TEXT).size(13.5));
            });
        });
    ui.add_space(6.0);
}

pub fn show_logo(ui: &mut Ui, texture: &egui::TextureHandle, height: f32) {
    let size = crate::assets::logo_size_for_height(texture, height);
    ui.add(egui::Image::new(texture).fit_to_exact_size(size));
}

pub fn logo_fallback(ui: &mut Ui) {
    Frame::none()
        .fill(NAV_ACTIVE_BG)
        .stroke(Stroke::new(1.0, BORDER_ACCENT))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(9.0, 7.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("HAC").strong().color(ACCENT).size(15.0));
        });
}
