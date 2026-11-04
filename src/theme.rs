use std::sync::OnceLock;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, SharedString, px, rgb};

/// The product palette for the studio-console surface.
///
/// Raw colours live here and nowhere else. Every call site reads a named role
/// from palette(), while standard controls resolve the same values through the
/// component theme.
pub struct Palette {
    pub paper: Hsla,
    pub paper_hi: Hsla,
    pub paper_mid: Hsla,
    pub paper_lo: Hsla,
    pub line: Hsla,
    pub line_strong: Hsla,

    pub ink: Hsla,
    pub ink_muted: Hsla,

    pub console: Hsla,
    pub console_hi: Hsla,
    pub console_line: Hsla,
    pub cream: Hsla,
    pub cream_muted: Hsla,

    pub blue: Hsla,
    pub blue_hover: Hsla,
    pub blue_active: Hsla,
    pub blue_soft: Hsla,

    pub signal: Hsla,
    pub signal_soft: Hsla,
    pub amber: Hsla,
    pub green: Hsla,

    pub tile: Hsla,
    pub tile_hover: Hsla,
    pub tile_active: Hsla,
    pub meter_face: Hsla,
}

fn h(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(|| Palette {
        // Three dark tiers: recessed hardware chrome, the page canvas it
        // frames, and the pads that sit on top of that canvas.
        paper: h(0x15170F),
        paper_hi: h(0x1C1F16),
        paper_mid: h(0x24281E),
        paper_lo: h(0x2D3126),
        line: h(0x2B2F23),
        line_strong: h(0x3E4335),

        ink: h(0xF3EBDD),
        ink_muted: h(0xA39E8C),

        console: h(0x0E100A),
        console_hi: h(0x1A1D13),
        console_line: h(0x2C3024),
        cream: h(0xF3EBDD),
        cream_muted: h(0xAAA494),

        blue: h(0x4C6CF2),
        blue_hover: h(0x6A86FF),
        blue_active: h(0x3A56D6),
        blue_soft: h(0x232C5C),

        signal: h(0xDE5638),
        signal_soft: h(0x3A1D17),
        amber: h(0xE0A63A),
        green: h(0x7E9F84),

        tile: h(0x21251A),
        tile_hover: h(0x292E21),
        tile_active: h(0x323728),
        meter_face: h(0x0A0B07),
    })
}

pub const DISPLAY_FONT: &str = "Bahnschrift";
pub const BODY_FONT: &str = "Segoe UI";
pub const MONO_FONT: &str = "Cascadia Mono";

/// Apply the soundboard palette and type system to GPUI Component.
///
/// Theme::update is the write path that keeps the component colour copy,
/// renderable tokens and Base projection in step.
pub fn install(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let p = palette();
    Theme::update(cx, |theme| {
        theme.font_family = SharedString::from(BODY_FONT);
        theme.mono_font_family = SharedString::from(MONO_FONT);
        theme.font_size = px(15.);
        theme.mono_font_size = px(12.);
        theme.radius = px(8.);
        theme.radius_lg = px(14.);
        theme.shadow = false;

        theme.background = p.paper;
        theme.foreground = p.ink;
        theme.border = p.line;
        theme.input = p.line_strong;
        theme.primary = p.blue;
        theme.primary_foreground = p.cream;
        theme.primary_hover = p.blue_hover;
        theme.primary_active = p.blue_active;
        theme.button = p.paper_hi;
        theme.button_hover = p.paper_mid;
        theme.button_active = p.paper_lo;
        theme.button_foreground = p.ink;
        theme.button_primary = p.blue;
        theme.button_primary_hover = p.blue_hover;
        theme.button_primary_active = p.blue_active;
        theme.button_primary_foreground = p.cream;
        theme.button_secondary = p.paper_mid;
        theme.button_secondary_hover = p.paper_lo;
        theme.button_secondary_active = p.line_strong;
        theme.button_secondary_foreground = p.ink;
        theme.secondary = p.paper_mid;
        theme.secondary_foreground = p.ink;
        theme.secondary_hover = p.paper_lo;
        theme.secondary_active = p.line_strong;
        theme.muted = p.paper_lo;
        theme.muted_foreground = p.ink_muted;
        theme.accent = p.paper_lo;
        theme.accent_foreground = p.ink;
        theme.popover = p.paper_mid;
        theme.popover_foreground = p.ink;
        theme.danger = p.signal;
        theme.danger_foreground = p.cream;
        theme.danger_hover = p.signal;
        theme.danger_active = p.signal;
        theme.success = p.green;
        theme.success_foreground = p.console;
        theme.warning = p.amber;
        theme.warning_foreground = p.console;
        theme.info = p.blue;
        theme.info_foreground = p.cream;
        theme.ring = p.blue;
        theme.selection = p.blue_soft;
        theme.caret = p.cream;
        theme.scrollbar = p.paper;
        theme.scrollbar_thumb = p.line_strong;
        theme.scrollbar_thumb_hover = p.cream_muted;

        theme.title_bar = p.console;
        theme.title_bar_border = p.console_line;
        theme.sidebar = p.console;
        theme.sidebar_foreground = p.cream;
        theme.sidebar_border = p.console_line;
        theme.sidebar_accent = p.console_hi;
        theme.sidebar_accent_foreground = p.cream;
        theme.sidebar_primary = p.blue;
        theme.sidebar_primary_foreground = p.cream;

        theme.list_even = p.paper_hi;
        theme.list_hover = p.paper_mid;
        theme.list_active = p.blue_soft;
        theme.list_active_border = p.blue;
        theme.list_head = p.console_hi;
        theme.group_box = p.paper_hi;
        theme.group_box_foreground = p.ink;
        theme.slider_bar = p.blue;
        theme.slider_thumb = p.cream;
        theme.switch = p.line_strong;
        theme.switch_thumb = p.cream;
        theme.progress_bar = p.blue;
        theme.skeleton = p.paper_mid;
        theme.drop_target = p.blue_soft;
    });
}
