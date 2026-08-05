// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Tray-menu icons with native AppKit artwork where it has a clear semantic
//! match and crisp custom fallbacks everywhere else.
//!
//! muda displays macOS menu images in an 18 pt slot but Windows and GTK consume
//! 16 px bitmaps. Custom icons are rendered directly at those backend sizes so
//! the OS never has to resample a mismatched 32 px source.
//!
//! Design rules for the set (Lucide conventions at 32×32):
//! - Shared stroke weight ([`STROKE`]) and ~6 px padding (optical ~20×20).
//! - Outline strokes throughout, including pause, play, moon, and refresh.
//! - No mixed per-glyph stroke thicknesses.

#[cfg(target_os = "macos")]
use tray_icon::menu::NativeIcon;
use tray_icon::menu::{Icon, IconMenuItem};

#[derive(Clone, Copy)]
pub enum StatusKind {
    Running,
    Stopped,
}

#[derive(Clone, Copy)]
pub enum ActionKind {
    Open,
    Pause,
    Resume,
    Update,
    Show,
    Quit,
}

/// Shared outline stroke in the 32-unit logical coordinate system.
const STROKE: f32 = 2.0;

const LOGICAL_SIZE: f32 = 32.0;

#[cfg(target_os = "macos")]
const RASTER_SIZE: u32 = 36;
#[cfg(not(target_os = "macos"))]
const RASTER_SIZE: u32 = 16;

pub struct MenuIcons {
    /// Matches [`prefer_light_glyphs`] at the time these bitmaps were drawn.
    light_glyphs: bool,
    dot_running: Icon,
    dot_stopped: Icon,
    open: Icon,
    pause: Icon,
    resume: Icon,
    update: Icon,
    show: Icon,
    quit: Icon,
    keep_awake: Icon,
}

impl MenuIcons {
    pub fn new() -> Self {
        Self::with_appearance(prefer_light_glyphs())
    }

    fn with_appearance(light_glyphs: bool) -> Self {
        let ink = ink_for(light_glyphs);
        Self {
            light_glyphs,
            // Outline rings; running adds an inner ring (radio-on).
            dot_running: draw_status_running(ink).expect("dot_running"),
            dot_stopped: draw_status_stopped(ink).expect("dot_stopped"),
            open: draw_open(ink).expect("open"),
            pause: draw_pause(ink).expect("pause"),
            resume: draw_resume(ink).expect("resume"),
            update: draw_update(ink).expect("update"),
            show: draw_show(ink).expect("show"),
            quit: draw_quit(ink).expect("quit"),
            keep_awake: draw_keep_awake(ink).expect("keep_awake"),
        }
    }

    /// Rebuild bitmaps when the OS light/dark preference changes.
    ///
    /// muda doesn't expose a template-image flag for custom RGBA menu icons, so
    /// ink is baked into pixels. Call this from the periodic status poll (or an
    /// appearance notification) and reapply icons when it returns `true`.
    pub fn refresh_for_appearance(&mut self) -> bool {
        let light = prefer_light_glyphs();
        if light == self.light_glyphs {
            return false;
        }
        *self = Self::with_appearance(light);
        true
    }
}

pub fn status_item(text: &str, kind: StatusKind, icons: &MenuIcons) -> IconMenuItem {
    IconMenuItem::new(text, false, Some(status_bitmap(icons, kind)), None)
}

pub fn action_item(text: &str, kind: ActionKind, icons: &MenuIcons) -> IconMenuItem {
    #[cfg(target_os = "macos")]
    if let Some(native) = native_action(kind) {
        return IconMenuItem::with_native_icon(text, true, Some(native), None);
    }
    IconMenuItem::new(text, true, Some(action_bitmap(icons, kind)), None)
}

pub fn apply_status(item: &IconMenuItem, kind: StatusKind, icons: &MenuIcons) {
    item.set_icon(Some(status_bitmap(icons, kind)));
}

