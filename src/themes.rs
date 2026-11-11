//! The app's colour themes: System, Light and Dark from the kit's neutral greys, and named
//! editor themes, each a static table of its published colours from which the kit, the
//! diff's tints and the syntax colours derive.

use std::cell::RefCell;
use std::sync::OnceLock;

use rune::Color;
use rune::color::srgb_decode;
use rune_kit::syntax::{Syntax, SyntaxTheme};
use rune_kit::{Kit, PaletteFocus, PaletteSeed, StateColors};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Theme {
    System,
    Light,
    Dark,
    TokyoNight,
    Paper,
    Catppuccin,
    Dracula,
    GithubDark,
    Gruvbox,
    Monokai,
    Nord,
    OneDark,
    RosePine,
    Solarized,
}

impl Theme {
    /// Every theme, in the order Settings shows them.
    pub const ALL: [Theme; 14] = [
        Theme::System,
        Theme::Light,
        Theme::Dark,
        Theme::TokyoNight,
        Theme::Paper,
        Theme::Catppuccin,
        Theme::Dracula,
        Theme::GithubDark,
        Theme::Gruvbox,
        Theme::Monokai,
        Theme::Nord,
        Theme::OneDark,
        Theme::RosePine,
        Theme::Solarized,
    ];

    /// The name in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::TokyoNight => "tokyo-night",
            Theme::Paper => "paper",
            Theme::Catppuccin => "catppuccin",
            Theme::Dracula => "dracula",
            Theme::GithubDark => "github-dark",
            Theme::Gruvbox => "gruvbox",
            Theme::Monokai => "monokai",
            Theme::Nord => "nord",
            Theme::OneDark => "one-dark",
            Theme::RosePine => "rose-pine",
            Theme::Solarized => "solarized",
        }
    }

    /// The theme a settings file names; `None` for a name this version doesn't know.
    pub fn from_id(id: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::TokyoNight => "Tokyo Night",
            Theme::Paper => "Paper",
            Theme::Catppuccin => "Catppuccin",
            Theme::Dracula => "Dracula",
            Theme::GithubDark => "GitHub Dark",
            Theme::Gruvbox => "Gruvbox",
            Theme::Monokai => "Monokai",
            Theme::Nord => "Nord",
            Theme::OneDark => "One Dark",
            Theme::RosePine => "Rosé Pine",
            Theme::Solarized => "Solarized",
        }
    }

    /// The named theme's colours; `None` for the kit's own System, Light and Dark.
    pub fn scheme(self) -> Option<&'static Scheme> {
        Some(match self {
            Theme::System | Theme::Light | Theme::Dark => return None,
            Theme::TokyoNight => &TOKYO_NIGHT,
            Theme::Paper => &PAPER,
            Theme::Catppuccin => &CATPPUCCIN,
            Theme::Dracula => &DRACULA,
            Theme::GithubDark => &GITHUB_DARK,
            Theme::Gruvbox => &GRUVBOX,
            Theme::Monokai => &MONOKAI,
            Theme::Nord => &NORD,
            Theme::OneDark => &ONE_DARK,
            Theme::RosePine => &ROSE_PINE,
            Theme::Solarized => &SOLARIZED,
        })
    }

    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
            _ => self.scheme().is_some_and(|s| s.dark),
        }
    }
}

/// A named theme's colours as published, in `0xrrggbb` sRGB. The kit derives its layers,
/// borders and quieter text from `bg`, `fg` and `accent`; the diff's added and removed lines,
/// its moved lines and search matches take the state colours; code takes the token colours.
#[derive(Debug, PartialEq)]
pub struct Scheme {
    pub dark: bool,
    bg: u32,
    fg: u32,
    accent: u32,
    added: u32,
    removed: u32,
    warning: u32,
    info: u32,
    keyword: u32,
    string: u32,
    number: u32,
    kind: u32,
    function: u32,
    property: u32,
    comment: u32,
}

impl Scheme {
    /// The colours a theme's preview shows: background, text, accent, added and removed.
    fn swatch(&self) -> [Color; 5] {
        [self.bg, self.fg, self.accent, self.added, self.removed].map(rgb)
    }

