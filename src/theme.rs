//! Visual identity: a quiet "network console".
//!
//! Near-black cool surfaces, hairline borders, monospace section labels, and a
//! single signal-teal accent that is only ever spent on *live* state (your own
//! identity, a speaking peer, an active mic, a focused input). Everything else
//! stays greyscale so the accent always means "something is happening".
//!
//! The public surface is [`apply`] (call once at startup) and the reusable
//! [`Container`] card widget.

use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Frame, InnerResponse, Margin, Rect,
    RichText, Sense, Stroke, TextStyle, Ui, Visuals,
};

// ---- tokens -----------------------------------------------------------------

/// App background (the space behind cards).
pub const BG: Color32 = Color32::from_rgb(0x0e, 0x11, 0x16);
/// Card / raised surface.
pub const SURFACE: Color32 = Color32::from_rgb(0x16, 0x1b, 0x22);
/// Hovered / faint surface.
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x1f, 0x26, 0x30);
/// Input wells (darker than the app background).
pub const WELL: Color32 = Color32::from_rgb(0x0a, 0x0d, 0x11);
/// Hairline borders.
pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x31, 0x3b);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xd6, 0xdb, 0xe1);
/// Secondary / label text.
pub const MUTED: Color32 = Color32::from_rgb(0x7a, 0x87, 0x94);
/// The one accent: signal teal — reserved for live state.
pub const ACCENT: Color32 = Color32::from_rgb(0x2d, 0xd4, 0xbf);
/// Ink used on top of the accent fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x06, 0x14, 0x12);
/// Other people's names (a calm, distinct steel tone).
pub const PEER_NAME: Color32 = Color32::from_rgb(0x8f, 0xa3, 0xb8);

// Interactive greys: button face, hover, and hover border. Kept greyscale so
// the teal accent always reads as "live", never as "hovered".
pub const BTN: Color32 = Color32::from_rgb(0x21, 0x28, 0x33);
const BTN_HOVER: Color32 = Color32::from_rgb(0x2c, 0x36, 0x44);
const BTN_ACTIVE: Color32 = Color32::from_rgb(0x34, 0x40, 0x50);
const STROKE_HOVER: Color32 = Color32::from_rgb(0x44, 0x53, 0x64);
const TEXT_BRIGHT: Color32 = Color32::from_rgb(0xee, 0xf2, 0xf6);

const RADIUS: u8 = 10;

/// Install the global style. Call once with the app's egui context.
pub fn apply(ctx: &Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = SURFACE_HOVER;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(12);

    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0x2d, 0xd4, 0xbf, 60);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    let cr = CornerRadius::same(8);
    // Non-interactive chrome (labels, separators, card frames).
    v.widgets.noninteractive.corner_radius = cr;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

    // Idle buttons / combo boxes / inputs: solid grey face, hairline border.
    v.widgets.inactive.corner_radius = cr;
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.expansion = 0.0;

    // Hover: lift the fill and brighten border + text (clear, greyscale feedback).
    v.widgets.hovered.corner_radius = cr;
    v.widgets.hovered.bg_fill = BTN_HOVER;
    v.widgets.hovered.weak_bg_fill = BTN_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, STROKE_HOVER);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);
    v.widgets.hovered.expansion = 1.0;

    // Pressed: slightly brighter still, accent hairline to confirm the action.
    v.widgets.active.corner_radius = cr;
    v.widgets.active.bg_fill = BTN_ACTIVE;
    v.widgets.active.weak_bg_fill = BTN_ACTIVE;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);
    v.widgets.active.expansion = 0.0;

    v.widgets.open.corner_radius = cr;
    v.widgets.open.bg_fill = BTN;
    v.widgets.open.weak_bg_fill = BTN;
    v.widgets.open.bg_stroke = Stroke::new(1.0, STROKE_HOVER);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);

    ctx.all_styles_mut(|style| {
        style.visuals = v.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 8.0);
        style.spacing.window_margin = Margin::same(0);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.interact_size.y = 30.0;
        style.spacing.combo_width = 0.0;

        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
    });
}

/// A monospace, letter-spaced section label — the console "eyebrow".
pub fn label_font() -> FontId {
    FontId::new(11.0, FontFamily::Monospace)
}

/// A small filled status dot, drawn (not a font glyph, which may be missing).
pub fn status_dot(ui: &mut Ui, color: Color32) {
    let d = 10.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(d, d), Sense::hover());
    ui.painter().circle_filled(rect.center(), d * 0.35, color);
}

/// A compact horizontal level meter (0..1), full available width.
///
/// Teal while nominal, amber when hot (>0.7), red near clip (>0.9) — so a
/// glance tells you whether audio is actually flowing.
pub fn vu_meter(ui: &mut Ui, level: f32) {
    let level = level.clamp(0.0, 1.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 6.0), Sense::hover());
    let painter = ui.painter();
    let radius = CornerRadius::same(2);
    painter.rect_filled(rect, radius, WELL);
    if level > 0.001 {
        let fill_w = (rect.width() * level).max(2.0);
        let fill = Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
        let color = if level > 0.9 {
            Color32::from_rgb(0xff, 0x6b, 0x6b)
        } else if level > 0.7 {
            Color32::from_rgb(0xf5, 0xc2, 0x11)
        } else {
            ACCENT
        };
        painter.rect_filled(fill, radius, color);
    }
}

/// Render a section eyebrow with a hairline rule beneath it.
pub fn eyebrow(ui: &mut Ui, text: &str) {
    // Fake tracking by inserting thin spaces between characters.
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    ui.label(RichText::new(spaced).font(label_font()).color(MUTED).strong());
    ui.add_space(6.0);
}

// ---- Container --------------------------------------------------------------

/// A card: raised surface, hairline border, rounded corners, comfortable
/// padding, and an optional console-style section label.
///
/// ```ignore
/// theme::Container::titled("Peers").show(ui, |ui| {
///     ui.label("…");
/// });
/// ```
#[derive(Default)]
pub struct Container<'a> {
    title: Option<&'a str>,
    accent: bool,
    padding: Option<i8>,
}

impl<'a> Container<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A card with a section eyebrow at the top.
    pub fn titled(title: &'a str) -> Self {
        Container { title: Some(title), ..Default::default() }
    }

    /// Draw the border in the accent colour to signal "live" / focused.
    pub fn accent(mut self, on: bool) -> Self {
        self.accent = on;
        self
    }

    /// Override the inner padding (default 14).
    pub fn padding(mut self, p: i8) -> Self {
        self.padding = Some(p);
        self
    }

    pub fn show<R>(
        self,
        ui: &mut Ui,
        add_contents: impl FnOnce(&mut Ui) -> R,
    ) -> InnerResponse<R> {
        let border = if self.accent { ACCENT } else { BORDER };
        let frame = Frame::new()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(Margin::same(self.padding.unwrap_or(14)));

        frame.show(ui, |ui| {
            if let Some(t) = self.title {
                eyebrow(ui, t);
            }
            add_contents(ui)
        })
    }
}