pub fn apply_action(item: &IconMenuItem, kind: ActionKind, icons: &MenuIcons) {
    #[cfg(target_os = "macos")]
    if let Some(native) = native_action(kind) {
        item.set_native_icon(Some(native));
        return;
    }
    item.set_icon(Some(action_bitmap(icons, kind)));
}

#[cfg(target_os = "macos")]
fn native_action(kind: ActionKind) -> Option<NativeIcon> {
    match kind {
        ActionKind::Open => Some(NativeIcon::ColumnView),
        ActionKind::Resume => Some(NativeIcon::RightFacingTriangle),
        ActionKind::Update => Some(NativeIcon::Refresh),
        ActionKind::Pause | ActionKind::Show | ActionKind::Quit => None,
    }
}

/// Keep-awake row. muda's [`CheckMenuItem`] cannot carry an icon, so this is an
/// [`IconMenuItem`] with a crescent-moon glyph; on-state uses a leading
/// checkmark in the title (AppKit's state column isn't available on icon items).
pub fn keep_awake_item(enabled: bool, icons: &MenuIcons) -> IconMenuItem {
    IconMenuItem::new(
        keep_awake_title(enabled),
        true,
        Some(icons.keep_awake.clone()),
        None,
    )
}

pub fn apply_keep_awake(item: &IconMenuItem, enabled: bool, icons: &MenuIcons) {
    item.set_text(keep_awake_title(enabled));
    item.set_icon(Some(icons.keep_awake.clone()));
}

fn keep_awake_title(enabled: bool) -> String {
    let label = crate::keep_awake::label();
    if enabled {
        format!("✓  {label}")
    } else {
        label.to_string()
    }
}

fn status_bitmap(icons: &MenuIcons, kind: StatusKind) -> Icon {
    match kind {
        StatusKind::Running => icons.dot_running.clone(),
        StatusKind::Stopped => icons.dot_stopped.clone(),
    }
}

fn action_bitmap(icons: &MenuIcons, kind: ActionKind) -> Icon {
    match kind {
        ActionKind::Open => icons.open.clone(),
        ActionKind::Pause => icons.pause.clone(),
        ActionKind::Resume => icons.resume.clone(),
        ActionKind::Update => icons.update.clone(),
        ActionKind::Show => icons.show.clone(),
        ActionKind::Quit => icons.quit.clone(),
    }
}

/// Ink for monochrome glyphs. Light glyphs on dark menus; dark glyphs on light.
fn ink_for(light_glyphs: bool) -> (u8, u8, u8) {
    if light_glyphs {
        (0xf2, 0xf2, 0xf7)
    } else {
        (0x1c, 0x1c, 0x1e)
    }
}

/// Current menu-chrome ink, also used to tint the non-template awake tray icon.
pub fn current_ink() -> (u8, u8, u8) {
    ink_for(prefer_light_glyphs())
}

/// Re-stamp every tray-menu icon after [`MenuIcons::refresh_for_appearance`].
pub fn reapply_all(
    icons: &MenuIcons,
    status: &IconMenuItem,
    status_kind: StatusKind,
    open: &IconMenuItem,
    pause: &IconMenuItem,
    pause_kind: ActionKind,
    keep_awake: &IconMenuItem,
    keep_awake_enabled: bool,
    update: &IconMenuItem,
    settings: &IconMenuItem,
    quit: &IconMenuItem,
) {
    apply_status(status, status_kind, icons);
    apply_action(open, ActionKind::Open, icons);
    apply_action(pause, pause_kind, icons);
    apply_keep_awake(keep_awake, keep_awake_enabled, icons);
    apply_action(update, ActionKind::Update, icons);
    apply_action(settings, ActionKind::Show, icons);
    apply_action(quit, ActionKind::Quit, icons);
}

