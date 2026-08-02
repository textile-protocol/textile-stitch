// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Small monochrome menu icons for the tray menu (Docker-style line art).

use tray_icon::menu::Icon;

const SIZE: u32 = 16;

pub struct MenuIcons {
    pub dot_running: Icon,
    pub dot_stopped: Icon,
    pub open: Icon,
    pub pause: Icon,
    pub resume: Icon,
    pub update: Icon,
    pub show: Icon,
    pub quit: Icon,
}

impl MenuIcons {
    pub fn new() -> Self {
        Self {
            // Status dots keep color so running/stopped reads at a glance.
            dot_running: solid_circle(0x34, 0xc7, 0x59).expect("dot_running"),
            dot_stopped: solid_circle(0x8e, 0x8e, 0x93).expect("dot_stopped"),
            open: draw_open().expect("open"),
            pause: draw_pause().expect("pause"),
            resume: draw_resume().expect("resume"),
            update: draw_update().expect("update"),
            show: draw_show().expect("show"),
            quit: draw_quit().expect("quit"),
        }
    }
}

fn solid_circle(r: u8, g: u8, b: u8) -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    px.fill_circle(7.5, 7.5, 4.2, r, g, b, 255);
    px.into_icon()
}

fn draw_open() -> Result<Icon, tray_icon::menu::BadIcon> {
    // Window frame with a sidebar — "go to dashboard" vibe.
    let mut px = Canvas::new();
    let c = ink();
    px.stroke_rect(2, 3, 13, 12, c, 1);
    px.vline(6, 3, 12, c);
    px.into_icon()
}

fn draw_pause() -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let c = ink();
    px.fill_rect(4, 3, 6, 12, c);
    px.fill_rect(9, 3, 11, 12, c);
    px.into_icon()
}

fn draw_resume() -> Result<Icon, tray_icon::menu::BadIcon> {
    let mut px = Canvas::new();
    let c = ink();
    // Play triangle pointing right.
    for y in 3..13 {
        let t = (y - 3) as f32 / 9.0;
        let half = if t <= 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
        let width = (half * 7.0).round() as i32;
        for x in 5..(5 + width).max(5) {
            px.set(x, y, c.0, c.1, c.2, 255);
        }
    }
    px.into_icon()
}

fn draw_update() -> Result<Icon, tray_icon::menu::BadIcon> {
    // Circle with exclamation — matches Docker's "Download update" cue.
    let mut px = Canvas::new();
    let c = ink();
    px.stroke_circle(7.5, 7.5, 5.5, c, 1.3);
    px.fill_rect(7, 4, 8, 9, c);
    px.fill_rect(7, 11, 8, 12, c);
    px.into_icon()
}

fn draw_show() -> Result<Icon, tray_icon::menu::BadIcon> {
    // Simple window.
    let mut px = Canvas::new();
    let c = ink();
    px.stroke_rect(2, 3, 13, 12, c, 1);
    px.hline(2, 6, 13, c);
    px.into_icon()
}

fn draw_quit() -> Result<Icon, tray_icon::menu::BadIcon> {
    // Power symbol: arc + stem.
    let mut px = Canvas::new();
    let c = ink();
    px.stroke_arc(7.5, 8.0, 5.0, 40.0, 320.0, c, 1.4);
    px.fill_rect(7, 2, 8, 8, c);
    px.into_icon()
}

fn ink() -> (u8, u8, u8) {
    // Near-black so icons read on light macOS menus; still visible enough on dark.
    (0x1c, 0x1c, 0x1e)
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

    fn set(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            return;
        }
        let i = ((y as u32 * SIZE + x as u32) * 4) as usize;
        // Keep the more opaque pixel when strokes overlap.
        if a >= self.rgba[i + 3] {
            self.rgba[i] = r;
            self.rgba[i + 1] = g;
            self.rgba[i + 2] = b;
            self.rgba[i + 3] = a;
        }
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: (u8, u8, u8)) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.set(x, y, c.0, c.1, c.2, 255);
            }
        }
    }

    fn hline(&mut self, x0: i32, y: i32, x1: i32, c: (u8, u8, u8)) {
        for x in x0..=x1 {
            self.set(x, y, c.0, c.1, c.2, 255);
        }
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, c: (u8, u8, u8)) {
        for y in y0..=y1 {
            self.set(x, y, c.0, c.1, c.2, 255);
        }
    }

    fn stroke_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: (u8, u8, u8), _w: i32) {
        self.hline(x0, y0, x1, c);
        self.hline(x0, y1, x1, c);
        self.vline(x0, y0, y1, c);
        self.vline(x1, y0, y1, c);
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, r: u8, g: u8, b: u8, a: u8) {
        let r2 = radius * radius;
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                if dx * dx + dy * dy <= r2 {
                    self.set(x, y, r, g, b, a);
                }
            }
        }
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, c: (u8, u8, u8), thickness: f32) {
        let outer = (radius + thickness / 2.0).powi(2);
        let inner = (radius - thickness / 2.0).max(0.0).powi(2);
        for y in 0..SIZE as i32 {
            for x in 0..SIZE as i32 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d2 = dx * dx + dy * dy;
                if d2 <= outer && d2 >= inner {
                    self.set(x, y, c.0, c.1, c.2, 255);
                }
            }
        }
    }

    fn stroke_arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        start_deg: f32,
        end_deg: f32,
        c: (u8, u8, u8),
        thickness: f32,
    ) {
        let steps = 48;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let deg = start_deg + (end_deg - start_deg) * t;
            let rad = deg.to_radians();
            let x = (cx + radius * rad.cos()).round() as i32;
            let y = (cy + radius * rad.sin()).round() as i32;
            // Stamp a small disk for thickness.
            let half = (thickness / 2.0).ceil() as i32;
            for dy in -half..=half {
                for dx in -half..=half {
                    if dx * dx + dy * dy <= half * half {
                        self.set(x + dx, y + dy, c.0, c.1, c.2, 255);
                    }
                }
            }
        }
    }
}
