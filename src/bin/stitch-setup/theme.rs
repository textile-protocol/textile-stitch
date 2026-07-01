// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Textile's visual language for the desktop app: the web app's design tokens
//! (warm off-white, magenta accent, muted neutrals) ported onto egui, plus the
//! Lato font and a handful of reusable widgets (status pill, primary button,
//! card, toggle) so the panel and wizard read like one polished product rather
//! than stock egui.

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, StrokeKind, TextStyle, Theme,
};

/// Lato — the same family the web app ships. Regular for body, Bold for headings
/// and buttons. Bundled emoji/monospace fonts stay as fallbacks.
const LATO_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Lato-Regular.ttf");
const LATO_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Lato-Bold.ttf");

/// Name of the bold font family, used for headings, buttons and emphasis labels.
const BOLD: &str = "Lato-Bold";

/// The resolved colors for one appearance (light or dark). Field names mirror the
/// web app's `--tx-*` custom properties so the two stay recognizably in sync.
pub struct Palette {
    pub bg: Color32,      // --tx-bg-primary (app background)
    pub surface: Color32, // --tx-bg-secondary (cards, inputs)
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub text: Color32,        // --tx-text-primary
    pub text_muted: Color32,  // --tx-text-secondary
    pub text_faint: Color32,  // --tx-text-tertiary
    pub border: Color32,      // --tx-border-secondary
    pub border_soft: Color32, // --tx-border-tertiary
    pub accent: Color32,      // --tx-accent
    pub accent_tint: Color32, // --tx-accent-tint
    pub on_accent: Color32,   // text on an accent fill
    pub success: Color32,     // --tx-text-success
    pub success_bg: Color32,  // --tx-bg-success
    pub warning: Color32,     // --tx-text-warning
    pub warning_bg: Color32,  // --tx-bg-warning
    pub danger: Color32,
    pub danger_bg: Color32,
}

impl Palette {
    /// Textile light theme (web `:root`).
    fn light() -> Self {
        Self {
            bg: hex(0xFB, 0xFA, 0xF7),
            surface: Color32::WHITE,
            surface_hover: hex(0xF1, 0xEE, 0xE8),
            surface_active: hex(0xE9, 0xE5, 0xDD),
            text: hex(0x15, 0x18, 0x1C),
            text_muted: Color32::from_rgba_unmultiplied(21, 24, 28, 153), // 60%
            text_faint: Color32::from_rgba_unmultiplied(21, 24, 28, 115), // 45%
            border: Color32::from_rgba_unmultiplied(21, 24, 28, 46),      // 18%
            border_soft: Color32::from_rgba_unmultiplied(21, 24, 28, 26), // 10%
            accent: hex(0x9C, 0x2E, 0xB0),
            accent_tint: Color32::from_rgba_unmultiplied(236, 104, 248, 20),
            on_accent: Color32::WHITE,
            success: hex(0x3B, 0x6D, 0x11),
            success_bg: hex(0xEC, 0xF4, 0xDF),
            warning: hex(0xBA, 0x75, 0x17),
            warning_bg: hex(0xFB, 0xF0, 0xDC),
            danger: hex(0xB0, 0x3A, 0x2E),
            danger_bg: hex(0xF7, 0xE7, 0xE4),
        }
    }

    /// Textile dark theme (web `[data-theme="dark"]`).
    fn dark() -> Self {
        Self {
            bg: hex(0x33, 0x36, 0x3E),
            surface: hex(0x2A, 0x2D, 0x34),
            surface_hover: hex(0x30, 0x33, 0x3B),
            surface_active: hex(0x22, 0x24, 0x2A),
            text: hex(0xF5, 0xF4, 0xED),
            text_muted: Color32::from_rgba_unmultiplied(255, 255, 255, 166), // 65%
            text_faint: Color32::from_rgba_unmultiplied(255, 255, 255, 115), // 45%
            border: Color32::from_rgba_unmultiplied(255, 255, 255, 56),      // 22%
            border_soft: Color32::from_rgba_unmultiplied(255, 255, 255, 31), // 12%
            accent: hex(0xEC, 0x68, 0xF8),
            accent_tint: Color32::from_rgba_unmultiplied(236, 104, 248, 41),
            on_accent: hex(0x15, 0x18, 0x1C),
            success: hex(0x97, 0xC4, 0x59),
            success_bg: hex(0x17, 0x34, 0x04),
            warning: hex(0xEF, 0x9F, 0x27),
            warning_bg: hex(0x41, 0x24, 0x02),
            danger: hex(0xEF, 0x8A, 0x7A),
            danger_bg: hex(0x3A, 0x1B, 0x16),
        }
    }

    /// The palette matching the current (system-driven) appearance.
    pub fn current(ctx: &egui::Context) -> Self {
        match ctx.theme() {
            Theme::Dark => Self::dark(),
            Theme::Light => Self::light(),
        }
    }
}

