// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Tray-menu icons.
//!
//! On macOS we use AppKit named images via muda's [`NativeIcon`] — the same
//! template / status assets system menus use, so they stay crisp and follow
//! light/dark appearance. On Windows / Linux we draw anti-aliased 32×32
//! bitmaps (no SF Symbols there).

#[cfg(not(target_os = "macos"))]
use tray_icon::menu::Icon;
use tray_icon::menu::IconMenuItem;
#[cfg(target_os = "macos")]
use tray_icon::menu::NativeIcon;

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

/// Fallback bitmaps for non-macOS. Empty on macOS (native icons only).
pub struct MenuIcons {
    #[cfg(not(target_os = "macos"))]
    dot_running: Icon,
    #[cfg(not(target_os = "macos"))]
    dot_stopped: Icon,
    #[cfg(not(target_os = "macos"))]
    open: Icon,
    #[cfg(not(target_os = "macos"))]
    pause: Icon,
    #[cfg(not(target_os = "macos"))]
    resume: Icon,
    #[cfg(not(target_os = "macos"))]
    update: Icon,
    #[cfg(not(target_os = "macos"))]
    show: Icon,
    #[cfg(not(target_os = "macos"))]
    quit: Icon,
}

impl MenuIcons {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {}
        }
        #[cfg(not(target_os = "macos"))]
        {
            let ink = menu_ink();
            Self {
                // Radius chosen so the disc reads as a circle at menu size, not a diamond.
                dot_running: aa_circle(0x34, 0xc7, 0x59, 10.0).expect("dot_running"),
                dot_stopped: aa_circle(0x8e, 0x8e, 0x93, 10.0).expect("dot_stopped"),
                open: draw_open(ink).expect("open"),
                pause: draw_pause(ink).expect("pause"),
                resume: draw_resume(ink).expect("resume"),
                update: draw_update(ink).expect("update"),
                show: draw_show(ink).expect("show"),
                quit: draw_quit(ink).expect("quit"),
            }
        }
    }
}

pub fn status_item(text: &str, kind: StatusKind, icons: &MenuIcons) -> IconMenuItem {
    #[cfg(target_os = "macos")]
    {
        let _ = icons;
        IconMenuItem::with_native_icon(text, false, Some(native_status(kind)), None)
    }
    #[cfg(not(target_os = "macos"))]
    {
        IconMenuItem::new(text, false, Some(status_bitmap(icons, kind)), None)
    }
}

pub fn action_item(text: &str, kind: ActionKind, icons: &MenuIcons) -> IconMenuItem {
    #[cfg(target_os = "macos")]
    {
        let _ = icons;
        IconMenuItem::with_native_icon(text, true, Some(native_action(kind)), None)
    }
    #[cfg(not(target_os = "macos"))]
    {
        IconMenuItem::new(text, true, Some(action_bitmap(icons, kind)), None)
    }
}

pub fn apply_status(item: &IconMenuItem, kind: StatusKind, icons: &MenuIcons) {
    #[cfg(target_os = "macos")]
    {
        let _ = icons;
        item.set_native_icon(Some(native_status(kind)));
    }
    #[cfg(not(target_os = "macos"))]
    {
        item.set_icon(Some(status_bitmap(icons, kind)));
    }
}

pub fn apply_action(item: &IconMenuItem, kind: ActionKind, icons: &MenuIcons) {
    #[cfg(target_os = "macos")]
    {
        let _ = icons;
        item.set_native_icon(Some(native_action(kind)));
    }
    #[cfg(not(target_os = "macos"))]
    {
        item.set_icon(Some(action_bitmap(icons, kind)));
    }
}

#[cfg(target_os = "macos")]
fn native_status(kind: StatusKind) -> NativeIcon {
    match kind {
        // AppKit statusAvailable = the standard green circle (same family Docker uses).
        StatusKind::Running => NativeIcon::StatusAvailable,
        StatusKind::Stopped => NativeIcon::StatusNone,
    }
}

#[cfg(target_os = "macos")]
fn native_action(kind: ActionKind) -> NativeIcon {
    match kind {
        ActionKind::Open => NativeIcon::FollowLinkFreestanding,
        // Named AppKit set has no pause bars; stop-progress is the closest system
        // glyph. Resume uses the right-facing triangle (play).
        ActionKind::Pause => NativeIcon::StopProgress,
        ActionKind::Resume => NativeIcon::RightFacingTriangle,
        ActionKind::Update => NativeIcon::Caution,
        ActionKind::Show => NativeIcon::PreferencesGeneral,
        ActionKind::Quit => NativeIcon::StopProgressFreestanding,
    }
}

#[cfg(not(target_os = "macos"))]
fn status_bitmap(icons: &MenuIcons, kind: StatusKind) -> Icon {
    match kind {
        StatusKind::Running => icons.dot_running.clone(),
        StatusKind::Stopped => icons.dot_stopped.clone(),
    }
}

