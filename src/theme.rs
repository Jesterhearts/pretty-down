use serde::Deserialize;

/// A color value — either an ANSI 256 number or a hex RGB string.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Color {
    /// ANSI 256-color index (0-255)
    Ansi(u8),
    /// Hex RGB string like "#ff0000" or named color
    Hex(String),
}

impl Color {
    /// Convert to an ANSI escape sequence for foreground.
    pub fn to_fg(&self) -> String {
        match self {
            Color::Ansi(n) => format!("\x1b[38;5;{n}m"),
            Color::Hex(s) => {
                if let Some((r, g, b)) = parse_color(s) {
                    format!("\x1b[38;2;{r};{g};{b}m")
                } else {
                    String::new()
                }
            }
        }
    }

    /// Convert to an ANSI escape sequence for background.
    pub fn to_bg(&self) -> String {
        match self {
            Color::Ansi(n) => format!("\x1b[48;5;{n}m"),
            Color::Hex(s) => {
                if let Some((r, g, b)) = parse_color(s) {
                    format!("\x1b[48;2;{r};{g};{b}m")
                } else {
                    String::new()
                }
            }
        }
    }

    pub fn to_rgb(&self) -> Option<[u8; 3]> {
        match self {
            Color::Ansi(_) => None, // can't easily map to RGB
            Color::Hex(s) => parse_color(s).map(|(r, g, b)| [r, g, b]),
        }
    }
}

/// Style for a single element.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub strikethrough: bool,
    #[serde(default)]
    pub dim: bool,
}

impl Style {
    /// Convert to an ANSI escape sequence that applies all set attributes.
    pub fn to_ansi(&self) -> String {
        let mut s = String::new();
        if let Some(ref c) = self.fg {
            s.push_str(&c.to_fg());
        }
        if let Some(ref c) = self.bg {
            s.push_str(&c.to_bg());
        }
        if self.bold {
            s.push_str("\x1b[1m");
        }
        if self.dim {
            s.push_str("\x1b[2m");
        }
        if self.italic {
            s.push_str("\x1b[3m");
        }
        if self.underline {
            s.push_str("\x1b[4m");
        }
        if self.strikethrough {
            s.push_str("\x1b[9m");
        }
        s
    }
}

/// Complete theme for the markdown renderer.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4: Style,
    pub h5: Style,
    pub h6: Style,
    pub bold: Style,
    pub italic: Style,
    pub strikethrough: Style,
    pub code_inline: Style,
    pub code_block: Style,
    pub link: Style,
    pub blockquote: Style,
    pub horizontal_rule: Style,
    pub details_summary: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            h1: Style {
                fg: Some(Color::Hex("#ffffff".into())),
                bold: true,
                ..Default::default()
            },
            h2: Style {
                fg: Some(Color::Hex("#dcdcff".into())),
                bold: true,
                ..Default::default()
            },
            h3: Style {
                fg: Some(Color::Hex("#c8dcff".into())),
                bold: true,
                ..Default::default()
            },
            h4: Style {
                fg: Some(Color::Hex("#b4d2ff".into())),
                bold: true,
                ..Default::default()
            },
            h5: Style {
                fg: Some(Color::Hex("#aac8f0".into())),
                bold: true,
                ..Default::default()
            },
            h6: Style {
                fg: Some(Color::Hex("#a0bee6".into())),
                bold: true,
                ..Default::default()
            },
            bold: Style {
                bold: true,
                ..Default::default()
            },
            italic: Style {
                italic: true,
                ..Default::default()
            },
            strikethrough: Style {
                strikethrough: true,
                ..Default::default()
            },
            code_inline: Style {
                dim: true,
                ..Default::default()
            },
            code_block: Style {
                dim: true,
                ..Default::default()
            },
            link: Style {
                underline: true,
                ..Default::default()
            },
            blockquote: Style {
                dim: true,
                ..Default::default()
            },
            horizontal_rule: Style {
                dim: true,
                ..Default::default()
            },
            details_summary: Style {
                bold: true,
                ..Default::default()
            },
        }
    }
}

impl Theme {
    /// Load a theme from a JSON file.
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("cannot read theme: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("invalid theme JSON: {e}"))
    }

    /// Get the heading style and color for a given level (1-6).
    pub fn heading(
        &self,
        level: usize,
    ) -> &Style {
        match level {
            1 => &self.h1,
            2 => &self.h2,
            3 => &self.h3,
            4 => &self.h4,
            5 => &self.h5,
            _ => &self.h6,
        }
    }

    /// Get the heading RGB color for sixel rendering.
    pub fn heading_color(
        &self,
        level: usize,
    ) -> [u8; 3] {
        self.heading(level)
            .fg
            .as_ref()
            .and_then(|c| c.to_rgb())
            .unwrap_or([255, 255, 255])
    }
}

