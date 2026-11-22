//! JetBrains "New UI"-like dark and light themes.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme, Visuals, vec2};

pub const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub editor_bg: Color32,
    pub panel_bg: Color32,
    pub sidebar_bg: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub accent: Color32,
    pub selection: Color32,
    pub hover: Color32,
    pub button_bg: Color32,
    pub input_bg: Color32,
    pub current_line: Color32,
    pub gutter_text: Color32,
    pub gutter_active: Color32,
    pub modified: Color32,
    pub added: Color32,
    pub deleted: Color32,
    pub untracked: Color32,
    pub conflict: Color32,
    pub diff_add_bg: Color32,
    pub diff_del_bg: Color32,
    pub hunk: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub success: Color32,
    pub link: Color32,
    pub card: Color32,
    pub user_bubble: Color32,
    pub find_match: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            editor_bg: hex(0x1E1F22),
            panel_bg: hex(0x2B2D30),
            sidebar_bg: hex(0x232427),
            border: hex(0x393B40),
            text: hex(0xDFE1E5),
            text_weak: hex(0x868A91),
            accent: hex(0x3574F0),
            selection: hex(0x2E436E),
            hover: hex(0x393B40),
            button_bg: hex(0x393B40),
            input_bg: hex(0x1E1F22),
            current_line: hex(0x26282E),
            gutter_text: hex(0x4B5059),
            gutter_active: hex(0xA1A3AB),
            modified: hex(0x56A8F5),
            added: hex(0x73BD79),
            deleted: hex(0x868A91),
            untracked: hex(0xD1675A),
            conflict: hex(0xF75464),
            diff_add_bg: hex(0x294436),
            diff_del_bg: hex(0x5A3036),
            hunk: hex(0x56A8F5),
            warn: hex(0xF2C55C),
            error: hex(0xF75464),
            success: hex(0x5FB865),
            link: hex(0x548AF7),
            card: hex(0x25272B),
            user_bubble: hex(0x2E3440),
            find_match: Color32::from_rgba_unmultiplied(50, 89, 61, 160),
        }
    } else {
        Palette {
            editor_bg: hex(0xFFFFFF),
            panel_bg: hex(0xF7F8FA),
            sidebar_bg: hex(0xEBECF0),
            border: hex(0xDFE1E5),
            text: hex(0x1E1F22),
            text_weak: hex(0x6C707E),
            accent: hex(0x3574F0),
            selection: hex(0xA6D2FF),
            hover: hex(0xDFE1E5),
            button_bg: hex(0xEBECF0),
            input_bg: hex(0xFFFFFF),
            current_line: hex(0xF5F8FE),
            gutter_text: hex(0xAEB3C2),
            gutter_active: hex(0x767A8A),
            modified: hex(0x2B6FD1),
            added: hex(0x3D8F58),
            deleted: hex(0x8C8C8C),
            untracked: hex(0xC7222D),
            conflict: hex(0xD62D42),
            diff_add_bg: hex(0xD5F1DC),
            diff_del_bg: hex(0xF9D7DA),
            hunk: hex(0x3574F0),
            warn: hex(0xB58300),
            error: hex(0xD62D42),
            success: hex(0x3D8F58),
            link: hex(0x2E55A3),
            card: hex(0xFFFFFF),
            user_bubble: hex(0xEDF3FF),
            find_match: Color32::from_rgba_unmultiplied(130, 230, 150, 140),
        }
    }
}

