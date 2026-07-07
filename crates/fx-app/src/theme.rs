//! Centralized visual tokens, in the spirit of conduit's design system:
//! UI code references these roles rather than raw literals, so the whole app
//! shares one look and a retune happens in one place.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

// --- Spacing (4px base) ---
pub const SM: f32 = 4.0;
pub const MD: f32 = 8.0;

// --- Corner radii ---
pub const RADIUS_SM: f32 = 4.0;

// --- Surfaces (near-black, faintly cool) ---
pub const SURFACE_LIST: Color32 = Color32::from_rgb(24, 25, 28); // file list background
pub const SURFACE_PANEL: Color32 = Color32::from_rgb(30, 31, 35); // toolbar / sidebar
pub const SURFACE_FAINT: Color32 = Color32::from_rgb(37, 38, 43); // headers, faint fills
pub const SURFACE_INPUT: Color32 = Color32::from_rgb(44, 45, 51); // text edits, buttons
pub const HOVER: Color32 = Color32::from_rgb(52, 54, 61);
pub const BORDER: Color32 = Color32::from_rgb(50, 52, 59);

// --- Text ---
pub const TEXT_PRIMARY: Color32 = Color32::from_gray(224);
pub const TEXT_SECONDARY: Color32 = Color32::from_gray(158);
pub const TEXT_MUTED: Color32 = Color32::from_gray(120);

// --- Accent (one job: selection / focus) ---
pub const ACCENT: Color32 = Color32::from_rgb(74, 140, 240);
/// Opaque muted-accent fill for a selected row (readable with primary text).
pub const SELECTION_FILL: Color32 = Color32::from_rgb(44, 78, 134);
/// Subtle folder-name tint — a hair cooler than primary, not the old neon.
pub const FOLDER_TINT: Color32 = Color32::from_rgb(150, 190, 245);
/// Row hover wash.
pub const ROW_HOVER: Color32 = Color32::from_rgb(40, 42, 48);

/// Install global visuals + type ramp. Call once at startup, after fonts.
pub fn apply(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, TextStyle};
    let mut style = (*ctx.global_style()).clone();

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(11.5, FontFamily::Monospace),
        ),
        (
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        ),
    ]
    .into();

    let r = CornerRadius::same(RADIUS_SM as u8);
    let mut v = egui::Visuals::dark();
    v.panel_fill = SURFACE_PANEL;
    v.window_fill = SURFACE_PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.faint_bg_color = SURFACE_FAINT;
    v.extreme_bg_color = SURFACE_INPUT;
    v.code_bg_color = SURFACE_FAINT;
    v.selection.bg_fill = SELECTION_FILL;
    v.selection.stroke = Stroke::new(1.0, TEXT_PRIMARY);
    v.hyperlink_color = ACCENT;

    v.widgets.noninteractive.bg_fill = SURFACE_PANEL;
    v.widgets.noninteractive.weak_bg_fill = SURFACE_PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    v.widgets.noninteractive.corner_radius = r;

    v.widgets.inactive.bg_fill = SURFACE_INPUT;
    v.widgets.inactive.weak_bg_fill = SURFACE_INPUT;
    // No border on interactive widgets. egui's button reserves margin for
    // the border only in its framed (hover/selected) render, not its
    // inactive one, so a nonzero stroke makes widgets physically widen on
    // hover and shove their siblings. Fill-only states avoid that and read
    // cleaner. (Panels/separators keep their border via `noninteractive`.)
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    v.widgets.inactive.corner_radius = r;

    for st in [&mut v.widgets.hovered, &mut v.widgets.active] {
        st.bg_fill = HOVER;
        st.weak_bg_fill = HOVER;
        st.bg_stroke = Stroke::NONE;
        st.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
        st.corner_radius = r;
    }

    // Zero expansion on EVERY state: egui grows hovered/active widgets by a
    // pixel or two by default, which shifts everything laid out after them
    // (the layout jitter conduit hit). A uniform 0 keeps geometry stable.
    for st in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        st.expansion = 0.0;
    }

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(MD, SM + 1.0);
    style.spacing.button_padding = egui::vec2(SM + 2.0, SM);
    ctx.set_global_style(style);
}