/// Parse a color string: "#rrggbb", "#rgb", "rgb(r,g,b)", or any CSS named
/// color.
pub fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    use palette::Srgb;
    use palette::named;

    let val = s.trim();

    if let Some(hex) = val.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
                Some((r * 17, g * 17, b * 17))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some((r, g, b))
            }
            _ => None,
        };
    }

    if let Some(inner) = val
        .strip_prefix("rgb(")
        .or_else(|| val.strip_prefix("RGB("))
        && let Some(inner) = inner.strip_suffix(')')
    {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            let r = parts[0].trim().parse().ok()?;
            let g = parts[1].trim().parse().ok()?;
            let b = parts[2].trim().parse().ok()?;
            return Some((r, g, b));
        }
    }

    // Named CSS colors via palette
    let srgb: Srgb<u8> = named::from_str(&val.to_ascii_lowercase())?.into_format();
    Some((srgb.red, srgb.green, srgb.blue))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_color ──────────────────────────────────────────────

    #[test]
    fn parse_color_hex6() {
        assert_eq!(parse_color("#ff0000"), Some((255, 0, 0)));
        assert_eq!(parse_color("#00ff00"), Some((0, 255, 0)));
        assert_eq!(parse_color("#0000ff"), Some((0, 0, 255)));
        assert_eq!(parse_color("#1a2b3c"), Some((0x1a, 0x2b, 0x3c)));
    }

    #[test]
    fn parse_color_hex3() {
        // #rgb expands each digit: r*17, g*17, b*17
        assert_eq!(parse_color("#f00"), Some((255, 0, 0)));
        assert_eq!(parse_color("#0f0"), Some((0, 255, 0)));
        assert_eq!(parse_color("#abc"), Some((0xaa, 0xbb, 0xcc)));
    }

    #[test]
    fn parse_color_rgb_function() {
        assert_eq!(parse_color("rgb(10, 20, 30)"), Some((10, 20, 30)));
        assert_eq!(parse_color("RGB(255,128,0)"), Some((255, 128, 0)));
    }

    #[test]
    fn parse_color_named() {
        assert_eq!(parse_color("red"), Some((255, 0, 0)));
        assert_eq!(parse_color("white"), Some((255, 255, 255)));
        assert_eq!(parse_color("black"), Some((0, 0, 0)));
    }

    #[test]
    fn parse_color_trims_whitespace() {
        assert_eq!(parse_color("  #ff0000  "), Some((255, 0, 0)));
    }

    #[test]
    fn parse_color_invalid() {
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_color(""), None);
    }

    // ── Color ────────────────────────────────────────────────────

    #[test]
    fn color_ansi_to_fg() {
        assert_eq!(Color::Ansi(196).to_fg(), "\x1b[38;5;196m");
    }

    #[test]
    fn color_ansi_to_bg() {
        assert_eq!(Color::Ansi(22).to_bg(), "\x1b[48;5;22m");
    }

    #[test]
    fn color_hex_to_fg() {
        assert_eq!(Color::Hex("#ff0000".into()).to_fg(), "\x1b[38;2;255;0;0m");
    }

    #[test]
    fn color_hex_to_bg() {
        assert_eq!(Color::Hex("#00ff00".into()).to_bg(), "\x1b[48;2;0;255;0m");
    }

    #[test]
    fn color_invalid_hex_returns_empty() {
        assert_eq!(Color::Hex("nope".into()).to_fg(), "");
        assert_eq!(Color::Hex("nope".into()).to_bg(), "");
    }

    #[test]
    fn color_to_rgb() {
        assert_eq!(Color::Hex("#ff8040".into()).to_rgb(), Some([255, 128, 64]));
        assert_eq!(Color::Ansi(1).to_rgb(), None);
    }

    // ── Style ────────────────────────────────────────────────────

    #[test]
    fn style_default_is_empty() {
        assert_eq!(Style::default().to_ansi(), "");
    }

    #[test]
    fn style_bold() {
        let s = Style {
            bold: true,
            ..Default::default()
        };
        assert_eq!(s.to_ansi(), "\x1b[1m");
    }

    #[test]
    fn style_combined() {
        let s = Style {
            fg: Some(Color::Hex("#ff0000".into())),
            bold: true,
            italic: true,
            ..Default::default()
        };
        let ansi = s.to_ansi();
        assert!(ansi.contains("\x1b[38;2;255;0;0m"));
        assert!(ansi.contains("\x1b[1m"));
        assert!(ansi.contains("\x1b[3m"));
    }

    // ── Theme ────────────────────────────────────────────────────

    #[test]
    fn theme_heading_levels() {
        let theme = Theme::default();
        // heading(1) through heading(5) should return distinct styles;
        // heading(6+) should all map to h6.
        assert!(theme.heading(1).bold);
        assert!(theme.heading(6).bold);
        // Level 99 should clamp to h6
        assert!(std::ptr::eq(theme.heading(7), theme.heading(6)));
    }

    #[test]
    fn theme_heading_color_defaults_white_for_h1() {
        let theme = Theme::default();
        assert_eq!(theme.heading_color(1), [255, 255, 255]);
    }

    #[test]
    fn theme_heading_color_fallback_when_no_fg() {
        let mut theme = Theme::default();
        theme.h1.fg = None;
        assert_eq!(theme.heading_color(1), [255, 255, 255]);
    }

    // ── Theme JSON deserialization ───────────────────────────────

    #[test]
    fn theme_from_json() {
        let json = r##"{
            "h1": { "fg": "#ff0000", "bold": true },
            "link": { "fg": 33, "underline": true }
        }"##;
        let theme: Theme = serde_json::from_str(json).unwrap();
        assert!(theme.h1.bold);
        assert!(matches!(theme.h1.fg, Some(Color::Hex(ref s)) if s == "#ff0000"));
        assert!(matches!(theme.link.fg, Some(Color::Ansi(33))));
        assert!(theme.link.underline);
    }
}
