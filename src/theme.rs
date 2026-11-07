//! The colours of ARCE.AI in the terminal.
//!
//! The tokens are the ones of the app (light and dark sets, the same names), so the CLI and the app
//! look like one product. A terminal that cannot show 24-bit colour gets the nearest of the 256
//! colours; `NO_COLOR` (or the setting `theme = "none"`) turns every colour off.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// One set of design tokens.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg: Rgb,
    pub surface: Rgb,
    pub surface2: Rgb,
    pub surface3: Rgb,
    pub ink: Rgb,
    pub muted: Rgb,
    pub faint: Rgb,
    pub line: Rgb,
    pub accent: Rgb,
    pub accent2: Rgb,
    pub accent_soft: Rgb,
    pub green: Rgb,
    pub green_soft: Rgb,
    pub amber: Rgb,
    pub amber_soft: Rgb,
    pub red: Rgb,
    pub user_bubble: Rgb,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        bg: Rgb(0xF6, 0xF7, 0xFA),
        surface: Rgb(0xFF, 0xFF, 0xFF),
        surface2: Rgb(0xF1, 0xF3, 0xF7),
        surface3: Rgb(0xE9, 0xEC, 0xF2),
        ink: Rgb(0x1C, 0x20, 0x2B),
        muted: Rgb(0x71, 0x79, 0x88),
        faint: Rgb(0x9B, 0xA2, 0xAF),
        line: Rgb(0xE4, 0xE7, 0xED),
        accent: Rgb(0x5D, 0x67, 0xE8),
        accent2: Rgb(0x7A, 0x82, 0xF3),
        accent_soft: Rgb(0xEE, 0xF0, 0xFF),
        green: Rgb(0x2E, 0x9D, 0x76),
        green_soft: Rgb(0xE7, 0xF6, 0xF0),
        amber: Rgb(0xBD, 0x7A, 0x1D),
        amber_soft: Rgb(0xFF, 0xF4, 0xE4),
        red: Rgb(0xC0, 0x4B, 0x52),
        user_bubble: Rgb(0xE9, 0xEB, 0xFF),
    };

    pub const DARK: Palette = Palette {
        bg: Rgb(0x10, 0x11, 0x16),
        surface: Rgb(0x19, 0x1B, 0x22),
        surface2: Rgb(0x20, 0x23, 0x2C),
        surface3: Rgb(0x2B, 0x2F, 0x3A),
        ink: Rgb(0xF0, 0xF1, 0xF5),
        muted: Rgb(0xA2, 0xA7, 0xB3),
        faint: Rgb(0x73, 0x7A, 0x89),
        line: Rgb(0x30, 0x33, 0x3E),
        accent: Rgb(0x8C, 0x93, 0xFF),
        accent2: Rgb(0xA4, 0xA9, 0xFF),
        accent_soft: Rgb(0x29, 0x2B, 0x49),
        green: Rgb(0x67, 0xC8, 0xA0),
        green_soft: Rgb(0x20, 0x3A, 0x33),
        amber: Rgb(0xE2, 0xAD, 0x62),
        amber_soft: Rgb(0x3C, 0x30, 0x21),
        red: Rgb(0xE0, 0x7A, 0x80),
        user_bubble: Rgb(0x29, 0x2B, 0x49),
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    None,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub palette: Palette,
    pub depth: ColorDepth,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Sets the theme once at start; later calls are ignored.
pub fn init(choice: &str) {
    let _ = THEME.set(Theme::detect(choice));
}

pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| Theme::detect("auto"))
}

impl Theme {
    /// `auto` reads the terminal's background from `COLORFGBG` when it is set and takes dark otherwise
    /// (most terminals); `light`, `dark` force one; `none` turns colour off.
    pub fn detect(choice: &str) -> Theme {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let piped = !std::io::IsTerminal::is_terminal(&std::io::stdout());
        let depth = if no_color || piped || choice == "none" || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
            ColorDepth::None
        } else if std::env::var("COLORTERM").is_ok_and(|c| c.contains("truecolor") || c.contains("24bit")) || cfg!(windows) {
            ColorDepth::TrueColor
        } else {
            ColorDepth::Ansi256
        };
        let light = match choice {
            "light" => true,
            "dark" => false,
            _ => std::env::var("COLORFGBG")
                .ok()
                .and_then(|v| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok()))
                .is_some_and(|bg| bg == 7 || bg == 15),
        };
        Theme { palette: if light { Palette::LIGHT } else { Palette::DARK }, depth }
    }

    pub fn plain() -> Theme {
        Theme { palette: Palette::DARK, depth: ColorDepth::None }
    }

    fn fg_code(&self, c: Rgb) -> String {
        match self.depth {
            ColorDepth::None => String::new(),
            ColorDepth::TrueColor => format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2),
            ColorDepth::Ansi256 => format!("\x1b[38;5;{}m", to_ansi256(c)),
        }
    }

    fn bg_code(&self, c: Rgb) -> String {
        match self.depth {
            ColorDepth::None => String::new(),
            ColorDepth::TrueColor => format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2),
            ColorDepth::Ansi256 => format!("\x1b[48;5;{}m", to_ansi256(c)),
        }
    }

    pub fn paint(&self, text: &str, style: Style) -> String {
        if self.depth == ColorDepth::None || text.is_empty() {
            return text.to_string();
        }
        let p = &self.palette;
        let mut open = String::new();
        if let Some(fg) = style.fg {
            open.push_str(&self.fg_code(fg.of(p)));
        }
        if let Some(bg) = style.bg {
            open.push_str(&self.bg_code(bg.of(p)));
        }
        if style.bold {
            open.push_str("\x1b[1m");
        }
        if style.italic {
            open.push_str("\x1b[3m");
        }
        if style.underline {
            open.push_str("\x1b[4m");
        }
        if style.strike {
            open.push_str("\x1b[9m");
        }
        if style.dim {
            open.push_str("\x1b[2m");
        }
        if open.is_empty() {
            return text.to_string();
        }
        format!("{open}{text}\x1b[0m")
    }
}