    fn kit(&self) -> Kit {
        let seed = PaletteSeed { bg: rgb(self.bg), fg: rgb(self.fg), accent: rgb(self.accent) };
        let mut k = Kit::from_palette_focus(seed, PaletteFocus::Accent);
        // The kit tints its layers and opaque borders with the more colourful of `bg` and `fg`,
        // so a cream text like Gruvbox's turns every panel amber. Step them from the theme's
        // own background, by the kit's amounts, so they keep its hue.
        let [l, a, b, _] = seed.bg.to_oklab();
        let (base, toward_fg) = if self.dark { (l.max(0.145), 1.0) } else { (l, -1.0) };
        let lift = |dl: f32| Color::from_oklab([(base + toward_fg * dl).clamp(0.0, 1.0), a, b, 1.0]);
        let [l1, l2, l3, l4] = if self.dark { [0.04, 0.07, 0.10, 0.13] } else { [0.02, 0.045, 0.065, 0.10] }.map(lift);
        let c = &mut k.c;
        (c.layer_01, c.layer_02, c.layer_03, c.layer_04) = (l1, l2, l3, l4);
        (c.bg_button_neutral, c.surface_weak, c.surface_raised, c.surface_raised_base) = (l1, l1, l1, l1);
        (c.surface_base, c.surface_disabled, c.surface_raised_hover) = (l2, l2, l3);
        c.std_border = lift(if self.dark { 0.20 } else { 0.18 });
        (c.border_weak, c.border_hover, c.border_active, c.border_disabled) = (lift(0.12), lift(0.26), lift(0.28), lift(0.10));
        // The kit tints its states from its own neutral hues; take the theme's instead, washed
        // over the surface as the kit does, so added lines are the theme's green.
        let surface = k.c.bg_base;
        let state = |hex: u32| {
            let hue = rgb(hex);
            StateColors { bg: wash(surface, hue, 0.12), fg: hue, border: wash(surface, hue, 0.35), icon: hue }
        };
        k.c.success = state(self.added);
        k.c.danger = state(self.removed);
        k.c.warning = state(self.warning);
        k.c.info = state(self.info);
        k
    }

    /// The theme's own token colours over the kit's defaults, which keep comments-as-faint
    /// and added and removed lines from the states.
    pub fn syntax(&self, k: &Kit) -> SyntaxTheme {
        [
            (Syntax::Keyword, self.keyword),
            (Syntax::String, self.string),
            (Syntax::Number, self.number),
            (Syntax::Type, self.kind),
            (Syntax::Function, self.function),
            (Syntax::Property, self.property),
            (Syntax::Comment, self.comment),
        ]
        .into_iter()
        .fold(SyntaxTheme::from_kit(k), |t, (kind, hex)| t.with(kind, rgb(hex)))
    }
}

/// `0xrrggbb` sRGB to the linear colour Rune paints.
fn rgb(hex: u32) -> Color {
    let channel = |shift: u32| srgb_decode(((hex >> shift) & 0xff) as f32 / 255.0);
    Color { r: channel(16), g: channel(8), b: channel(0), a: 1.0 }
}

/// `t` of the way from `base` toward `hue` in OKLab.
fn wash(base: Color, hue: Color, t: f32) -> Color {
    let [a, b] = [base.to_oklab(), hue.to_oklab()];
    Color::from_oklab([0, 1, 2, 3].map(|i| if i == 3 { a[3] } else { a[i] + (b[i] - a[i]) * t }))
}

thread_local! {
    /// The last kit built, and for which theme and appearance: every frame asks for one, and
    /// the theme rarely changes, so most frames clone it rather than derive it again.
    static KIT: RefCell<Option<(Theme, bool, Kit)>> = const { RefCell::new(None) };
}

/// The kit for `theme`, with `system_dark` the system's appearance for System.
pub fn kit(theme: Theme, system_dark: bool) -> Kit {
    let dark = theme.dark(system_dark);
    KIT.with_borrow_mut(|cache| {
        if let Some((t, d, k)) = cache
            && *t == theme
            && *d == dark
        {
            return k.clone();
        }
        let k = build(theme, dark);
        *cache = Some((theme, dark, k.clone()));
        k
    })
}

