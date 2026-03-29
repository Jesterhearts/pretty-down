use std::path::Path;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::font::Font;
use crate::sixel;

/// ANSI escape helpers
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const ITALIC: &str = "\x1b[3m";
    pub const UNDERLINE: &str = "\x1b[4m";
    pub const STRIKETHROUGH: &str = "\x1b[9m";

    /// OSC 8 hyperlink start
    pub fn link_start(url: &str) -> String {
        format!("\x1b]8;;{url}\x1b\\")
    }

    /// OSC 8 hyperlink end
    pub fn link_end() -> String {
        "\x1b]8;;\x1b\\".to_string()
    }
}

/// Heading font sizes in pixels for h1..h6
const HEADING_SIZES: [u32; 6] = [48, 40, 32, 28, 24, 20];

/// Heading colors [r, g, b] for h1..h6
const HEADING_COLORS: [[u8; 3]; 6] = [
    [255, 255, 255],
    [220, 220, 255],
    [200, 220, 255],
    [180, 210, 255],
    [170, 200, 240],
    [160, 190, 230],
];

struct RenderState {
    /// Text accumulated for the current heading (rendered as sixel)
    heading_text: String,
    /// Current heading level (0 = not in heading)
    heading_level: usize,
    /// Whether we are inside bold
    bold: bool,
    /// Whether we are inside italic
    italic: bool,
    /// Whether we are inside strikethrough
    strikethrough: bool,
    /// Current link URL (if inside a link)
    link_url: Option<String>,
    /// Whether we are inside a code block
    in_code_block: bool,
    /// List nesting with item index (None = unordered, Some(n) = ordered starting at n)
    list_stack: Vec<Option<u64>>,
    /// Whether we just started a list item (for prefix)
    item_index: Vec<usize>,
    /// Base path for resolving relative image paths
    base_path: Option<std::path::PathBuf>,
}

impl RenderState {
    fn new(base_path: Option<std::path::PathBuf>) -> Self {
        Self {
            heading_text: String::new(),
            heading_level: 0,
            bold: false,
            italic: false,
            strikethrough: false,
            link_url: None,
            in_code_block: false,
            list_stack: Vec::new(),
            item_index: Vec::new(),
            base_path,
        }
    }

    fn push_style(&self, out: &mut String) {
        if self.bold {
            out.push_str(ansi::BOLD);
        }
        if self.italic {
            out.push_str(ansi::ITALIC);
        }
        if self.strikethrough {
            out.push_str(ansi::STRIKETHROUGH);
        }
    }

    fn list_indent(&self) -> String {
        "  ".repeat(self.list_stack.len().saturating_sub(1))
    }
}

