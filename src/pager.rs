use std::io::{self, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, ClearType},
};

/// A display line is either a plain text line or an atomic sixel block.
enum Line {
    /// A regular text line (may contain ANSI escapes).
    Text(String),
    /// A sixel image block — occupies `height` terminal rows.
    Sixel { data: String, height: u16 },
}

impl Line {
    /// Number of terminal rows this line occupies.
    fn rows(&self) -> u16 {
        match self {
            Line::Text(_) => 1,
            Line::Sixel { height, .. } => *height,
        }
    }
}

/// Split rendered output into display lines.
///
/// Sixel sequences (delimited by `\x1bP` .. `\x1b\`) are kept as atomic
/// blocks. Everything else is split on newlines.
fn split_lines(output: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut rest = output;

    while !rest.is_empty() {
        // Look for the start of a sixel sequence
        if let Some(sixel_start) = rest.find("\x1bP") {
            // Text before the sixel
            let before = &rest[..sixel_start];
            for text_line in before.split('\n') {
                lines.push(Line::Text(text_line.to_string()));
            }

            // Find the sixel terminator (ST = ESC \)
            let after_start = &rest[sixel_start..];
            let end = after_start
                .find("\x1b\\")
                .map(|i| i + 2)
                .unwrap_or(after_start.len());

            let sixel_data = &after_start[..end];

            // Estimate sixel height: parse raster attributes "Pan;Pad;Ph;Pv"
            // or count '-' (newline in sixel = 6 pixel rows)
            let height = estimate_sixel_rows(sixel_data);
            lines.push(Line::Sixel {
                data: sixel_data.to_string(),
                height,
            });

            rest = &after_start[end..];
            // Consume a trailing newline after the sixel if present
            if rest.starts_with('\n') {
                rest = &rest[1..];
            }
        } else {
            // No more sixel — split remaining text on newlines
            for text_line in rest.split('\n') {
                lines.push(Line::Text(text_line.to_string()));
            }
            break;
        }
    }

    // Remove trailing empty lines
    while matches!(lines.last(), Some(Line::Text(t)) if t.is_empty()) {
        lines.pop();
    }

    lines
}

/// Estimate how many terminal rows a sixel image occupies.
///
/// First tries to parse the raster attributes (`"Pan;Pad;Ph;Pv`), falling
/// back to counting sixel newlines (`-`) which each represent 6 pixel rows.
fn estimate_sixel_rows(data: &str) -> u16 {
    // Try raster attributes: look for "Pan;Pad;Ph;Pv after the q
    if let Some(q_pos) = data.find('q') {
        let after_q = &data[q_pos + 1..];
        if let Some(raster) = after_q.strip_prefix('"') {
            let parts: Vec<&str> = raster.splitn(5, ';').collect();
            if parts.len() >= 4 {
                if let Ok(pv) = parts[3]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u32>()
                {
                    // Convert pixel height to terminal rows.
                    // Typical terminal cell height is ~20px but varies.
                    // We use a conservative estimate; terminals that support
                    // sixel generally report cell size via CSI 16 t, but
                    // for simplicity we assume ~20px per row.
                    let cell_height = 20u32;
                    return ((pv + cell_height - 1) / cell_height).max(1) as u16;
                }
            }
        }
    }

    // Fallback: count '-' characters (each = 6 pixel rows) outside of
    // color definitions
    let band_count = data.chars().filter(|&c| c == '-').count() as u32 + 1;
    let pixel_height = band_count * 6;
    let cell_height = 20u32;
    ((pixel_height + cell_height - 1) / cell_height).max(1) as u16
}

