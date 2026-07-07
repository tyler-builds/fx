//! Centralized visual tokens, in the spirit of conduit's design system:
//! UI code references these roles rather than raw literals, so the whole app
//! shares one look and a retune happens in one place.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

// --- Spacing (4px base) ---
pub const SM: f32 = 4.0;
pub const MD: f32 = 8.0;
pub const LG: f32 = 12.0;

// --- Corner radii ---
pub const RADIUS_SM: f32 = 5.0;
pub const RADIUS_MD: f32 = 8.0;

// --- Surfaces ---
// Deliberate elevation: the chrome (toolbar/sidebar/tabs) sits darkest, and
// the file list is lifted a step above it so content reads as a raised page,
// the way the reference separates its rail from its canvas.
pub const SURFACE_CHROME: Color32 = Color32::from_rgb(24, 25, 27); // toolbar / sidebar / tabs
pub const SURFACE_LIST: Color32 = Color32::from_rgb(31, 32, 34); // file list / content
pub const SURFACE_FAINT: Color32 = Color32::from_rgb(38, 40, 46); // headers, cards
pub const SURFACE_INPUT: Color32 = Color32::from_rgb(43, 45, 52); // text edits, buttons
pub const HOVER: Color32 = Color32::from_rgb(52, 55, 63);
/// True hairline — low-contrast so structure is felt, not drawn.
pub const BORDER: Color32 = Color32::from_rgb(42, 44, 51);

// Back-compat alias: some panels still reference the old chrome name.
pub const SURFACE_PANEL: Color32 = SURFACE_CHROME;

// --- Text ---
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(232, 233, 237);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(160, 164, 172);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(120, 124, 132);
/// Section eyebrows in the sidebar (quiet, so the nav items lead).
pub const SECTION: Color32 = Color32::from_rgb(128, 132, 140);

// --- Accent (one job: selection / focus) ---
pub const ACCENT: Color32 = Color32::from_rgb(88, 150, 246);
/// Opaque muted-accent fill for a selected row (readable with primary text).
pub const SELECTION_FILL: Color32 = Color32::from_rgb(42, 76, 132);
/// Subtle folder-name tint — a hair cooler than primary, not the old neon.
pub const FOLDER_TINT: Color32 = Color32::from_rgb(150, 190, 245);
/// Row hover wash.
pub const ROW_HOVER: Color32 = Color32::from_rgb(41, 43, 50);
/// Storage-meter track and fill.
pub const METER_TRACK: Color32 = Color32::from_rgb(48, 50, 58);
/// Inactive tab fill — recessed a step below the content/active tab so the
/// tab strip reads as one set, not floating chips.
pub const TAB_INACTIVE: Color32 = Color32::from_rgb(27, 28, 31);

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