/// Render markdown to a string containing ANSI escape codes and sixel sequences.
///
/// Headings are rendered as sixel images using the provided font.
/// Body text uses ANSI terminal styling.
/// Links use OSC 8 hyperlinks.
/// Images are rendered inline as sixel.
pub fn render(markdown: &str, font: &Font, base_path: Option<&Path>) -> String {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(markdown, options);

    let mut out = String::new();
    let mut state = RenderState::new(base_path.map(|p| p.to_path_buf()));

    for event in parser {
        match event {
            // ── Block-level tags ──────────────────────────────────────
            Event::Start(Tag::Heading { level, .. }) => {
                state.heading_level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                state.heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if state.heading_level > 0 {
                    let idx = (state.heading_level - 1).min(5);
                    let size = HEADING_SIZES[idx];
                    let color = HEADING_COLORS[idx];
                    let text = std::mem::take(&mut state.heading_text);
                    let (w, h, pixels) = crate::font::render_text(font, &text, size, color);
                    if w > 0 && h > 0 {
                        out.push_str(&sixel::encode_rgba(w, h, &pixels));
                    }
                    out.push('\n');
                    state.heading_level = 0;
                }
            }

            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                out.push_str(ansi::RESET);
                out.push_str("\n\n");
            }

            // ── Code blocks ──────────────────────────────────────────
            Event::Start(Tag::CodeBlock(_)) => {
                state.in_code_block = true;
                out.push_str(ansi::DIM);
                out.push_str("  ");
            }
            Event::End(TagEnd::CodeBlock) => {
                state.in_code_block = false;
                out.push_str(ansi::RESET);
                out.push('\n');
            }

            // ── Inline styling ───────────────────────────────────────
            Event::Start(Tag::Emphasis) => {
                state.italic = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::ITALIC);
                }
            }
            Event::End(TagEnd::Emphasis) => {
                state.italic = false;
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            Event::Start(Tag::Strong) => {
                state.bold = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::BOLD);
                }
            }
            Event::End(TagEnd::Strong) => {
                state.bold = false;
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            Event::Start(Tag::Strikethrough) => {
                state.strikethrough = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::STRIKETHROUGH);
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                state.strikethrough = false;
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            // ── Links ────────────────────────────────────────────────
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                if state.heading_level == 0 {
                    out.push_str(&ansi::link_start(&url));
                    out.push_str(ansi::UNDERLINE);
                }
                state.link_url = Some(url);
            }
            Event::End(TagEnd::Link) => {
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    out.push_str(&ansi::link_end());
                }
                state.link_url = None;
            }

            // ── Images ───────────────────────────────────────────────
            Event::Start(Tag::Image { dest_url, .. }) => {
                let path = if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
                    // Network images not supported yet
                    None
                } else {
                    let p = Path::new(dest_url.as_ref());
                    if p.is_absolute() {
                        Some(p.to_path_buf())
                    } else {
                        state.base_path.as_ref().map(|bp| bp.join(p))
                    }
                };

                if let Some(path) = path {
                    // Render image as sixel, max 800px wide
                    if let Some(sixel_data) = sixel::encode_image_file(&path, 800) {
                        out.push_str(&sixel_data);
                        out.push('\n');
                    }
                }
            }
            Event::End(TagEnd::Image) => {}

            // ── Lists ────────────────────────────────────────────────
            Event::Start(Tag::List(first_item)) => {
                state.list_stack.push(first_item);
                state.item_index.push(0);
            }
            Event::End(TagEnd::List(_)) => {
                state.list_stack.pop();
                state.item_index.pop();
                if state.list_stack.is_empty() {
                    out.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                let indent = state.list_indent();
                if let Some(idx) = state.item_index.last_mut() {
                    let prefix = match state.list_stack.last() {
                        Some(Some(start)) => {
                            let n = *start as usize + *idx;
                            format!("{indent}{n}. ")
                        }
                        _ => format!("{indent}  \u{2022} "),
                    };
                    out.push_str(&prefix);
                    *idx += 1;
                }
            }
            Event::End(TagEnd::Item) => {
                out.push_str(ansi::RESET);
                out.push('\n');
            }

            // ── Block quote ──────────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => {
                out.push_str(ansi::DIM);
                out.push_str("  \u{2502} ");
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                out.push_str(ansi::RESET);
            }

            // ── Horizontal rule ──────────────────────────────────────
            Event::Rule => {
                out.push_str(&"\u{2500}".repeat(40));
                out.push('\n');
            }

            // ── Inline code ──────────────────────────────────────────
            Event::Code(code) => {
                if state.heading_level > 0 {
                    state.heading_text.push_str(&code);
                } else {
                    out.push_str(ansi::DIM);
                    out.push_str(&code);
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            // ── Text content ─────────────────────────────────────────
            Event::Text(text) => {
                if state.heading_level > 0 {
                    state.heading_text.push_str(&text);
                } else if state.in_code_block {
                    // Indent each line of code blocks
                    for (i, line) in text.lines().enumerate() {
                        if i > 0 {
                            out.push_str("\n  ");
                        }
                        out.push_str(line);
                    }
                } else {
                    out.push_str(&text);
                }
            }

            Event::SoftBreak => {
                if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push(' ');
                }
            }
            Event::HardBreak => {
                if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push('\n');
                }
            }

            // Ignore everything else for now (HTML, footnotes, etc.)
            _ => {}
        }
    }

    out
}
