// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Tray-menu icons — one family of monochrome outline glyphs on every OS.
//!
//! AppKit named images mix templates, filled status dots, and full-color
//! assets (`Computer`), so we draw the same anti-aliased 32×32 bitmaps
//! everywhere. Ink follows the menu chrome (dark glyphs on light menus,
//! light on dark). Stroke weight is shared so the set reads as one family.

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
        let ink = menu_ink();
        Self {
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
/// [`IconMenuItem`] with a sleep (zZZ) glyph; on-state uses a leading checkmark
/// in the title (AppKit's state column isn't available on icon items).
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
fn menu_ink() -> (u8, u8, u8) {
    if prefer_light_glyphs() {
        (0xf2, 0xf2, 0xf7)
    } else {
        (0x1c, 0x1c, 0x1e)
    }
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
    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
        .ok()?;
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

// --- Shared 32×32 anti-aliased outline glyphs ---

const SIZE: u32 = 32;

fn draw_status_running(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    px.stroke_circle_aa(cx, cy, 9.0, c, STROKE);
    px.fill_circle_aa(cx, cy, 4.0, c.0, c.1, c.2);
    px.into_icon()
}

fn draw_status_stopped(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_circle_aa(15.5, 15.5, 9.0, c, STROKE);
    px.into_icon()
}

/// Outlined panel with an inset chevron — "open the panel".
fn draw_open(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_round_rect(6.0, 6.0, 26.0, 26.0, 3.0, c, STROKE);
    // Chevron pointing into the panel (→).
    px.stroke_line_aa(12.0, 11.0, 18.0, 15.5, c, STROKE);
    px.stroke_line_aa(18.0, 15.5, 12.0, 20.0, c, STROKE);
    px.into_icon()
}

/// Two outlined vertical bars.
fn draw_pause(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_round_rect(9.0, 8.0, 13.5, 24.0, 1.5, c, STROKE);
    px.stroke_round_rect(18.5, 8.0, 23.0, 24.0, 1.5, c, STROKE);
    px.into_icon()
}

/// Outlined play triangle.
fn draw_resume(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_triangle_aa(10.0, 7.0, 10.0, 25.0, 24.0, 16.0, c, STROKE);
    px.into_icon()
}

/// Outlined circular arrows (refresh).
fn draw_update(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    // Open ring (gap at top-right) + arrow head.
    px.stroke_arc_aa(cx, cy, 9.0, 40.0, 300.0, c, STROKE);
    // Arrow head pointing clockwise at the gap.
    px.stroke_line_aa(cx + 5.5, cy - 7.5, cx + 9.0, cy - 4.0, c, STROKE);
    px.stroke_line_aa(cx + 9.0, cy - 4.0, cx + 5.0, cy - 3.0, c, STROKE);
    px.into_icon()
}

/// Outlined gear for Settings.
fn draw_show(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    px.stroke_circle_aa(cx, cy, 4.5, c, STROKE);
    // Six teeth as short radial strokes.
    for i in 0..6 {
        let ang = (i as f32 * 60.0).to_radians();
        let x0 = cx + ang.cos() * 7.0;
        let y0 = cy + ang.sin() * 7.0;
        let x1 = cx + ang.cos() * 11.0;
        let y1 = cy + ang.sin() * 11.0;
        px.stroke_line_aa(x0, y0, x1, y1, c, STROKE);
    }
    px.into_icon()
}

/// Outlined zZZ sleep glyph for Keep awake (💤).
fn draw_keep_awake(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    // Three Z's of increasing size, rising left → right like 💤.
    stroke_z(&mut px, 5.0, 18.0, 7.0, c, 1.6);
    stroke_z(&mut px, 11.5, 11.5, 9.5, c, 1.8);
    stroke_z(&mut px, 19.0, 4.5, 12.0, c, STROKE);
    px.into_icon()
}

/// One block-letter Z: top bar, diagonal, bottom bar.
fn stroke_z(px: &mut Canvas, x: f32, y: f32, size: f32, c: (u8, u8, u8), thickness: f32) {
    let x1 = x + size;
    let y1 = y + size * 0.85;
    px.stroke_line_aa(x, y, x1, y, c, thickness);
    px.stroke_line_aa(x1, y, x, y1, c, thickness);
    px.stroke_line_aa(x, y1, x1, y1, c, thickness);
}

/// Outlined power symbol (circle with stem) for Quit.
fn draw_quit(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_arc_aa(15.5, 17.0, 9.0, 45.0, 315.0, c, STROKE);
    px.stroke_line_aa(15.5, 6.0, 15.5, 16.0, c, STROKE);
    px.into_icon()
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

    fn stroke_triangle_aa(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        self.stroke_line_aa(x0, y0, x1, y1, c, thickness);
        self.stroke_line_aa(x1, y1, x2, y2, c, thickness);
        self.stroke_line_aa(x2, y2, x0, y0, c, thickness);
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
