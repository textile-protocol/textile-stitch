// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Tray-menu icons — one family of monochrome Lucide-style glyphs on every OS.
//!
//! AppKit named images mix templates, filled status dots, and full-color
//! assets (`Computer`), so we draw the same anti-aliased 32×32 bitmaps
//! everywhere. Ink follows the menu chrome (dark glyphs on light menus,
//! light on dark).
//!
//! Design rules for the set (Lucide conventions at 32×32):
//! - Shared stroke weight ([`STROKE`]) and ~6 px padding (optical ~20×20).
//! - Outline strokes for wireframe shapes; solid fills for pause, play,
//!   moon, and the refresh arrowhead so weight matches at menu size.
//! - No mixed per-glyph stroke thicknesses.

use tray_icon::menu::Icon;
use tray_icon::menu::IconMenuItem;

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

/// Shared outline stroke for every glyph in the set.
const STROKE: f32 = 2.0;

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
            // Outline rings; running adds a solid inner disc (radio-on).
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
    IconMenuItem::new(text, true, Some(action_bitmap(icons, kind)), None)
}

pub fn apply_status(item: &IconMenuItem, kind: StatusKind, icons: &MenuIcons) {
    item.set_icon(Some(status_bitmap(icons, kind)));
}

pub fn apply_action(item: &IconMenuItem, kind: ActionKind, icons: &MenuIcons) {
    item.set_icon(Some(action_bitmap(icons, kind)));
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
#[cfg(target_os = "windows")]
fn windows_apps_use_light_theme() -> Option<bool> {
    use std::process::Command;
    // Hide the console — this runs on every appearance poll (~2s) and would
    // otherwise flash `reg.exe` on the desktop.
    let mut cmd = Command::new("reg");
    cmd.args([
        "query",
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "/v",
        "AppsUseLightTheme",
    ]);
    crate::win_cmd::no_window(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_apps_use_light_theme_reg(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `reg query … /v AppsUseLightTheme` stdout.
#[cfg(any(test, target_os = "windows"))]
fn parse_apps_use_light_theme_reg(stdout: &str) -> Option<bool> {
    // Typical line: `    AppsUseLightTheme    REG_DWORD    0x1`
    let value = stdout
        .lines()
        .find(|line| line.contains("AppsUseLightTheme"))?
        .split_whitespace()
        .last()?;
    let n = u32::from_str_radix(value.trim_start_matches("0x"), 16).ok()?;
    Some(n != 0)
}

#[cfg(test)]
mod theme_ink_tests {
    use super::parse_apps_use_light_theme_reg;

    #[test]
    fn parses_light_and_dark_reg_output() {
        let light = "\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\n    AppsUseLightTheme    REG_DWORD    0x1\n\n";
        let dark = "\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\n    AppsUseLightTheme    REG_DWORD    0x0\n\n";
        assert_eq!(parse_apps_use_light_theme_reg(light), Some(true));
        assert_eq!(parse_apps_use_light_theme_reg(dark), Some(false));
        assert_eq!(parse_apps_use_light_theme_reg("nope"), None);
    }
}

#[cfg(test)]
mod glyph_bounds_tests {
    use super::{paint_glyph_rgba, SIZE, STROKE};

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

            let rgba = paint_glyph_rgba(name, ink);
            let (min_x, min_y, max_x, max_y) =
                ink_bounds(&rgba).unwrap_or_else(|| panic!("{name}: expected non-empty ink"));
            assert!(
                min_x >= 1 && min_y >= 1 && max_x < SIZE - 1 && max_y < SIZE - 1,
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
            let rgba = paint_glyph_rgba(name, ink);
            let (min_x, min_y, max_x, max_y) = ink_bounds(&rgba).expect(name);
            let w = max_x - min_x + 1;
            let h = max_y - min_y + 1;
            spans.push((name, w, h));
            assert!(
                (16..=24).contains(&w) && (16..=24).contains(&h),
                "{name}: optical size {w}×{h} outside 16..24 (family mismatch)"
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
        let mut min_x = SIZE;
        let mut min_y = SIZE;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut any = false;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let a = rgba[((y * SIZE + x) * 4 + 3) as usize];
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
}

#[cfg(test)]
fn paint_glyph_rgba(name: &str, ink: (u8, u8, u8)) -> Vec<u8> {
    let mut px = Canvas::new();
    match name {
        "running" => {
            let cx = 15.5;
            let cy = 15.5;
            px.stroke_circle_aa(cx, cy, 9.0, ink, STROKE);
            px.fill_circle_aa(cx, cy, 3.5, ink.0, ink.1, ink.2);
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

// --- Shared 32×32 anti-aliased outline glyphs ---

const SIZE: u32 = 32;

fn draw_status_running(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    px.stroke_circle_aa(cx, cy, 9.0, c, STROKE);
    px.fill_circle_aa(cx, cy, 3.5, c.0, c.1, c.2);
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

/// Two solid rounded bars — Lucide pause; solid weight matches outline siblings.
fn draw_pause(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_pause(&mut px, c);
    px.into_icon()
}

fn paint_pause(px: &mut Canvas, c: (u8, u8, u8)) {
    px.fill_round_rect(8.0, 7.0, 13.5, 25.0, 1.75, c);
    px.fill_round_rect(18.5, 7.0, 24.0, 25.0, 1.75, c);
}

/// Solid play triangle (Lucide play).
fn draw_resume(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_resume(&mut px, c);
    px.into_icon()
}

fn paint_resume(px: &mut Canvas, c: (u8, u8, u8)) {
    // CCW winding (top → tip → bottom) for a correct inside SDF.
    px.fill_triangle_aa(9.5, 6.5, 25.0, 16.0, 9.5, 25.5, c);
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
    // Filled arrowhead at the clockwise end of the arc.
    px.fill_triangle_aa(
        cx + 3.5,
        cy - 9.0,
        cx + 10.0,
        cy - 8.0,
        cx + 6.0,
        cy - 3.0,
        c,
    );
}

/// Horizontal sliders — Settings at menu size; radial "gears" read as suns.
fn draw_show(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_show(&mut px, c);
    px.into_icon()
}

fn paint_show(px: &mut Canvas, c: (u8, u8, u8)) {
    // Tracks stop at the knob so the stroke doesn't cut through (Lucide).
    const KNOB_R: f32 = 3.0;
    const GAP: f32 = 1.25;
    for &(y, knob_x) in &[(9.5_f32, 12.0_f32), (16.0, 20.5), (22.5, 14.5)] {
        let left = 6.5_f32;
        let right = 25.5_f32;
        let stop_l = knob_x - KNOB_R - GAP;
        let stop_r = knob_x + KNOB_R + GAP;
        if stop_l > left {
            px.stroke_line_aa(left, y, stop_l, y, c, STROKE);
        }
        if stop_r < right {
            px.stroke_line_aa(stop_r, y, right, y, c, STROKE);
        }
        px.stroke_circle_aa(knob_x, y, KNOB_R, c, STROKE);
    }
}

/// Solid crescent moon for Keep awake (Lucide `moon` — sleep control).
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
    px.fill_crescent_aa(cx, cy, r, cut_cx, cut_cy, cut_r, c);
}

/// Power symbol (circle with stem) for Quit.
fn draw_quit(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    paint_quit(&mut px, c);
    px.into_icon()
}

fn paint_quit(px: &mut Canvas, c: (u8, u8, u8)) {
    // Centered in the 32×32 canvas (stem + open ring).
    px.stroke_arc_aa(15.5, 16.5, 9.0, 52.0, 308.0, c, STROKE);
    px.stroke_line_aa(15.5, 6.0, 15.5, 16.0, c, STROKE);
}

struct Canvas {
    rgba: Vec<u8>,
}

impl Canvas {
    fn new() -> Self {
        Self {
            rgba: vec![0u8; (SIZE * SIZE * 4) as usize],
        }
    }

    fn into_icon(self) -> Result<Icon, tray_icon::menu::BadIcon> {
        Icon::from_rgba(self.rgba, SIZE, SIZE)
    }

    fn blend(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: f32) {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            return;
        }
        let a = a.clamp(0.0, 1.0);
        let i = ((y as u32 * SIZE + x as u32) * 4) as usize;
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
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
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
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
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
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let d = sd_round_rect(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, radius).abs();
                let alpha = coverage(d, thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
        }
    }

    fn fill_round_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        c: (u8, u8, u8),
    ) {
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let d = sd_round_rect(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, radius);
                // Negative SDF is inside; coverage() expects distance from edge outward.
                let alpha = if d <= 0.0 { 1.0 } else { coverage(d, 0.0) };
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
        let steps = (len * 2.0).ceil() as i32;
        let half = thickness / 2.0;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            self.fill_circle_aa(x0 + dx * t, y0 + dy * t, half, c.0, c.1, c.2);
        }
    }

    fn fill_triangle_aa(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        c: (u8, u8, u8),
    ) {
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let d = sd_triangle(px, py, x0, y0, x1, y1, x2, y2);
                let alpha = if d <= 0.0 { 1.0 } else { coverage(d, 0.0) };
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
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

    /// Filled `circle(cx,cy,r) \ circle(cut_cx,cut_cy,cut_r)` (crescent moon).
    fn fill_crescent_aa(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        cut_cx: f32,
        cut_cy: f32,
        cut_r: f32,
        c: (u8, u8, u8),
    ) {
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let d_outer = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r;
                let d_cut = ((px - cut_cx).powi(2) + (py - cut_cy).powi(2)).sqrt() - cut_r;
                let d = d_outer.max(-d_cut);
                let alpha = if d <= 0.0 { 1.0 } else { coverage(d, 0.0) };
                if alpha > 0.01 {
                    self.blend(x, y, c.0, c.1, c.2, alpha);
                }
            }
        }
    }
}

fn coverage(dist: f32, radius: f32) -> f32 {
    let edge = 1.0;
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

fn sd_triangle(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    // Signed distance via edge half-planes + vertex distances. Assumes CCW winding.
    let e0x = x1 - x0;
    let e0y = y1 - y0;
    let e1x = x2 - x1;
    let e1y = y2 - y1;
    let e2x = x0 - x2;
    let e2y = y0 - y2;
    let v0x = px - x0;
    let v0y = py - y0;
    let v1x = px - x1;
    let v1y = py - y1;
    let v2x = px - x2;
    let v2y = py - y2;

    let cross0 = e0x * v0y - e0y * v0x;
    let cross1 = e1x * v1y - e1y * v1x;
    let cross2 = e2x * v2y - e2y * v2x;
    let inside = cross0 >= 0.0 && cross1 >= 0.0 && cross2 >= 0.0;

    let d0 = dist_to_segment(px, py, x0, y0, x1, y1);
    let d1 = dist_to_segment(px, py, x1, y1, x2, y2);
    let d2 = dist_to_segment(px, py, x2, y2, x0, y0);
    let dist = d0.min(d1).min(d2);
    if inside {
        -dist
    } else {
        dist
    }
}

fn dist_to_segment(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len2 = (dx * dx + dy * dy).max(0.0001);
    let t = ((px - x0) * dx + (py - y0) * dy) / len2;
    let t = t.clamp(0.0, 1.0);
    let sx = x0 + dx * t;
    let sy = y0 + dy * t;
    ((px - sx).powi(2) + (py - sy).powi(2)).sqrt()
}