fn prefer_light_glyphs() -> bool {
    #[cfg(target_os = "macos")]
    {
        // `AppleInterfaceStyle` is unset in light mode; "Dark" when dark.
        macos_interface_style_is_dark().unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        // AppsUseLightTheme=1 → light app chrome / menus → dark ink.
        // Missing key or query failure: Win11 tray menus are usually dark.
        !windows_apps_use_light_theme().unwrap_or(false)
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        std::env::var_os("GTK_THEME")
            .map(|v| v.to_string_lossy().to_ascii_lowercase().contains("dark"))
            .unwrap_or(false)
    }
}

#[cfg(target_os = "macos")]
fn macos_interface_style_is_dark() -> Option<bool> {
    use std::process::Command;
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .ok()?;
    if !output.status.success() {
        // Command fails when the key is absent (light mode).
        return Some(false);
    }
    let style = String::from_utf8_lossy(&output.stdout);
    Some(style.to_ascii_lowercase().contains("dark"))
}

/// `HKCU\...\Personalize\AppsUseLightTheme` — 1 light, 0 dark.
///
/// In-process Advapi32 read — never spawn `reg.exe` (that flashed a console
/// and stalled the UI thread every status poll).
#[cfg(target_os = "windows")]
fn windows_apps_use_light_theme() -> Option<bool> {
    crate::win_reg::hkcu_get_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .map(|n| n != 0)
}

#[cfg(test)]
mod glyph_bounds_tests {
    use super::{paint_glyph_rgba, LOGICAL_SIZE, RASTER_SIZE, STROKE};

    const GLYPHS: &[&str] = &[
        "running",
        "stopped",
        "open",
        "pause",
        "resume",
        "update",
        "show",
        "keep_awake",
        "quit",
    ];

