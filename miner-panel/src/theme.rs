use eframe::egui::{
    self, Color32, FontFamily, FontId, Frame, Margin, Rect, Rounding, Sense, Stroke, TextStyle, Ui,
    Vec2,
};

pub mod colors {
    use super::Color32;

    pub const BG_DEEP: Color32 = Color32::from_rgb(0, 0, 0);
    pub const BG_PANEL: Color32 = Color32::from_rgb(0, 0, 0);
    pub const BG_HEADER: Color32 = Color32::from_rgb(5, 5, 5);
    pub const BG_CARD: Color32 = Color32::from_rgb(10, 10, 10);
    pub const BG_INPUT: Color32 = Color32::from_rgb(3, 3, 3);
    pub const BORDER: Color32 = Color32::from_rgb(115, 52, 0);
    pub const BORDER_SOFT: Color32 = Color32::from_rgb(51, 25, 4);
    pub const TEXT: Color32 = Color32::from_rgb(247, 247, 247);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(178, 164, 151);
    pub const GOLD: Color32 = Color32::from_rgb(255, 122, 0);
    pub const GOLD_DIM: Color32 = Color32::from_rgb(143, 63, 0);
    pub const ACCENT: Color32 = Color32::from_rgb(255, 122, 0);
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(159, 65, 0);
    pub const SLIDER_TRACK: Color32 = Color32::from_rgb(20, 11, 3);
    pub const GREEN: Color32 = Color32::from_rgb(255, 140, 25);
    pub const GREEN_DIM: Color32 = Color32::from_rgb(137, 56, 0);
    pub const RED: Color32 = Color32::from_rgb(248, 108, 108);
    pub const RED_DIM: Color32 = Color32::from_rgb(140, 48, 48);
    pub const BLUE: Color32 = Color32::from_rgb(255, 157, 46);
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
    v.selection.bg_fill = Color32::from_rgb(88, 38, 0);
    v.selection.stroke = Stroke::new(1.0, BLUE);

    let round = Rounding::same(8.0);
    v.widgets.noninteractive.rounding = round;
    v.widgets.inactive.rounding = round;
    v.widgets.hovered.rounding = round;
    v.widgets.active.rounding = round;
    v.widgets.open.rounding = round;

    v.widgets.inactive.bg_fill = SLIDER_TRACK; // visible slider rail on cards
    v.widgets.noninteractive.bg_fill = BG_CARD;
    v.widgets.hovered.bg_fill = Color32::from_rgb(27, 14, 5);
    v.widgets.active.bg_fill = Color32::from_rgb(40, 19, 6);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SOFT);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.active.bg_stroke = Stroke::new(1.5, ACCENT_DIM);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.5, ACCENT);

    v.slider_trailing_fill = true;
    v.selection.bg_fill = Color32::from_rgb(114, 50, 0);

    v.window_rounding = Rounding::same(10.0);
    v.window_stroke = Stroke::new(1.0, BORDER);

    style.spacing.item_spacing = Vec2::new(12.0, 10.0);
    style.spacing.button_padding = Vec2::new(16.0, 9.0);
    style.spacing.slider_width = 280.0;
    style.spacing.slider_rail_height = 10.0;
    style.spacing.indent = 18.0;
    style.spacing.window_margin = Margin::same(14.0);

    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(22.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(14.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(12.0, FontFamily::Proportional),
    );

    ctx.set_style(style);
}

pub fn header_frame() -> Frame {
    Frame::none()
        .fill(BG_HEADER)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .inner_margin(Margin::symmetric(22.0, 14.0))
}

pub fn footer_frame() -> Frame {
    Frame::none()
        .fill(BG_HEADER)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .inner_margin(Margin::symmetric(22.0, 10.0))
}

pub fn content_frame() -> Frame {
    Frame::none()
        .fill(BG_DEEP)
        .inner_margin(Margin::symmetric(22.0, 16.0))
}

pub fn section_card() -> Frame {
    Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::symmetric(18.0, 14.0))
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
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::symmetric(12.0, 14.0))
        .show(ui, |ui| {
            ui.set_min_width(160.0);
            ui.set_min_height(72.0);
            ui.horizontal(|ui| {
                let (stripe, _) =
                    ui.allocate_exact_size(Vec2::new(4.0, 44.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(stripe, Rounding::same(2.0), accent);
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(title).size(12.0).color(TEXT_MUTED));
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(value).size(20.0).strong().color(TEXT));
                });
            });
        });
}

pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(TEXT).size(13.5));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TabIcon {
    Settings,
    Dashboard,
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MinerBadgeState {
    Mining,
    Stopped,
    Paused,
}

fn paint_tab_icon(painter: &egui::Painter, rect: Rect, icon: TabIcon, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.5;
    match icon {
        TabIcon::Settings => {
            painter.circle_stroke(c, s * 0.55, Stroke::new(1.6, color));
            for i in 0..6 {
                let a = std::f32::consts::TAU * i as f32 / 6.0;
                let inner = c + Vec2::angled(a) * s * 0.28;
                let outer = c + Vec2::angled(a) * s * 0.82;
                painter.line_segment([inner, outer], Stroke::new(1.8, color));
            }
            painter.circle_filled(c, s * 0.16, color);
        }
        TabIcon::Dashboard => {
            let gap = s * 0.14;
            let half = (s - gap) * 0.5;
            let tl = c + Vec2::new(-half - gap * 0.5, -half - gap * 0.5);
            for row in 0..2 {
                for col in 0..2 {
                    let min = tl + Vec2::new(col as f32 * (half + gap), row as f32 * (half + gap));
                    let tile = Rect::from_min_size(min, Vec2::splat(half));
                    painter.rect_filled(tile, Rounding::same(2.0), color);
                }
            }
        }
        TabIcon::Help => {
            painter.circle_stroke(c, s * 0.72, Stroke::new(1.8, color));
            painter.text(
                c + Vec2::new(0.0, s * 0.06),
                egui::Align2::CENTER_CENTER,
                "?",
                FontId::new(s * 1.15, FontFamily::Proportional),
                color,
            );
        }
    }
}

pub fn tab_bar(ui: &mut Ui, content: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(Color32::from_rgba_premultiplied(3, 3, 3, 240))
        .stroke(Stroke::new(1.0, BORDER_SOFT))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(8.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                content(ui);
            });
        });
}

pub fn tab_pill(ui: &mut Ui, selected: bool, icon: TabIcon, label: &str) -> bool {
    let desired = Vec2::new(148.0, 42.0);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = response.hovered();
        let fill = if selected {
            Color32::from_rgba_premultiplied(255, 122, 0, 58)
        } else if hover {
            Color32::from_rgba_premultiplied(104, 48, 4, 90)
        } else {
            Color32::from_rgba_premultiplied(8, 8, 8, 160)
        };
        let stroke = if selected {
            Stroke::new(1.5, ACCENT)
        } else if hover {
            Stroke::new(1.0, BORDER)
        } else {
            Stroke::new(1.0, BORDER_SOFT)
        };
        let text_color = if selected {
            TEXT
        } else if hover {
            ACCENT
        } else {
            TEXT_MUTED
        };
        let icon_color = if selected { ACCENT } else { text_color };

        ui.painter().rect_filled(rect, Rounding::same(11.0), fill);
        ui.painter().rect_stroke(rect, Rounding::same(11.0), stroke);
        if selected {
            let bar = Rect::from_min_size(
                egui::pos2(rect.left() + 10.0, rect.bottom() - 4.0),
                Vec2::new(rect.width() - 20.0, 3.0),
            );
            ui.painter().rect_filled(bar, Rounding::same(2.0), GREEN);
        }

        let icon_rect = Rect::from_center_size(
            egui::pos2(rect.left() + 22.0, rect.center().y),
            Vec2::splat(18.0),
        );
        paint_tab_icon(ui.painter(), icon_rect, icon, icon_color);

        ui.painter().text(
            egui::pos2(rect.left() + 40.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::new(14.0, FontFamily::Proportional),
            text_color,
        );
    }
    if response.clicked() {
        ui.ctx().request_repaint();
    }
    response.clicked()
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
        ui.set_min_width(280.0);
        ui.add(
            egui::Slider::new(value, min..=max)
                .step_by(step as f64)
                .trailing_fill(true)
                .show_value(false),
        );
    });
}

pub fn btn_danger(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
            .fill(RED_DIM)
            .stroke(Stroke::new(1.0, RED))
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
            let pulse = ((time * 3.2).sin() * 0.5 + 0.5) as f32;
            (
                Color32::from_rgba_premultiplied(255, 112, 0, (28.0 + pulse * 38.0) as u8),
                Stroke::new(1.8, GREEN),
                GREEN,
                Some((
                    8.0 + pulse * 5.0,
                    GREEN.linear_multiply(0.25 + pulse * 0.35),
                )),
                GREEN,
            )
        }
        MinerBadgeState::Paused => (
            Color32::from_rgba_premultiplied(150, 110, 40, 48),
            Stroke::new(1.5, GOLD),
            GOLD,
            None,
            GOLD,
        ),
        MinerBadgeState::Stopped => (
            Color32::from_rgba_premultiplied(70, 80, 96, 50),
            Stroke::new(1.0, BORDER),
            Color32::from_rgb(150, 160, 175),
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
                ui.painter().circle_filled(center, 5.5, dot);
                if state == MinerBadgeState::Mining {
                    ui.painter().circle_stroke(
                        center,
                        7.5,
                        Stroke::new(1.5, GREEN.linear_multiply(0.65)),
                    );
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(label).strong().color(text).size(15.5));
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
            Color32::from_rgba_premultiplied(255, 112, 0, 32),
            Stroke::new(1.0, GREEN_DIM),
            GREEN,
            GREEN,
        ),
        MinerBadgeState::Paused => (
            Color32::from_rgba_premultiplied(150, 110, 40, 36),
            Stroke::new(1.0, GOLD_DIM),
            GOLD,
            GOLD,
        ),
        MinerBadgeState::Stopped => (
            Color32::TRANSPARENT,
            Stroke::new(1.0, BORDER_SOFT),
            TEXT_MUTED,
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
                    .fill(Color32::from_rgba_premultiplied(175, 148, 90, 28))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(num.to_string())
                                .strong()
                                .color(GOLD)
                                .size(14.0),
                        );
                    });
                ui.add_space(6.0);
                ui.label(egui::RichText::new(text).color(TEXT).size(14.0));
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
        .fill(Color32::from_rgba_premultiplied(175, 148, 90, 30))
        .stroke(Stroke::new(1.0, GOLD_DIM))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("HAC").strong().color(GOLD).size(17.0));
        });
}