fn build(theme: Theme, dark: bool) -> Kit {
    if let Some(s) = theme.scheme() {
        return s.kit();
    }
    let mut k = Kit::neutral(dark);
    // Focus rings in slate, the hue of the app's greys, rather than the kit's blue accent;
    // light enough on dark to be seen, darker on light.
    let slate = rune::oklch(if dark { 0.68 } else { 0.55 }, 0.035, 262.0);
    k.c.border_focus = slate;
    k.c.border_active = slate;
    k
}

/// Each theme's preview colours (see [`Scheme::swatch`]), in [`Theme::ALL`] order; System's
/// are Light's, as its preview shows both. Worked out once, the first time Settings opens.
pub fn swatches() -> &'static [[Color; 5]; 14] {
    static SWATCHES: OnceLock<[[Color; 5]; 14]> = OnceLock::new();
    SWATCHES.get_or_init(|| {
        Theme::ALL.map(|t| match t.scheme() {
            Some(s) => s.swatch(),
            None => {
                let c = Kit::neutral(t == Theme::Dark).c;
                [c.bg_base, c.text_base, c.text_muted, c.success.icon, c.danger.icon]
            }
        })
    })
}

// The published palettes. Where a theme has variants, the one most people mean by its name:
// Tokyo Night's Night, Catppuccin's Mocha, Gruvbox's dark medium, Rosé Pine's main,
// Solarized's dark, and PaperColor's light for Paper.

static TOKYO_NIGHT: Scheme = Scheme {
    dark: true,
    bg: 0x1a1b26,
    fg: 0xc0caf5,
    accent: 0x7aa2f7,
    added: 0x9ece6a,
    removed: 0xf7768e,
    warning: 0xe0af68,
    info: 0x7dcfff,
    keyword: 0xbb9af7,
    string: 0x9ece6a,
    number: 0xff9e64,
    kind: 0x2ac3de,
    function: 0x7aa2f7,
    property: 0x73daca,
    comment: 0x565f89,
};

static PAPER: Scheme = Scheme {
    dark: false,
    bg: 0xeeeeee,
    fg: 0x444444,
    accent: 0x005faf,
    added: 0x008700,
    removed: 0xaf0000,
    warning: 0xd75f00,
    info: 0x0087af,
    keyword: 0x8700af,
    string: 0x5f8700,
    number: 0xd75f00,
    kind: 0x005faf,
    function: 0x0087af,
    property: 0xd70087,
    comment: 0x878787,
};

static CATPPUCCIN: Scheme = Scheme {
    dark: true,
    bg: 0x1e1e2e,
    fg: 0xcdd6f4,
    accent: 0xcba6f7,
    added: 0xa6e3a1,
    removed: 0xf38ba8,
    warning: 0xf9e2af,
    info: 0x89b4fa,
    keyword: 0xcba6f7,
    string: 0xa6e3a1,
    number: 0xfab387,
    kind: 0xf9e2af,
    function: 0x89b4fa,
    property: 0xb4befe,
    comment: 0x9399b2,
};

static DRACULA: Scheme = Scheme {
    dark: true,
    bg: 0x282a36,
    fg: 0xf8f8f2,
    accent: 0xbd93f9,
    added: 0x50fa7b,
    removed: 0xff5555,
    warning: 0xf1fa8c,
    info: 0x8be9fd,
    keyword: 0xff79c6,
    string: 0xf1fa8c,
    number: 0xbd93f9,
    kind: 0x8be9fd,
    function: 0x50fa7b,
    property: 0xffb86c,
    comment: 0x6272a4,
};

static GITHUB_DARK: Scheme = Scheme {
    dark: true,
    bg: 0x0d1117,
    fg: 0xe6edf3,
    accent: 0x4493f8,
    added: 0x3fb950,
    removed: 0xf85149,
    warning: 0xd29922,
    info: 0x58a6ff,
    keyword: 0xff7b72,
    string: 0xa5d6ff,
    number: 0x79c0ff,
    kind: 0xffa657,
    function: 0xd2a8ff,
    property: 0x79c0ff,
    comment: 0x8b949e,
};

