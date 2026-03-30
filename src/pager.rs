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

use crate::renderer::RenderOutput;

/// A display line is either a plain text line, a sixel block, or a pending
/// image.
enum Line {
    /// A regular text line (may contain ANSI escapes).
    Text(String),
    /// A sixel image block — occupies `height` terminal rows.
    Sixel { data: String, height: u16 },
    /// A placeholder for an image being encoded in the background.
    PendingImage { id: usize, estimated_rows: u16 },
}

impl Line {
    /// Number of terminal rows this line occupies.
    fn rows(&self) -> u16 {
        match self {
            Line::Text(_) => 1,
            Line::Sixel { height, .. } => *height,
            Line::PendingImage { estimated_rows, .. } => *estimated_rows,
        }
    }
}

/// Split rendered output into display lines.
///
/// Sixel sequences (delimited by `\x1bP` .. `\x1b\`) are kept as atomic
/// blocks. Image placeholders (`\x00IMG:id:rows\x00`) become PendingImage
/// lines. Everything else is split on newlines.
fn split_lines(output: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut rest = output;

    while !rest.is_empty() {
        // Look for image placeholder or sixel sequence
        let placeholder_pos = rest.find("\x00IMG:");
        let sixel_pos = rest.find("\x1bP");

        // Find whichever comes first
        let next = match (placeholder_pos, sixel_pos) {
            (Some(p), Some(s)) if p < s => Some(("placeholder", p)),
            (_, Some(s)) => Some(("sixel", s)),
            (Some(p), None) => Some(("placeholder", p)),
            (None, None) => None,
        };

        match next {
            Some(("placeholder", pos)) => {
                // Text before the placeholder
                let before = &rest[..pos];
                for text_line in before.split('\n') {
                    lines.push(Line::Text(text_line.to_string()));
                }

                let after = &rest[pos..];
                // Parse \x00IMG:id:rows\x00
                if let Some(end) = after[1..].find('\x00') {
                    let marker = &after[1..end + 1]; // "IMG:id:rows"
                    let parts: Vec<&str> = marker.splitn(3, ':').collect();
                    if parts.len() == 3 {
                        let id: usize = parts[1].parse().unwrap_or(0);
                        let rows: u16 = parts[2].parse().unwrap_or(3);
                        lines.push(Line::PendingImage {
                            id,
                            estimated_rows: rows,
                        });
                    }
                    rest = &after[end + 2..]; // skip past closing \x00
                    if rest.starts_with('\n') {
                        rest = &rest[1..];
                    }
                } else {
                    rest = &after[1..];
                }
            }
            Some(("sixel", pos)) => {
                let before = &rest[..pos];
                for text_line in before.split('\n') {
                    lines.push(Line::Text(text_line.to_string()));
                }

                let after_start = &rest[pos..];
                let end = after_start
                    .find("\x1b\\")
                    .map(|i| i + 2)
                    .unwrap_or(after_start.len());

                let sixel_data = &after_start[..end];
                let height = estimate_sixel_rows(sixel_data);
                lines.push(Line::Sixel {
                    data: sixel_data.to_string(),
                    height,
                });

                rest = &after_start[end..];
                if rest.starts_with('\n') {
                    rest = &rest[1..];
                }
            }
            _ => {
                for text_line in rest.split('\n') {
                    lines.push(Line::Text(text_line.to_string()));
                }
                break;
            }
        }
    }

    // Remove trailing empty lines
    while matches!(lines.last(), Some(Line::Text(t)) if t.is_empty()) {
        lines.pop();
    }

    lines
}

/// Estimate how many terminal rows a sixel image occupies.
fn estimate_sixel_rows(data: &str) -> u16 {
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
                return crate::sixel::pixel_height_to_rows(pv);
            }
        }
    }

    let band_count = data.chars().filter(|&c| c == '-').count() as u32 + 1;
    let pixel_height = band_count * 6;
    crate::sixel::pixel_height_to_rows(pixel_height)
}

/// Resolve any pending images that are ready into Sixel lines, so the
/// scroll math uses the correct actual height.
fn resolve_ready_images(
    lines: &mut [Line],
    pending: &[crate::sixel::PendingImage],
) {
    for line in lines.iter_mut() {
        if let Line::PendingImage { id, .. } = line
            && let Some(p) = pending.get(*id)
            && p.is_ready()
        {
            let data = p.wait().to_string();
            let height = if data.is_empty() {
                0
            } else {
                estimate_sixel_rows(&data)
            };
            *line = Line::Sixel { data, height };
        }
    }
}

