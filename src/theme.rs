use gpui::{App, BoxShadow, Global, Hsla, hsla, point, px, rgb};

/// Theme colours, dark and light.
#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub background: Hsla,
    pub foreground: Hsla,
    pub card: Hsla,
    pub popover: Hsla,
    pub primary: Hsla,
    pub primary_fg: Hsla,
    pub secondary: Hsla,
    pub secondary_fg: Hsla,
    pub muted: Hsla,
    pub muted_fg: Hsla,
    pub accent: Hsla,
    pub destructive: Hsla,
    pub border: Hsla,
    pub input: Hsla,
    panel_alpha: f32,
}

fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            dark: true,
            background: hex(0x09090b),
            foreground: hex(0xfafafa),
            card: hex(0x18181b),
            popover: hex(0x18181b),
            primary: hex(0x51a2ff),
            primary_fg: hex(0x09090b),
            secondary: hex(0x27272a),
            secondary_fg: hex(0xfafafa),
            muted: hex(0x27272a),
            muted_fg: hex(0x9f9fa9),
            accent: hex(0x27272a),
            destructive: hex(0xff6467),
            border: hsla(0., 0., 1., 0.10),
            input: hsla(0., 0., 1., 0.15),
            panel_alpha: 0.48,
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            background: hex(0xffffff),
            foreground: hex(0x09090b),
            card: hex(0xffffff),
            popover: hex(0xffffff),
            primary: hex(0x155dfc),
            primary_fg: hex(0xfafafa),
            secondary: hex(0xf4f4f5),
            secondary_fg: hex(0x18181b),
            muted: hex(0xf4f4f5),
            muted_fg: hex(0x67676f),
            accent: hex(0xf4f4f5),
            destructive: hex(0xe7000b),
            border: hex(0xe4e4e7),
            input: hex(0xe4e4e7),
            panel_alpha: 0.08,
        }
    }

    /// `shadow-panel`: 0 4px 16px 0 black at 48% (dark) or 8% (light).
    pub fn panel_shadow(&self) -> Vec<BoxShadow> {
        vec![shadow(0., 4., 16., 0., self.panel_alpha)]
    }

    /// Ghost-button hover: `hover:bg-accent dark:hover:bg-accent/50`.
    pub fn ghost_hover(&self) -> Hsla {
        if self.dark { self.accent.opacity(0.5) } else { self.accent }
    }
}

fn shadow(x: f32, y: f32, blur: f32, spread: f32, alpha: f32) -> BoxShadow {
    BoxShadow { color: hsla(0., 0., 0., alpha), offset: point(px(x), px(y)), blur_radius: px(blur), spread_radius: px(spread) }
}

/// Tailwind v4 shadows.
pub fn shadow_xs() -> Vec<BoxShadow> {
    vec![shadow(0., 1., 2., 0., 0.05)]
}
pub fn shadow_md() -> Vec<BoxShadow> {
    vec![shadow(0., 4., 6., -1., 0.1), shadow(0., 2., 4., -2., 0.1)]
}
pub fn shadow_lg() -> Vec<BoxShadow> {
    vec![shadow(0., 10., 15., -3., 0.1), shadow(0., 4., 6., -4., 0.1)]
}

impl Global for Theme {}

pub fn theme(cx: &App) -> Theme {
    *cx.global::<Theme>()
}

/// Tailwind v4 palette entries used by app badges (computed from the web build).
pub const SKY: u32 = 0x00a5f5;
pub const CYAN: u32 = 0x00b9dc;
pub const INDIGO: u32 = 0x5f5fff;
pub const VIOLET: u32 = 0x8c50ff;
pub const AMBER: u32 = 0xff9b00;
pub const RED: u32 = 0xfa2d37;
pub const EMERALD: u32 = 0x00be7d;
pub fn color(v: u32) -> Hsla {
    hex(v)
}