    #[test]
    fn shared_stroke_is_uniform() {
        assert!((STROKE - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn raster_size_matches_the_platform_menu_backend() {
        #[cfg(target_os = "macos")]
        assert_eq!(RASTER_SIZE, 36, "AppKit uses an 18 pt menu-image slot");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(RASTER_SIZE, 16, "Windows and GTK consume 16 px images");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_native_icons_only_for_exact_action_matches() {
        use super::{native_action, ActionKind, NativeIcon};

        assert_eq!(
            native_action(ActionKind::Open),
            Some(NativeIcon::ColumnView)
        );
        assert_eq!(
            native_action(ActionKind::Resume),
            Some(NativeIcon::RightFacingTriangle)
        );
        assert_eq!(native_action(ActionKind::Update), Some(NativeIcon::Refresh));
        assert_eq!(native_action(ActionKind::Pause), None);
        assert_eq!(native_action(ActionKind::Show), None);
        assert_eq!(native_action(ActionKind::Quit), None);
    }

    #[test]
    fn every_glyph_builds_and_stays_inside_canvas() {
        let ink = (0x1c, 0x1c, 0x1e);
        for name in GLYPHS {
            // Building the tray Icon proves RGBA dims are valid.
            let _ = match *name {
                "running" => super::draw_status_running(ink),
                "stopped" => super::draw_status_stopped(ink),
                "open" => super::draw_open(ink),
                "pause" => super::draw_pause(ink),
                "resume" => super::draw_resume(ink),
                "update" => super::draw_update(ink),
                "show" => super::draw_show(ink),
                "keep_awake" => super::draw_keep_awake(ink),
                "quit" => super::draw_quit(ink),
                other => panic!("unknown glyph {other}"),
            }
            .unwrap_or_else(|e| panic!("{name}: {e}"));

            let rgba = paint_glyph_rgba(name, ink, RASTER_SIZE);
            let (min_x, min_y, max_x, max_y) =
                ink_bounds(&rgba).unwrap_or_else(|| panic!("{name}: expected non-empty ink"));
            assert!(
                min_x >= 1 && min_y >= 1 && max_x < RASTER_SIZE - 1 && max_y < RASTER_SIZE - 1,
                "{name}: ink touches canvas edge (bounds {min_x},{min_y}..{max_x},{max_y}); \
                 leave ≥1 px margin so AA fringe isn't clipped"
            );
        }
    }

    #[test]
    fn glyphs_share_similar_optical_size() {
        let ink = (0x1c, 0x1c, 0x1e);
        let mut spans = Vec::new();
        for name in [
            "running",
            "open",
            "pause",
            "update",
            "show",
            "keep_awake",
            "quit",
        ] {
            let rgba = paint_glyph_rgba(name, ink, RASTER_SIZE);
            let (min_x, min_y, max_x, max_y) = ink_bounds(&rgba).expect(name);
            let w = max_x - min_x + 1;
            let h = max_y - min_y + 1;
            spans.push((name, w, h));
            let scale = RASTER_SIZE as f32 / LOGICAL_SIZE;
            let min_span = (16.0 * scale).floor() as u32;
            let max_span = (24.0 * scale).ceil() as u32;
            assert!(
                (min_span..=max_span).contains(&w) && (min_span..=max_span).contains(&h),
                "{name}: optical size {w}×{h} outside {min_span}..{max_span} (family mismatch)"
            );
        }
        let ws: Vec<u32> = spans.iter().map(|(_, w, _)| *w).collect();
        let min_w = *ws.iter().min().unwrap();
        let max_w = *ws.iter().max().unwrap();
        assert!(
            max_w - min_w <= 6,
            "width spread too large across family: {spans:?}"
        );
    }

    fn ink_bounds(rgba: &[u8]) -> Option<(u32, u32, u32, u32)> {
        let size = (rgba.len() / 4) as f32;
        let size = size.sqrt() as u32;
        let mut min_x = size;
        let mut min_y = size;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut any = false;
        for y in 0..size {
            for x in 0..size {
                let a = rgba[((y * size + x) * 4 + 3) as usize];
                if a > 16 {
                    any = true;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        any.then_some((min_x, min_y, max_x, max_y))
    }

    #[test]
    fn quit_is_a_power_symbol_with_a_top_gap_and_centered_stem() {
        const TEST_SIZE: u32 = 64;
        let rgba = paint_glyph_rgba("quit", (0x1c, 0x1c, 0x1e), TEST_SIZE);
        let alpha_at = |logical_x: f32, logical_y: f32| {
            let x = (logical_x / LOGICAL_SIZE * TEST_SIZE as f32).floor() as u32;
            let y = (logical_y / LOGICAL_SIZE * TEST_SIZE as f32).floor() as u32;
            rgba[((y * TEST_SIZE + x) * 4 + 3) as usize]
        };

        assert!(alpha_at(15.5, 8.0) > 180, "stem should be solid");
        assert!(alpha_at(12.5, 8.0) < 32, "left side of gap should be clear");
        assert!(
            alpha_at(18.5, 8.0) < 32,
            "right side of gap should be clear"
        );
        assert!(alpha_at(15.5, 25.5) > 180, "ring bottom should be solid");
    }

    #[test]
    fn settings_gear_is_outline_only() {
        const TEST_SIZE: u32 = 64;
        let rgba = paint_glyph_rgba("show", (0x1c, 0x1c, 0x1e), TEST_SIZE);
        let alpha_at = |logical_x: f32, logical_y: f32| {
            let x = (logical_x / LOGICAL_SIZE * TEST_SIZE as f32).floor() as u32;
            let y = (logical_y / LOGICAL_SIZE * TEST_SIZE as f32).floor() as u32;
            rgba[((y * TEST_SIZE + x) * 4 + 3) as usize]
        };

        assert!(alpha_at(15.5, 15.5) < 32, "gear center should be clear");
        assert!(alpha_at(18.5, 15.5) > 180, "inner ring should be visible");
        assert!(alpha_at(20.5, 15.5) < 32, "gear body should not be filled");
        assert!(alpha_at(25.0, 15.5) > 180, "outer teeth should be visible");
    }

    #[test]
    fn every_fallback_renders_directly_at_windows_and_gtk_size() {
        const BACKEND_SIZE: u32 = 16;
        for name in GLYPHS {
            let rgba = paint_glyph_rgba(name, (0x1c, 0x1c, 0x1e), BACKEND_SIZE);
            assert_eq!(rgba.len(), (BACKEND_SIZE * BACKEND_SIZE * 4) as usize);
            let (min_x, min_y, max_x, max_y) = ink_bounds(&rgba).expect(name);
            assert!(
                min_x > 0 && min_y > 0 && max_x < BACKEND_SIZE - 1 && max_y < BACKEND_SIZE - 1,
                "{name}: 16 px fallback clips at {min_x},{min_y}..{max_x},{max_y}"
            );
        }
    }
}

#[cfg(test)]
fn paint_glyph_rgba(name: &str, ink: (u8, u8, u8), size: u32) -> Vec<u8> {
    let mut px = Canvas::with_size(size);
    match name {
        "running" => {
            let cx = 15.5;
            let cy = 15.5;
            px.stroke_circle_aa(cx, cy, 9.0, ink, STROKE);
            px.stroke_circle_aa(cx, cy, 3.5, ink, STROKE);
        }
        "stopped" => px.stroke_circle_aa(15.5, 15.5, 9.0, ink, STROKE),
        "open" => paint_open(&mut px, ink),
        "pause" => paint_pause(&mut px, ink),
        "resume" => paint_resume(&mut px, ink),
        "update" => paint_update(&mut px, ink),
        "show" => paint_show(&mut px, ink),
        "keep_awake" => paint_keep_awake(&mut px, ink),
        "quit" => paint_quit(&mut px, ink),
        other => panic!("unknown glyph {other}"),
    }
    px.rgba
}

// --- Shared anti-aliased outline glyphs in a 32-unit logical canvas ---

fn draw_status_running(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    px.stroke_circle_aa(cx, cy, 9.0, c, STROKE);
    px.stroke_circle_aa(cx, cy, 3.5, c, STROKE);
    px.into_icon()
}

fn draw_status_stopped(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_circle_aa(15.5, 15.5, 9.0, c, STROKE);
    px.into_icon()
}

/// Panel with a left rail — Lucide `panel-left` / "open the panel".
fn draw_open(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_open(&mut px, c);
    px.into_icon()
}

fn paint_open(px: &mut Canvas, c: (u8, u8, u8)) {
    px.stroke_round_rect(6.5, 6.5, 25.5, 25.5, 3.0, c, STROKE);
    // Left sidebar rail (narrower than half — reads as a panel, not columns).
    px.stroke_line_aa(12.0, 7.5, 12.0, 24.5, c, STROKE);
}

/// Two rounded outline bars — Lucide pause in the shared family stroke.
fn draw_pause(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_pause(&mut px, c);
    px.into_icon()
}

fn paint_pause(px: &mut Canvas, c: (u8, u8, u8)) {
    px.stroke_round_rect(8.0, 7.0, 13.5, 25.0, 1.75, c, STROKE);
    px.stroke_round_rect(18.5, 7.0, 24.0, 25.0, 1.75, c, STROKE);
}

/// Outline play triangle (Lucide play).
fn draw_resume(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_resume(&mut px, c);
    px.into_icon()
}

fn paint_resume(px: &mut Canvas, c: (u8, u8, u8)) {
    px.stroke_line_aa(9.5, 6.5, 25.0, 16.0, c, STROKE);
    px.stroke_line_aa(25.0, 16.0, 9.5, 25.5, c, STROKE);
    px.stroke_line_aa(9.5, 25.5, 9.5, 6.5, c, STROKE);
}

/// Circular refresh arrow (Lucide `refresh-cw`).
fn draw_update(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_update(&mut px, c);
    px.into_icon()
}

fn paint_update(px: &mut Canvas, c: (u8, u8, u8)) {
    let cx = 15.5;
    let cy = 15.5;
    // Open ring with gap at top-right for the arrowhead.
    px.stroke_arc_aa(cx, cy, 9.0, 55.0, 315.0, c, STROKE);
    // Open chevron arrowhead keeps the refresh icon outline-only.
    px.stroke_line_aa(cx + 3.5, cy - 9.0, cx + 10.0, cy - 8.0, c, STROKE);
    px.stroke_line_aa(cx + 10.0, cy - 8.0, cx + 6.0, cy - 3.0, c, STROKE);
}

/// Outlined gear for Settings.
fn draw_show(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_show(&mut px, c);
    px.into_icon()
}

fn paint_show(px: &mut Canvas, c: (u8, u8, u8)) {
    const CX: f32 = 15.5;
    const CY: f32 = 15.5;
    const ROOT_R: f32 = 7.5;
    const TOOTH_R: f32 = 10.0;
    let mut outline = Vec::with_capacity(8 * 6);

    for tooth in 0..8 {
        let center = tooth as f32 * 45.0;
        for (offset, radius) in [
            (-22.5_f32, ROOT_R),
            (-13.0, ROOT_R),
            (-13.0, TOOTH_R),
            (13.0, TOOTH_R),
            (13.0, ROOT_R),
            (22.5, ROOT_R),
        ] {
            let angle = (center + offset).to_radians();
            outline.push((CX + radius * angle.cos(), CY + radius * angle.sin()));
        }
    }

    px.stroke_polygon_aa(&outline, c, STROKE);
    px.stroke_circle_aa(CX, CY, 3.0, c, STROKE);
}

/// Outline crescent moon for Keep awake (Lucide `moon` — sleep control).
fn draw_keep_awake(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_keep_awake(&mut px, c);
    px.into_icon()
}

fn paint_keep_awake(px: &mut Canvas, c: (u8, u8, u8)) {
    let cx = 17.0;
    let cy = 15.5;
    let r = 9.0;
    let cut_cx = cx + 5.0;
    let cut_cy = cy - 3.0;
    let cut_r = 8.0;
    px.stroke_crescent_aa(cx, cy, r, cut_cx, cut_cy, cut_r, c, STROKE);
}

/// Power symbol (circle with stem) for Quit.
fn draw_quit(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_quit(&mut px, c);
    px.into_icon()
}

fn paint_quit(px: &mut Canvas, c: (u8, u8, u8)) {
    // Travel from the upper-right around the bottom to the upper-left, leaving
    // a centered top gap for the stem. The old 52°..308° arc left its gap on
    // the right, which made the glyph look like a broken refresh icon.
    px.stroke_arc_aa(15.5, 16.5, 9.0, -52.0, 232.0, c, STROKE);
    px.stroke_line_aa(15.5, 6.0, 15.5, 16.0, c, STROKE);
}

struct Canvas {
    rgba: Vec<u8>,
    size: u32,
    scale: f32,
}

impl Canvas {
    fn new() -> Self {
        Self::with_size(RASTER_SIZE)
    }

    fn with_size(size: u32) -> Self {
        Self {
            rgba: vec![0u8; (size * size * 4) as usize],
            size,
            scale: size as f32 / LOGICAL_SIZE,
        }
    }

    fn into_icon(self) -> Result<Icon, tray_icon::menu::BadIcon> {
        Icon::from_rgba(self.rgba, self.size, self.size)
    }

    fn blend(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: f32) {
        if x < 0 || y < 0 || x >= self.size as i32 || y >= self.size as i32 {
            return;
        }
        let a = a.clamp(0.0, 1.0);
        let i = ((y as u32 * self.size + x as u32) * 4) as usize;
        let src_a = (a * 255.0).round() as u16;
        let dst_a = self.rgba[i + 3] as u16;
        if src_a >= dst_a {
            self.rgba[i] = r;
            self.rgba[i + 1] = g;
            self.rgba[i + 2] = b;
            self.rgba[i + 3] = src_a as u8;
        }
    }

    fn fill_circle_aa(&mut self, cx: f32, cy: f32, radius: f32, r: u8, g: u8, b: u8) {
        let cx = cx * self.scale;
        let cy = cy * self.scale;
        let radius = radius * self.scale;
        for y in 0..self.size as i32 {
            for x in 0..self.size as i32 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let alpha = coverage(d, radius);
                if alpha > 0.01 {
                    self.blend(x, y, r, g, b, alpha);
                }
            }
        }
    }

    fn stroke_circle_aa(&mut self, cx: f32, cy: f32, radius: f32, c: (u8, u8, u8), thickness: f32) {
        let cx = cx * self.scale;
        let cy = cy * self.scale;
        let radius = radius * self.scale;
        let thickness = thickness * self.scale;
        for y in 0..self.size as i32 {
            for x in 0..self.size as i32 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let alpha = coverage((d - radius).abs(), thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
        }
    }

    fn stroke_round_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        let x0 = x0 * self.scale;
        let y0 = y0 * self.scale;
        let x1 = x1 * self.scale;
        let y1 = y1 * self.scale;
        let radius = radius * self.scale;
        let thickness = thickness * self.scale;
        for y in 0..self.size as i32 {
            for x in 0..self.size as i32 {
                let d = sd_round_rect(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, radius).abs();
                let alpha = coverage(d, thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
        }
    }

    fn stroke_line_aa(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        let steps = (len * self.scale * 2.0).ceil() as i32;
        let half = thickness / 2.0;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            self.fill_circle_aa(x0 + dx * t, y0 + dy * t, half, c.0, c.1, c.2);
        }
    }

    fn stroke_arc_aa(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        start_deg: f32,
        end_deg: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        let steps = 96;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let deg = start_deg + (end_deg - start_deg) * t;
            let rad = deg.to_radians();
            self.fill_circle_aa(
                cx + radius * rad.cos(),
                cy + radius * rad.sin(),
                thickness / 2.0,
                c.0,
                c.1,
                c.2,
            );
        }
    }

    fn stroke_polygon_aa(&mut self, points: &[(f32, f32)], c: (u8, u8, u8), thickness: f32) {
        for index in 0..points.len() {
            let (x0, y0) = points[index];
            let (x1, y1) = points[(index + 1) % points.len()];
            self.stroke_line_aa(x0, y0, x1, y1, c, thickness);
        }
    }

    /// Outline of `circle(cx,cy,r) \ circle(cut_cx,cut_cy,cut_r)`.
    #[allow(clippy::too_many_arguments)]
    fn stroke_crescent_aa(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        cut_cx: f32,
        cut_cy: f32,
        cut_r: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        let cx = cx * self.scale;
        let cy = cy * self.scale;
        let r = r * self.scale;
        let cut_cx = cut_cx * self.scale;
        let cut_cy = cut_cy * self.scale;
        let cut_r = cut_r * self.scale;
        let thickness = thickness * self.scale;
        for y in 0..self.size as i32 {
            for x in 0..self.size as i32 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let d_outer = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r;
                let d_cut = ((px - cut_cx).powi(2) + (py - cut_cy).powi(2)).sqrt() - cut_r;
                let d = d_outer.max(-d_cut);
                let alpha = coverage(d.abs(), thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
        }
    }
}

fn coverage(dist: f32, radius: f32) -> f32 {
    // A one-pixel transition (0.5 px on each side) stays sharp at the exact
    // backend target size while retaining smooth diagonal and curved edges.
    let edge = 0.5;
    if dist <= radius - edge {
        1.0
    } else if dist >= radius + edge {
        0.0
    } else {
        (radius + edge - dist) / (2.0 * edge)
    }
}

fn sd_round_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> f32 {
    let half_w = (x1 - x0) * 0.5;
    let half_h = (y1 - y0) * 0.5;
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let dx = (px - cx).abs() - (half_w - radius);
    let dy = (py - cy).abs() - (half_h - radius);
    let ax = dx.max(0.0);
    let ay = dy.max(0.0);
    (ax * ax + ay * ay).sqrt() + dx.min(0.0).max(dy.min(0.0)) - radius
}