const fn hex(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Install Lato once at startup. Cheap to call, but only needs calling a single
/// time (font atlases are rebuilt on `set_fonts`).
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Lato".to_owned(),
        Arc::new(egui::FontData::from_static(LATO_REGULAR)),
    );
    fonts.font_data.insert(
        BOLD.to_owned(),
        Arc::new(egui::FontData::from_static(LATO_BOLD)),
    );

    // Lato leads the proportional stack; the bundled emoji font stays as a
    // fallback so glyphs like ● still render.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Lato".to_owned());
    // A dedicated bold family for headings/buttons (egui maps weight by family,
    // not by a style flag).
    fonts.families.insert(
        FontFamily::Name(BOLD.into()),
        vec![BOLD.to_owned(), "Lato".to_owned()],
    );

    ctx.set_fonts(fonts);
}

/// Apply the Textile style for the current appearance. Called every frame so a
/// system light/dark switch is picked up live; it's just struct assignment, so
/// the cost is negligible.
pub fn apply(ctx: &egui::Context) {
    let theme = ctx.theme();
    let p = Palette::current(ctx);
    let mut style = (*ctx.style_of(theme)).clone();

    let bold = FontFamily::Name(BOLD.into());
    style.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, bold.clone())),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, bold.clone())),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into();

    // Roomier than egui's defaults — the cramped spacing is a big part of what
    // reads as "unfinished".
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 8.0);
    s.button_padding = egui::vec2(12.0, 7.0);
    s.interact_size.y = 30.0;
    s.icon_width = 18.0;
    s.icon_width_inner = 10.0;
    s.combo_width = 220.0;
    s.menu_margin = Margin::same(6);

    let v = &mut style.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border_soft);
    v.window_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = p.surface; // text-edit / scroll background
    v.faint_bg_color = p.surface_hover;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent_tint;
    v.selection.stroke = Stroke::new(1.0, p.accent);

    let round = CornerRadius::same(8);
    let w = &mut v.widgets;
    // Labels, separators, frames drawn at rest.
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border_soft);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = round;

    // Buttons / combos at rest = a clean "ghost" surface.
    w.inactive.bg_fill = p.surface;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = round;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.surface_hover;
    w.hovered.weak_bg_fill = p.surface_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = round;
    w.hovered.expansion = 1.0;

    w.active.bg_fill = p.surface_active;
    w.active.weak_bg_fill = p.surface_active;
    w.active.bg_stroke = Stroke::new(1.0, p.border);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = round;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.surface_hover;
    w.open.weak_bg_fill = p.surface_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = round;

    ctx.set_style_of(theme, style);
}

/// A card frame: the surface color, a soft border, rounded corners and inner
/// padding. Used for meta rows and the log pane.
pub fn card(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border_soft))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(13, 11))
}

/// A filled accent button for the single most important action on screen.
pub fn primary_button(ui: &mut egui::Ui, p: &Palette, label: &str) -> egui::Response {
    let text = egui::RichText::new(label).color(p.on_accent);
    ui.add(
        egui::Button::new(text)
            .fill(p.accent)
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

/// A ghost button tinted with a semantic color (e.g. danger for Stop). Keeps the
/// surface look but recolors the label and border.
pub fn tinted_button(ui: &mut egui::Ui, color: Color32, label: &str) -> egui::Response {
    let text = egui::RichText::new(label).color(color);
    ui.add(
        egui::Button::new(text)
            .stroke(Stroke::new(1.0, color.linear_multiply(0.5)))
            .corner_radius(CornerRadius::same(8))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

/// A rounded status badge: a tinted background, a solid dot and a bold label.
pub fn status_pill(ui: &mut egui::Ui, bg: Color32, fg: Color32, label: &str) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(11, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, fg);
                ui.label(
                    egui::RichText::new(label)
                        .color(fg)
                        .size(12.0)
                        .family(FontFamily::Name(BOLD.into())),
                );
            });
        });
}

/// The Textile mark on a light rounded tile (mirrors the dock icon), sized for a
/// header. Falls back to nothing if the icon didn't load.
pub fn header_tile(ui: &mut egui::Ui, p: &Palette, icon: &Option<egui::TextureHandle>) {
    let size = 36.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(9),
        p.surface,
        Stroke::new(1.0, p.border_soft),
        StrokeKind::Inside,
    );
    if let Some(tex) = icon {
        let img = rect.shrink(6.0);
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        ui.painter().image(tex.id(), img, uv, Color32::WHITE);
    }
}

/// A small uppercase field label (used above values in cards and forms).
pub fn field_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    // egui 0.35 RichText has no letter-spacing; spacing the glyphs by hand keeps
    // the small-caps label feel without a custom galley.
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    ui.label(
        egui::RichText::new(spaced.trim_end())
            .color(p.text_faint)
            .size(10.0),
    );
}

/// An iOS-style toggle. Honors the enclosing `ui`'s enabled state (dimmed and
/// non-interactive while the bot runs).
pub fn toggle(ui: &mut egui::Ui, p: &Palette, on: &mut bool) -> egui::Response {
    let size = egui::vec2(34.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let enabled = ui.is_enabled();
    let how = ui.ctx().animate_bool(resp.id, *on);
    let radius = rect.height() / 2.0;
    let track = if *on { p.accent } else { p.border };
    let track = if enabled {
        track
    } else {
        track.linear_multiply(0.5)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius as u8), track);
    let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how);
    ui.painter()
        .circle_filled(egui::pos2(cx, rect.center().y), radius - 2.5, p.surface);
    resp
}