pub fn apply(ctx: &egui::Context, dark: bool, ui_size: f32) {
    let p = palette(dark);
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, FontId::new(ui_size - 2.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(ui_size, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(ui_size, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(ui_size + 3.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(ui_size, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = vec2(6.0, 4.0);
        s.spacing.button_padding = vec2(7.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.interaction.tooltip_delay = 0.35;
    });

    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.panel_bg;
    v.window_fill = p.panel_bg;
    v.extreme_bg_color = p.input_bg;
    v.faint_bg_color = p.hover;
    v.code_bg_color = if dark { hex(0x1E1F22) } else { hex(0xF0F1F4) };
    // inline `code` in Claude's markdown, like the VS Code extension: tinted text on a soft chip
    let (code_fg, code_bg) = if dark {
        (hex(0xE5A27F), Color32::from_white_alpha(22))
    } else {
        (hex(0xB4532A), Color32::from_black_alpha(16))
    };
    egui_commonmark_backend::set_inline_code_style(ctx, code_fg, code_bg);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.hyperlink_color = p.link;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel_bg;
    w.noninteractive.weak_bg_fill = p.panel_bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.bg_fill = p.button_bg;
    w.inactive.weak_bg_fill = p.button_bg;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.active.bg_fill = p.selection;
    w.active.weak_bg_fill = p.selection;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        wv.corner_radius = CornerRadius::same(5);
        wv.expansion = 0.0;
    }

    let theme = if dark { Theme::Dark } else { Theme::Light };
    ctx.set_visuals_of(theme, v);
    ctx.set_theme(theme);
}

/// Distinct lane colors for the git graph.
pub const LANES: [Color32; 8] = [
    hex(0x3574F0),
    hex(0x5FB865),
    hex(0xE5A14B),
    hex(0xC77DBB),
    hex(0x2AACB8),
    hex(0xD1675A),
    hex(0xB3AE60),
    hex(0x8E8CD8),
];

/// Flat JetBrains-style toolbar icon button (no frame, hover background).
pub fn tool_btn(ui: &mut egui::Ui, pal: &Palette, glyph: &str, tip: &str, enabled: bool) -> bool {
    let font = FontId::proportional(14.0);
    let g = ui.painter().layout_no_wrap(glyph.to_owned(), font, pal.text);
    let size = vec2((g.size().x + 10.0).max(24.0), 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let resp = resp.on_hover_text(tip);
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, pal.hover);
    }
    let col = if enabled { pal.text } else { pal.text_weak.gamma_multiply(0.6) };
    ui.painter().galley_with_override_text_color(glyph_pos(&g, rect.center()), g, col);
    enabled && resp.clicked()
}

/// Paints a single icon glyph visually centered on `center`.
pub fn paint_glyph(p: &egui::Painter, center: egui::Pos2, glyph: &str, font: FontId, color: Color32) {
    let g = p.layout_no_wrap(glyph.to_owned(), font, color);
    p.galley(glyph_pos(&g, center), g, color);
}

/// Where to put `g` so its ink (not its line box) is centered on `center`: symbol glyphs
/// like × ✚ sit well below the middle of the line, so plain box-centering draws them low.
fn glyph_pos(g: &egui::Galley, center: egui::Pos2) -> egui::Pos2 {
    let ink = if g.mesh_bounds.is_positive() { g.mesh_bounds } else { g.rect };
    (center - ink.center().to_vec2()).round()
}

/// Painted checkbox. `state`: Some(true) checked, Some(false) empty, None partial.
pub fn paint_check(p: &egui::Painter, r: egui::Rect, state: Option<bool>, pal: &Palette) {
    if state == Some(false) {
        p.rect_stroke(r, 3.0, Stroke::new(1.0, pal.text_weak), egui::StrokeKind::Inside);
        return;
    }
    p.rect_filled(r, 3.0, pal.accent);
    let w = Stroke::new(1.8, Color32::WHITE);
    match state {
        Some(true) => {
            let pts = vec![
                r.left_top() + vec2(r.width() * 0.22, r.height() * 0.52),
                r.left_top() + vec2(r.width() * 0.42, r.height() * 0.72),
                r.left_top() + vec2(r.width() * 0.78, r.height() * 0.30),
            ];
            p.add(egui::Shape::line(pts, w));
        }
        _ => {
            p.line_segment([r.left_center() + vec2(3.0, 0.0), r.right_center() - vec2(3.0, 0.0)], w);
        }
    }
}
