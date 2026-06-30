// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The Textile app icon, embedded once and reused for the native window icon and
//! the in-app header. Rasterized from packages/public-docs textile-icon.svg.

/// 256x256 RGBA PNG of the Textile mark.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/stitch-icon-256.png");

/// Decode the embedded icon to `(rgba, width, height)`. Returns `None` if the
/// PNG can't be decoded, so a bad asset degrades to "no icon" rather than panics.
fn rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Icon for the native window (title bar / taskbar / dock).
pub fn window_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = rgba()?;
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

/// Upload the icon as an egui texture for drawing in the app header.
pub fn texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let (rgba, w, h) = rgba()?;
    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    Some(ctx.load_texture("textile-icon", image, egui::TextureOptions::LINEAR))
}