#[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
fn menu_ink() -> (u8, u8, u8) {
    if prefer_light_glyphs() {
        (0xf2, 0xf2, 0xf7)
    } else {
        (0x1c, 0x1c, 0x1e)
    }
}

#[cfg(not(target_os = "macos"))]
fn prefer_light_glyphs() -> bool {
    #[cfg(target_os = "windows")]
    {
        // AppsUseLightTheme=1 → light app chrome / menus → dark ink.
        // Missing key or query failure: Win11 tray menus are usually dark.
        !windows_apps_use_light_theme().unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("GTK_THEME")
            .map(|v| v.to_string_lossy().to_ascii_lowercase().contains("dark"))
            .unwrap_or(false)
    }
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

// --- Windows / Linux bitmap fallbacks (32×32, anti-aliased) ---

#[cfg(not(target_os = "macos"))]
const SIZE: u32 = 32;

#[cfg(not(target_os = "macos"))]
fn aa_circle(r: u8, g: u8, b: u8, radius: f32) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let c = (SIZE as f32 - 1.0) / 2.0;
    px.fill_circle_aa(c, c, radius, r, g, b);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn draw_open(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_round_rect(5.0, 6.0, 26.0, 25.0, 2.0, c, 2.0);
    px.vline_aa(12.0, 6.0, 25.0, c, 2.0);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn draw_pause(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.fill_round_rect(8.0, 7.0, 13.0, 24.0, 1.2, c);
    px.fill_round_rect(18.0, 7.0, 23.0, 24.0, 1.2, c);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn draw_resume(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px_x = x as f32 + 0.5;
            let px_y = y as f32 + 0.5;
            let a = edge(9.0, 6.0, 9.0, 25.0, px_x, px_y);
            let b = edge(9.0, 25.0, 24.0, 15.5, px_x, px_y);
            let d = edge(24.0, 15.5, 9.0, 6.0, px_x, px_y);
            let inside = a >= 0.0 && b >= 0.0 && d >= 0.0;
            let dist = a.min(b).min(d);
            let alpha = if inside {
                1.0
            } else if dist > -1.2 {
                ((1.2 + dist) / 1.2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            if alpha > 0.01 {
                px.blend(x as i32, y as i32, c.0, c.1, c.2, alpha);
            }
        }
    }
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn edge(x0: f32, y0: f32, x1: f32, y1: f32, x: f32, y: f32) -> f32 {
    (x - x0) * (y1 - y0) - (y - y0) * (x1 - x0)
}

#[cfg(not(target_os = "macos"))]
fn draw_update(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let cx = 15.5;
    let cy = 15.5;
    px.stroke_circle_aa(cx, cy, 10.0, c, 2.0);
    px.fill_round_rect(14.5, 8.0, 16.5, 18.0, 0.8, c);
    px.fill_circle_aa(cx, 21.5, 1.6, c.0, c.1, c.2);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn draw_show(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_round_rect(5.0, 7.0, 26.0, 24.0, 2.0, c, 2.0);
    px.hline_aa(5.0, 12.0, 26.0, c, 2.0);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
fn draw_quit(c: (u8, u8, u8)) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.stroke_arc_aa(15.5, 16.5, 9.0, 40.0, 320.0, c, 2.2);
    px.fill_round_rect(14.5, 5.0, 16.5, 16.0, 0.8, c);
    px.into_icon()
}

#[cfg(not(target_os = "macos"))]
struct Canvas {
    rgba: Vec<u8>,
}

#[cfg(not(target_os = "macos"))]
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
                let alpha = if d <= 0.0 { 1.0 } else { coverage(d, 0.75) };
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

    fn hline_aa(&mut self, x0: f32, y: f32, x1: f32, c: (u8, u8, u8), thickness: f32) {
        for py in 0..SIZE as i32 {
            for px in 0..SIZE as i32 {
                let x = px as f32 + 0.5;
                let yy = py as f32 + 0.5;
                if !(x0 - 1.0..=x1 + 1.0).contains(&x) {
                    continue;
                }
                let alpha = coverage((yy - y).abs(), thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(px, py, c.0, c.1, c.2, alpha);
                }
            }
        }
    }

    fn vline_aa(&mut self, x: f32, y0: f32, y1: f32, c: (u8, u8, u8), thickness: f32) {
        for py in 0..SIZE as i32 {
            for px in 0..SIZE as i32 {
                let xx = px as f32 + 0.5;
                let y = py as f32 + 0.5;
                if !(y0 - 1.0..=y1 + 1.0).contains(&y) {
                    continue;
                }
                let alpha = coverage((xx - x).abs(), thickness / 2.0);
                if alpha > 0.01 {
                    self.blend(px, py, c.0, c.1, c.2, alpha);
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
}

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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