/// Run the interactive pager on the rendered output.
pub fn run(output: &str) {
    let lines = split_lines(output);
    if lines.is_empty() {
        return;
    }

    let mut stdout = io::stdout();

    // Get terminal size
    let (_, term_rows) = terminal::size().unwrap_or((80, 24));
    // Reserve 1 row for the status bar
    let viewport_rows = term_rows.saturating_sub(1);

    // Check if the content fits without scrolling
    let total_rows: u16 = lines.iter().map(|l| l.rows()).sum();
    if total_rows <= viewport_rows {
        // Content fits — just print it directly, no pager needed
        print!("{output}");
        return;
    }

    // Enter raw mode for interactive scrolling
    terminal::enable_raw_mode().unwrap();
    crossterm::execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
    )
    .unwrap();

    let mut scroll_offset: usize = 0; // index into `lines`

    loop {
        // Clear screen and draw from scroll_offset
        crossterm::execute!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0),
        )
        .unwrap();

        let mut rows_used: u16 = 0;
        let mut line_idx = scroll_offset;
        while line_idx < lines.len() && rows_used + lines[line_idx].rows() <= viewport_rows {
            match &lines[line_idx] {
                Line::Text(text) => {
                    // Move to the correct row and print
                    crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                    write!(stdout, "{text}\r").unwrap();
                    rows_used += 1;
                }
                Line::Sixel { data, height } => {
                    crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                    write!(stdout, "{data}").unwrap();
                    rows_used += height;
                }
            }
            line_idx += 1;
        }

        // Status bar
        let progress = if lines.is_empty() {
            100
        } else {
            (line_idx * 100) / lines.len()
        };
        let status = format!(
            " [{progress}%] j/k:scroll  d/u:half-page  g/G:top/bottom  q:quit "
        );
        crossterm::execute!(stdout, cursor::MoveTo(0, term_rows - 1)).unwrap();
        write!(stdout, "\x1b[7m{status:<width$}\x1b[0m", width = 80).unwrap();
        stdout.flush().unwrap();

        // Read input
        if let Ok(Event::Key(key)) = event::read() {
            match key {
                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Esc, ..
                } => break,

                // Scroll down
                KeyEvent {
                    code: KeyCode::Char('j') | KeyCode::Down,
                    ..
                } => {
                    scroll_offset = advance_lines(&lines, scroll_offset, 1);
                }
                // Scroll up
                KeyEvent {
                    code: KeyCode::Char('k') | KeyCode::Up,
                    ..
                } => {
                    scroll_offset = retreat_lines(scroll_offset, 1);
                }
                // Half page down
                KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('d'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                } => {
                    let half = (viewport_rows / 2) as usize;
                    scroll_offset = advance_lines(&lines, scroll_offset, half);
                }
                // Half page up
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('u'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                } => {
                    let half = (viewport_rows / 2) as usize;
                    scroll_offset = retreat_lines(scroll_offset, half);
                }
                // Top
                KeyEvent {
                    code: KeyCode::Char('g'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Home,
                    ..
                } => {
                    scroll_offset = 0;
                }
                // Bottom
                KeyEvent {
                    code: KeyCode::Char('G'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::End, ..
                } => {
                    scroll_offset = scroll_to_end(&lines, viewport_rows);
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    scroll_offset =
                        advance_lines(&lines, scroll_offset, viewport_rows as usize);
                }

                _ => {}
            }
        }
    }

    // Restore terminal
    crossterm::execute!(
        stdout,
        cursor::Show,
        terminal::LeaveAlternateScreen,
    )
    .unwrap();
    terminal::disable_raw_mode().unwrap();
}

/// Advance scroll offset by `count` lines, clamped to valid range.
fn advance_lines(lines: &[Line], offset: usize, count: usize) -> usize {
    (offset + count).min(lines.len().saturating_sub(1))
}

/// Retreat scroll offset by `count` lines.
fn retreat_lines(offset: usize, count: usize) -> usize {
    offset.saturating_sub(count)
}

/// Compute the scroll offset that shows the last screenful.
fn scroll_to_end(lines: &[Line], viewport_rows: u16) -> usize {
    let mut rows: u16 = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        rows += line.rows();
        if rows > viewport_rows {
            return i + 1;
        }
    }
    0
}
