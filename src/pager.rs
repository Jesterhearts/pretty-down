use std::io::Write;
use std::io::{
    self,
};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::{
    self,
};
use crossterm::terminal::ClearType;
use crossterm::terminal::{
    self,
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
            if parts.len() >= 4
                && let Ok(pv) = parts[3]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u32>()
            {
                let cell_height = 20u32;
                return pv.div_ceil(cell_height).max(1) as u16;
            }
        }
    }

    // Fallback: count '-' characters (each = 6 pixel rows)
    let band_count = data.chars().filter(|&c| c == '-').count() as u32 + 1;
    let pixel_height = band_count * 6;
    let cell_height = 20u32;
    pixel_height.div_ceil(cell_height).max(1) as u16
}

/// Set up a file watcher that sends a message on the returned receiver when
/// the file at `path` is modified.
fn setup_watcher(path: &Path) -> Option<(mpsc::Receiver<()>, notify::RecommendedWatcher)> {
    use notify::Watcher;

    let (tx, rx) = mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                use notify::EventKind::*;
                if matches!(event.kind, Modify(_) | Create(_)) {
                    let _ = tx.send(());
                }
            }
        })
        .ok()?;

    watcher
        .watch(path, notify::RecursiveMode::NonRecursive)
        .ok()?;

    Some((rx, watcher))
}

/// Run the interactive pager.
///
/// If `watch_path` is provided, the file will be watched for changes and
/// `render_fn` will be called to produce updated output.
pub fn run(
    output: &str,
    watch_path: Option<&Path>,
    render_fn: &dyn Fn() -> String,
) {
    let mut lines = split_lines(output);
    if lines.is_empty() {
        return;
    }

    let mut stdout = io::stdout();

    // Get terminal size
    let (_, term_rows) = terminal::size().unwrap_or((80, 24));
    let viewport_rows = term_rows.saturating_sub(1);

    // Check if the content fits without scrolling and we're not watching
    let total_rows: u16 = lines.iter().map(|l| l.rows()).sum();
    if total_rows <= viewport_rows && watch_path.is_none() {
        print!("{output}");
        return;
    }

    // Set up file watcher if requested
    let watcher_state = watch_path.and_then(setup_watcher);
    let watch_rx = watcher_state.as_ref().map(|(rx, _)| rx);
    // Keep the watcher alive by holding it
    let _watcher = watcher_state.as_ref().map(|(_, w)| w);

    let watching = watch_rx.is_some();

    // Enter raw mode
    terminal::enable_raw_mode().unwrap();
    crossterm::execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide,).unwrap();

    let mut scroll_offset: usize = 0;
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            draw_screen(
                &mut stdout,
                &lines,
                scroll_offset,
                viewport_rows,
                term_rows,
                watching,
            );
            needs_redraw = false;
        }

        // Check for file changes (non-blocking)
        if let Some(rx) = watch_rx
            && rx.try_recv().is_ok()
        {
            // Drain any extra events
            while rx.try_recv().is_ok() {}
            // Small delay to let the file finish writing
            std::thread::sleep(Duration::from_millis(50));
            // Re-render
            let new_output = render_fn();
            lines = split_lines(&new_output);
            // Clamp scroll offset
            if scroll_offset >= lines.len() {
                scroll_offset = lines.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

        // Poll for keyboard input with a timeout so we can check file changes
        let poll_timeout = if watching {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(60)
        };

        if !event::poll(poll_timeout).unwrap_or(false) {
            continue;
        }

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
                    needs_redraw = true;
                }
                // Scroll up
                KeyEvent {
                    code: KeyCode::Char('k') | KeyCode::Up,
                    ..
                } => {
                    scroll_offset = retreat_lines(scroll_offset, 1);
                    needs_redraw = true;
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
                    needs_redraw = true;
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
                    needs_redraw = true;
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
                    needs_redraw = true;
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
                    needs_redraw = true;
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    scroll_offset = advance_lines(&lines, scroll_offset, viewport_rows as usize);
                    needs_redraw = true;
                }
                // Manual reload
                KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                } => {
                    let new_output = render_fn();
                    lines = split_lines(&new_output);
                    if scroll_offset >= lines.len() {
                        scroll_offset = lines.len().saturating_sub(1);
                    }
                    needs_redraw = true;
                }

                _ => {}
            }
        }
    }

    // Restore terminal
    crossterm::execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen,).unwrap();
    terminal::disable_raw_mode().unwrap();
}

/// Draw the current view to the screen.
fn draw_screen(
    stdout: &mut io::Stdout,
    lines: &[Line],
    scroll_offset: usize,
    viewport_rows: u16,
    term_rows: u16,
    watching: bool,
) {
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
    let watch_indicator = if watching { " [watching]" } else { "" };
    let status = format!(
        " [{progress}%]{watch_indicator} j/k:scroll  d/u:half-page  g/G:top/bottom  r:reload  \
         q:quit "
    );
    crossterm::execute!(stdout, cursor::MoveTo(0, term_rows - 1)).unwrap();
    write!(stdout, "\x1b[7m{status:<width$}\x1b[0m", width = 80).unwrap();
    stdout.flush().unwrap();
}

/// Advance scroll offset by `count` lines, clamped to valid range.
fn advance_lines(
    lines: &[Line],
    offset: usize,
    count: usize,
) -> usize {
    (offset + count).min(lines.len().saturating_sub(1))
}

/// Retreat scroll offset by `count` lines.
fn retreat_lines(
    offset: usize,
    count: usize,
) -> usize {
    offset.saturating_sub(count)
}

/// Compute the scroll offset that shows the last screenful.
fn scroll_to_end(
    lines: &[Line],
    viewport_rows: u16,
) -> usize {
    let mut rows: u16 = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        rows += line.rows();
        if rows > viewport_rows {
            return i + 1;
        }
    }
    0
}
