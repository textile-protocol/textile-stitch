// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The Textile icons, embedded once. Two variants, both rasterized from
//! packages/public-docs textile-icon.svg:
//! - the bare transparent mark, drawn inline in the app header;
//! - the app icon (mark on a grey-gradient rounded tile) for the native
//!   window/dock/taskbar icon, matching the macOS/Linux app icon.

/// 256x256 RGBA PNG of the bare Textile mark (transparent background).
const MARK_PNG: &[u8] = include_bytes!("../../../assets/stitch-icon-256.png");

/// 256x256 RGBA PNG of the app icon (mark on a grey-gradient rounded tile).
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/stitch-app-256.png");

/// Decode an embedded PNG to `(rgba, width, height)`. Returns `None` if it can't
/// be decoded, so a bad asset degrades to "no icon" rather than panics.
fn decode(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Icon for the native window (title bar / taskbar / dock) — the app icon tile.
pub fn window_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = decode(APP_ICON_PNG)?;
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

/// Upload the bare mark as an egui texture for drawing inline in the app header.
pub fn texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let (rgba, w, h) = decode(MARK_PNG)?;
    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    Some(ctx.load_texture("textile-icon", image, egui::TextureOptions::LINEAR))
}