static GRUVBOX: Scheme = Scheme {
    dark: true,
    bg: 0x282828,
    fg: 0xebdbb2,
    accent: 0xfe8019,
    added: 0xb8bb26,
    removed: 0xfb4934,
    warning: 0xfabd2f,
    info: 0x83a598,
    keyword: 0xfb4934,
    string: 0xb8bb26,
    number: 0xd3869b,
    kind: 0xfabd2f,
    function: 0x8ec07c,
    property: 0x83a598,
    comment: 0x928374,
};

static MONOKAI: Scheme = Scheme {
    dark: true,
    bg: 0x272822,
    fg: 0xf8f8f2,
    accent: 0x66d9ef,
    added: 0xa6e22e,
    removed: 0xf92672,
    warning: 0xe6db74,
    info: 0x66d9ef,
    keyword: 0xf92672,
    string: 0xe6db74,
    number: 0xae81ff,
    kind: 0x66d9ef,
    function: 0xa6e22e,
    property: 0xfd971f,
    comment: 0x75715e,
};

static NORD: Scheme = Scheme {
    dark: true,
    bg: 0x2e3440,
    fg: 0xd8dee9,
    accent: 0x88c0d0,
    added: 0xa3be8c,
    removed: 0xbf616a,
    warning: 0xebcb8b,
    info: 0x81a1c1,
    keyword: 0x81a1c1,
    string: 0xa3be8c,
    number: 0xb48ead,
    kind: 0x8fbcbb,
    function: 0x88c0d0,
    property: 0xd08770,
    comment: 0x616e88,
};

static ONE_DARK: Scheme = Scheme {
    dark: true,
    bg: 0x282c34,
    fg: 0xabb2bf,
    accent: 0x61afef,
    added: 0x98c379,
    removed: 0xe06c75,
    warning: 0xe5c07b,
    info: 0x61afef,
    keyword: 0xc678dd,
    string: 0x98c379,
    number: 0xd19a66,
    kind: 0xe5c07b,
    function: 0x61afef,
    property: 0xe06c75,
    comment: 0x7f848e,
};

static ROSE_PINE: Scheme = Scheme {
    dark: true,
    bg: 0x191724,
    fg: 0xe0def4,
    accent: 0xc4a7e7,
    added: 0x9ccfd8,
    removed: 0xeb6f92,
    warning: 0xf6c177,
    info: 0xc4a7e7,
    keyword: 0x31748f,
    string: 0xf6c177,
    number: 0xc4a7e7,
    kind: 0x9ccfd8,
    function: 0xebbcba,
    property: 0x9ccfd8,
    comment: 0x6e6a86,
};

static SOLARIZED: Scheme = Scheme {
    dark: true,
    bg: 0x002b36,
    fg: 0x839496,
    accent: 0x268bd2,
    added: 0x859900,
    removed: 0xdc322f,
    warning: 0xb58900,
    info: 0x268bd2,
    keyword: 0x859900,
    string: 0x2aa198,
    number: 0xd33682,
    kind: 0xb58900,
    function: 0x268bd2,
    property: 0x6c71c4,
    comment: 0x586e75,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_has_its_own_id_and_reads_back() {
        for t in Theme::ALL {
            assert_eq!(Theme::from_id(t.id()), Some(t));
        }
        assert_eq!(Theme::from_id("solarized-light"), None);
    }

    #[test]
    fn a_named_theme_paints_with_its_own_colours() {
        for t in Theme::ALL {
            let k = kit(t, true);
            assert_eq!(k.dark, t.dark(true), "{t:?}");
            let Some(s) = t.scheme() else { continue };
            assert_eq!(k.c.bg_base, rgb(s.bg), "{t:?}");
            assert_eq!(k.c.success.icon, rgb(s.added), "{t:?}");
            assert_eq!(s.syntax(&k).color(Syntax::Keyword), rgb(s.keyword), "{t:?}");
            // Text stays readable: the kit's contrast floors need a foreground that allows them.
            let (bg, fg) = (k.c.bg_base.luminance(), k.c.text_base.luminance());
            assert!((bg.max(fg) + 0.05) / (bg.min(fg) + 0.05) >= 4.5, "{t:?}");
        }
        assert!(!kit(Theme::System, false).dark);
        assert_eq!(rgb(0xffffff), Color::WHITE);
    }
}