/// Set up a file watcher.
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
pub fn run(
    output: &RenderOutput,
    watch_path: Option<&Path>,
    render_fn: &dyn Fn() -> RenderOutput,
) {
    let mut lines = split_lines(&output.text);
    let mut pending = &output.pending_images;
    // Owned storage for when we re-render
    #[allow(unused_assignments)]
    let mut current_output: Option<RenderOutput> = None;

    if lines.is_empty() {
        return;
    }

    let mut stdout = io::stdout();

    let (_, term_rows) = terminal::size().unwrap_or((80, 24));
    let viewport_rows = term_rows.saturating_sub(1);

    // If content fits and no watch and no pending images, just print directly
    let total_rows: u16 = lines.iter().map(|l| l.rows()).sum();
    let has_pending = lines.iter().any(|l| matches!(l, Line::PendingImage { .. }));
    if total_rows <= viewport_rows && watch_path.is_none() && !has_pending {
        // Print text, resolving any pending images inline
        print_output(output);
        return;
    }

    let watcher_state = watch_path.and_then(setup_watcher);
    let watch_rx = watcher_state.as_ref().map(|(rx, _)| rx);
    let _watcher = watcher_state.as_ref().map(|(_, w)| w);
    let watching = watch_rx.is_some();

    terminal::enable_raw_mode().unwrap();
    crossterm::execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide,).unwrap();

    let mut scroll_offset: usize = 0;
    let mut needs_redraw = true;
    let mut scrolled_down = false; // track if last action was downward

    loop {
        if needs_redraw {
            let overflow = draw_screen(
                &mut stdout,
                &lines,
                scroll_offset,
                viewport_rows,
                term_rows,
                watching,
            );
            // After a downward scroll, if a sixel caused auto-scroll,
            // advance scroll_offset to account for the rows that were
            // pushed off screen. Don't redraw — the screen already
            // shows the correct post-scroll state.
            if scrolled_down && overflow > 0 {
                let mut rows_to_skip = overflow;
                while rows_to_skip > 0 && scroll_offset < lines.len() {
                    let r = lines[scroll_offset].rows();
                    if r > rows_to_skip {
                        break;
                    }
                    scroll_offset += 1;
                    rows_to_skip -= r;
                }
            }
            scrolled_down = false;
            needs_redraw = false;
        }

        // Check for file changes
        if let Some(rx) = watch_rx
            && rx.try_recv().is_ok()
        {
            while rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(50));
            let new_output = render_fn();
            lines = split_lines(&new_output.text);
            current_output = Some(new_output);
            pending = &current_output.as_ref().unwrap().pending_images;
            if scroll_offset >= lines.len() {
                scroll_offset = lines.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

        // Resolve any pending images that finished encoding into Sixel
        // lines so the scroll math uses their actual height.
        let had_pending = lines.iter().any(|l| matches!(l, Line::PendingImage { .. }));
        if had_pending {
            resolve_ready_images(&mut lines, pending);
            let still_pending = lines.iter().any(|l| matches!(l, Line::PendingImage { .. }));
            if had_pending != still_pending {
                needs_redraw = true;
            }
        }

        let poll_timeout = if watching || had_pending {
            Duration::from_millis(50)
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
                    scrolled_down = true;
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
                    scrolled_down = true;
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
                    scrolled_down = true;
                    needs_redraw = true;
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    scroll_offset = advance_lines(&lines, scroll_offset, viewport_rows as usize);
                    scrolled_down = true;
                    needs_redraw = true;
                }
                KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                } => {
                    let new_output = render_fn();
                    lines = split_lines(&new_output.text);
                    current_output = Some(new_output);
                    pending = &current_output.as_ref().unwrap().pending_images;
                    if scroll_offset >= lines.len() {
                        scroll_offset = lines.len().saturating_sub(1);
                    }
                    needs_redraw = true;
                }

                _ => {}
            }
        }
    }

    crossterm::execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen,).unwrap();
    terminal::disable_raw_mode().unwrap();
}

/// Print output directly, resolving pending images synchronously.
pub fn print_output(output: &RenderOutput) {
    let lines = split_lines(&output.text);
    let pending = &output.pending_images;
    for line in &lines {
        match line {
            Line::Text(t) => println!("{t}"),
            Line::Sixel { data, .. } => println!("{data}"),
            Line::PendingImage { id, .. } => {
                if let Some(p) = pending.get(*id) {
                    let sixel = p.wait();
                    if !sixel.is_empty() {
                        println!("{sixel}");
                    }
                }
            }
        }
    }
}

/// Draw the current view.
///
/// Returns the number of rows the terminal auto-scrolled due to a sixel
/// image overflowing the viewport (0 if no overflow occurred).
fn draw_screen(
    stdout: &mut io::Stdout,
    lines: &[Line],
    scroll_offset: usize,
    viewport_rows: u16,
    term_rows: u16,
    watching: bool,
) -> u16 {
    // Begin synchronized update — terminal buffers all output until the
    // matching end sequence, eliminating flicker.
    write!(stdout, "\x1b[?2026h").unwrap();

    crossterm::execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
    )
    .unwrap();

    let mut rows_used: u16 = 0;
    let mut line_idx = scroll_offset;
    while line_idx < lines.len() && rows_used < viewport_rows {
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
                // If the sixel overflowed, the terminal auto-scrolled.
                // Stop rendering further lines since positioning is off.
                if rows_used > viewport_rows {
                    line_idx += 1;
                    break;
                }
            }
            Line::PendingImage { .. } => {
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                write!(stdout, "\x1b[2m  [loading image...]\x1b[0m\r").unwrap();
                rows_used += 1;
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

    // End synchronized update — terminal flushes the buffered frame at once.
    write!(stdout, "\x1b[?2026l").unwrap();
    stdout.flush().unwrap();

    rows_used.saturating_sub(viewport_rows)
}

fn advance_lines(
    lines: &[Line],
    offset: usize,
    count: usize,
) -> usize {
    (offset + count).min(lines.len().saturating_sub(1))
}

fn retreat_lines(
    offset: usize,
    count: usize,
) -> usize {
    offset.saturating_sub(count)
}

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
