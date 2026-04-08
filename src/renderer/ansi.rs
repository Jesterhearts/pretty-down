pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const ITALIC: &str = "\x1b[3m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const STRIKETHROUGH: &str = "\x1b[9m";

pub const OVERLINE: &str = "\x1b[53m";

/// OSC 8 hyperlink start
pub fn link_start(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}

/// OSC 8 hyperlink end
pub fn link_end() -> String {
    "\x1b]8;;\x1b\\".to_string()
}

/// Set foreground color using 24-bit true color.
pub fn fg_rgb(
    r: u8,
    g: u8,
    b: u8,
) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Set background color using 24-bit true color.
pub fn bg_rgb(
    r: u8,
    g: u8,
    b: u8,
) -> String {
    format!("\x1b[48;2;{r};{g};{b}m")
}

/// VTE-based width counter. The parser calls `print` only for visible
/// characters — all escape sequences (CSI, OSC, DCS, etc.) are routed
/// to other `Perform` methods that we leave as no-ops.
struct WidthCounter(usize);

impl vte::Perform for WidthCounter {
    fn print(
        &mut self,
        c: char,
    ) {
        self.0 += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
    }
}

/// Count the visible column width of a string, correctly skipping all
/// escape sequences (CSI, OSC 8 links, DCS, etc.) and accounting for
/// Unicode double-width characters.
pub fn visible_len(s: &str) -> usize {
    let mut counter = WidthCounter(0);
    let mut parser = vte::Parser::new();
    parser.advance(&mut counter, s.as_bytes());
    counter.0
}

/// VTE-based slicer. Collects bytes into a result buffer, but only
/// includes visible characters that fall within the target column range.
/// Escape sequences are always passed through so ANSI state is preserved.
struct VisibleSlicer {
    start: usize,
    end: usize,
    col: usize,
    result: String,
}

impl vte::Perform for VisibleSlicer {
    fn print(
        &mut self,
        c: char,
    ) {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if self.col >= self.start && self.col + w <= self.end {
            self.result.push(c);
        }
        self.col += w;
    }

    fn execute(
        &mut self,
        byte: u8,
    ) {
        // Pass through C0 controls (e.g. \n, \r) in the visible region
        if self.col >= self.start && self.col < self.end {
            self.result.push(byte as char);
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // Reconstruct the CSI sequence and always include it
        self.result.push('\x1b');
        self.result.push('[');
        let mut first_param = true;
        for param in params.iter() {
            if !first_param {
                self.result.push(';');
            }
            first_param = false;
            let mut first_sub = true;
            for &sub in param {
                if !first_sub {
                    self.result.push(':');
                }
                first_sub = false;
                self.result.push_str(&sub.to_string());
            }
        }
        for &b in intermediates {
            self.result.push(b as char);
        }
        self.result.push(action);
    }

    fn osc_dispatch(
        &mut self,
        params: &[&[u8]],
        bell_terminated: bool,
    ) {
        // Reconstruct OSC sequences (e.g. hyperlinks)
        self.result.push('\x1b');
        self.result.push(']');
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                self.result.push(';');
            }
            self.result.push_str(&String::from_utf8_lossy(param));
        }
        if bell_terminated {
            self.result.push('\x07');
        } else {
            self.result.push('\x1b');
            self.result.push('\\');
        }
    }

    fn esc_dispatch(
        &mut self,
        intermediates: &[u8],
        _ignore: bool,
        byte: u8,
    ) {
        self.result.push('\x1b');
        for &b in intermediates {
            self.result.push(b as char);
        }
        self.result.push(byte as char);
    }
}

/// Slice a string with ANSI escapes at visible column boundaries.
/// Returns the substring from visible column `start` with `width` visible
/// chars, preserving all escape sequences so ANSI state carries through.
pub fn visible_slice(
    s: &str,
    start: usize,
    width: usize,
) -> String {
    let mut slicer = VisibleSlicer {
        start,
        end: start + width,
        col: 0,
        result: String::new(),
    };
    let mut parser = vte::Parser::new();
    parser.advance(&mut slicer, s.as_bytes());
    slicer.result
}

/// Word-wrap a string that may contain ANSI escape sequences.
///
/// Wraps at `width` visible columns, preserving escape sequences
/// (which don't consume column space). Uses `vte` to identify
/// visible characters vs escape bytes.
pub fn wrap(
    text: &str,
    width: u16,
) -> String {
    let width = width as usize;
    if width == 0 {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let mut col = 0usize;
    // Track the last word boundary: (byte pos in result, visible col)
    let mut last_break: Option<(usize, usize)> = None;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        // Copy escape sequences verbatim (zero visible width).
        // We use a simple scan here because wrap only needs to skip
        // escapes, not interpret them — the vte state machine is
        // heavier than necessary for this pass.
        if ch == '\x1b' {
            result.push(ch);
            match chars.peek() {
                // CSI sequence: \x1b[ ... <letter>
                Some(&'[') => {
                    while let Some(next) = chars.next() {
                        result.push(next);
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC sequence: \x1b] ... (ST or BEL)
                Some(&']') => {
                    while let Some(next) = chars.next() {
                        result.push(next);
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            if let Some(&'\\') = chars.peek() {
                                result.push(chars.next().unwrap());
                                break;
                            }
                        }
                    }
                }
                // Other escapes (e.g. \x1b( ): two-byte sequence
                _ => {
                    if let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
            }
            continue;
        }

        if ch == '\n' {
            result.push('\n');
            col = 0;
            last_break = None;
            continue;
        }

        if ch == ' ' {
            last_break = Some((result.len(), col));
        }

        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        result.push(ch);
        col += char_width;

        if col > width {
            if let Some((break_pos, break_col)) = last_break.take() {
                result.replace_range(break_pos..break_pos + 1, "\n");
                col -= break_col + 1;
            } else {
                let ch_len = ch.len_utf8();
                let insert_pos = result.len() - ch_len;
                result.insert(insert_pos, '\n');
                col = char_width;
            }
        }
    }

    result
}
