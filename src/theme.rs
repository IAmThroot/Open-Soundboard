use egui::{Color32, CornerRadius, Stroke, Vec2};

pub const BG: Color32 = Color32::from_rgb(0x15, 0x17, 0x1c);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x26);
pub const BG_SOFT: Color32 = Color32::from_rgb(0x24, 0x28, 0x31);
pub const BORDER: Color32 = Color32::from_rgb(0x32, 0x37, 0x42);

pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe9, 0xef);
pub const MUTED: Color32 = Color32::from_rgb(0x96, 0x9e, 0xb0);

pub const ACCENT: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x1e, 0x2c, 0x46);
pub const GOOD: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
pub const GOOD_SOFT: Color32 = Color32::from_rgb(0x1c, 0x46, 0x2c);
pub const GOOD_TINT: Color32 = Color32::from_rgb(0x1a, 0x2a, 0x22);
pub const DANGER: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
pub const WARN: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);

pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.faint_bg_color = BG_SOFT;
    visuals.extreme_bg_color = Color32::from_rgb(0x0f, 0x11, 0x15);

    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
        &mut visuals.widgets.noninteractive,
    ] {
        widget.corner_radius = CornerRadius::same(8);
    }
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.weak_bg_fill = BG_SOFT;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2d, 0x33, 0x3e);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.active.weak_bg_fill = ACCENT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);

    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.spacing.interact_size.y = 26.0;
    });
}
