//! Font setup, copied from the conduit engine's approach: Inter as the
//! primary text font, Phosphor as a *separate* icon family.
//!
//! Icons must render in [`ICON_FONT`]/[`ICON_FONT_BOLD`], never the text
//! family: Inter defines Private-Use-Area glyphs that collide with
//! Phosphor's codepoints, so a shared family would shadow one with the
//! other. Keeping them separate means a compiling icon constant always has
//! a glyph — no tofu boxes.

use eframe::egui;
use std::sync::Arc;

pub const ICON_FONT: &str = "phosphor-icons";
pub const ICON_FONT_BOLD: &str = "phosphor-bold";

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/Inter.ttf"
        ))),
    );
    fonts.font_data.insert(
        "phosphor".to_owned(),
        Arc::new(egui_phosphor::Variant::Fill.font_data()),
    );
    fonts.font_data.insert(
        "phosphor-bold".to_owned(),
        Arc::new(egui_phosphor::Variant::Bold.font_data()),
    );
    // Inter is the primary text font; Phosphor is kept out of the text
    // families (see module docs).
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "Inter".to_owned());
    }
    fonts.families.insert(
        egui::FontFamily::Name(ICON_FONT.into()),
        vec!["phosphor".to_owned(), "Inter".to_owned()],
    );
    fonts.families.insert(
        egui::FontFamily::Name(ICON_FONT_BOLD.into()),
        vec!["phosphor-bold".to_owned(), "Inter".to_owned()],
    );
    ctx.set_fonts(fonts);
}

/// A Phosphor glyph as `RichText` in the icon family, preserving text size.
pub fn icon(glyph: &str) -> egui::RichText {
    egui::RichText::new(glyph).family(egui::FontFamily::Name(ICON_FONT.into()))
}

/// A `FontId` in the icon family, for painting glyphs directly via
/// `Painter::text` (where a `RichText` widget isn't used — e.g. sort arrows
/// in the column header).
pub fn icon_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(ICON_FONT.into()))
}

/// Like [`icon_font`] but the bold (non-filled) weight — for painting `×`.
pub fn icon_font_bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(ICON_FONT_BOLD.into()))
}
