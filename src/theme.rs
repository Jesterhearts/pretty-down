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
                if let Some((r, g, b)) = parse_color_str(s) {
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
                if let Some((r, g, b)) = parse_color_str(s) {
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
            Color::Hex(s) => parse_color_str(s).map(|(r, g, b)| [r, g, b]),
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

/// Parse a color string: "#rrggbb", "#rgb", or named colors.
fn parse_color_str(s: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = s.strip_prefix('#') {
        match hex.len() {
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
        }
    } else {
        // Named colors
        match s.to_ascii_lowercase().as_str() {
            "white" => Some((255, 255, 255)),
            "black" => Some((0, 0, 0)),
            "red" => Some((255, 0, 0)),
            "green" => Some((0, 128, 0)),
            "blue" => Some((0, 0, 255)),
            "yellow" => Some((255, 255, 0)),
            "cyan" => Some((0, 255, 255)),
            "magenta" => Some((255, 0, 255)),
            "orange" => Some((255, 165, 0)),
            "purple" => Some((128, 0, 128)),
            "gray" | "grey" => Some((128, 128, 128)),
            _ => None,
        }
    }
}