/// A token of the palette, resolved when painting so one style works in both themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Ink,
    Muted,
    Faint,
    Line,
    Accent,
    Accent2,
    AccentSoft,
    Green,
    GreenSoft,
    Amber,
    AmberSoft,
    Red,
    Surface2,
    Surface3,
    UserBubble,
}

impl Token {
    fn of(self, p: &Palette) -> Rgb {
        match self {
            Token::Ink => p.ink,
            Token::Muted => p.muted,
            Token::Faint => p.faint,
            Token::Line => p.line,
            Token::Accent => p.accent,
            Token::Accent2 => p.accent2,
            Token::AccentSoft => p.accent_soft,
            Token::Green => p.green,
            Token::GreenSoft => p.green_soft,
            Token::Amber => p.amber,
            Token::AmberSoft => p.amber_soft,
            Token::Red => p.red,
            Token::Surface2 => p.surface2,
            Token::Surface3 => p.surface3,
            Token::UserBubble => p.user_bubble,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Token>,
    pub bg: Option<Token>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub dim: bool,
}

impl Style {
    pub const fn fg(t: Token) -> Style {
        Style { fg: Some(t), bg: None, bold: false, italic: false, underline: false, strike: false, dim: false }
    }
    pub const fn bold(mut self) -> Style {
        self.bold = true;
        self
    }
    pub const fn italic(mut self) -> Style {
        self.italic = true;
        self
    }
    pub const fn underline(mut self) -> Style {
        self.underline = true;
        self
    }
    pub const fn strike(mut self) -> Style {
        self.strike = true;
        self
    }
    pub const fn on(mut self, t: Token) -> Style {
        self.bg = Some(t);
        self
    }
}

/// Short helpers for the common styles.
pub fn paint(text: &str, style: Style) -> String {
    theme().paint(text, style)
}
pub fn accent(text: &str) -> String {
    paint(text, Style::fg(Token::Accent))
}
pub fn muted(text: &str) -> String {
    paint(text, Style::fg(Token::Muted))
}
pub fn faint(text: &str) -> String {
    paint(text, Style::fg(Token::Faint))
}
pub fn green(text: &str) -> String {
    paint(text, Style::fg(Token::Green))
}
pub fn amber(text: &str) -> String {
    paint(text, Style::fg(Token::Amber))
}
pub fn red(text: &str) -> String {
    paint(text, Style::fg(Token::Red))
}
pub fn bold(text: &str) -> String {
    paint(text, Style { bold: true, ..Style::default() })
}

/// The nearest colour of the xterm 256-colour cube or grey ramp.
pub fn to_ansi256(c: Rgb) -> u8 {
    let Rgb(r, g, b) = c;
    // grey ramp when the channels are close
    if r.abs_diff(g) < 10 && g.abs_diff(b) < 10 && r.abs_diff(b) < 10 {
        let avg = (r as u16 + g as u16 + b as u16) / 3;
        if avg < 8 {
            return 16;
        }
        if avg > 238 {
            return 231;
        }
        return (232 + (avg - 8) / 10).min(255) as u8;
    }
    let level = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40) as u8
        }
    };
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

/// Removes ANSI escape sequences (for width counting and logs).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The width of [text] on screen (East Asian wide characters count two), ignoring escapes.
pub fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip_ansi(text).as_str())
}

pub fn term_width() -> usize {
    let from_env = || std::env::var("COLUMNS").ok().and_then(|c| c.parse::<usize>().ok());
    match crossterm::terminal::size() {
        Ok((w, _)) if w >= 20 => (w as usize).min(400),
        _ => from_env().unwrap_or(80).clamp(20, 400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_theme_adds_no_escapes() {
        let t = Theme::plain();
        assert_eq!(t.paint("hi", Style::fg(Token::Accent).bold()), "hi");
    }

    #[test]
    fn truecolor_uses_the_app_accent() {
        let t = Theme { palette: Palette::LIGHT, depth: ColorDepth::TrueColor };
        assert_eq!(t.paint("x", Style::fg(Token::Accent)), "\x1b[38;2;93;103;232mx\x1b[0m");
    }

    #[test]
    fn width_counts_wide_characters_and_skips_escapes() {
        assert_eq!(display_width("\x1b[1m日本\x1b[0ma"), 5);
    }

    #[test]
    fn ansi256_maps_greys_and_colours() {
        assert_eq!(to_ansi256(Rgb(0, 0, 0)), 16);
        assert_eq!(to_ansi256(Rgb(255, 255, 255)), 231);
        assert_eq!(to_ansi256(Rgb(255, 0, 0)), 196);
    }
}
