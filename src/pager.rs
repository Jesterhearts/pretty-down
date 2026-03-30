use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::io::{
    self,
};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use crossterm::event::{
    self,
};
use crossterm::terminal::ClearType;
use crossterm::terminal::{
    self,
};

use crate::renderer::RenderOutput;

/// A display line.
enum Line {
    /// A regular text line (may contain ANSI escapes).
    Text(String),
    /// A sixel image block — occupies `height` terminal rows.
    Sixel { data: String, height: u16 },
    /// A placeholder for an image being encoded in the background.
    PendingImage { id: usize, estimated_rows: u16 },
    /// Start of a `<details>` block (invisible marker, zero height).
    #[allow(dead_code)]
    DetailsStart { id: usize },
    /// The `<summary>` line for a details block.
    DetailsSummary { id: usize, text: String },
    /// End of a `<details>` block (invisible marker, zero height).
    DetailsEnd { id: usize },
    /// An animated GIF placeholder.
    Gif { id: usize, estimated_rows: u16 },
}

impl Line {
    /// Number of terminal rows this line occupies.
    fn rows(&self) -> u16 {
        match self {
            Line::Text(_) => 1,
            Line::Sixel { height, .. } => *height,
            Line::PendingImage { estimated_rows, .. } | Line::Gif { estimated_rows, .. } => {
                *estimated_rows
            }
            Line::DetailsSummary { .. } => 1,
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => 0,
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
        // Find the next special sequence: \x00 marker or \x1bP sixel
        let marker_pos = rest.find('\x00');
        let sixel_pos = rest.find("\x1bP");

        let next = match (marker_pos, sixel_pos) {
            (Some(m), Some(s)) if m < s => Some(("marker", m)),
            (_, Some(s)) => Some(("sixel", s)),
            (Some(m), None) => Some(("marker", m)),
            (None, None) => None,
        };

        match next {
            Some(("marker", pos)) => {
                // Text before the marker
                let before = &rest[..pos];
                for text_line in before.split('\n') {
                    lines.push(Line::Text(text_line.to_string()));
                }

                let after = &rest[pos..];
                if let Some(end) = after[1..].find('\x00') {
                    let marker = &after[1..end + 1];
                    parse_marker(marker, &mut lines);
                    rest = &after[end + 2..];
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

/// Parse a `\x00...\x00` marker into Line variants.
fn parse_marker(
    marker: &str,
    lines: &mut Vec<Line>,
) {
    let parts: Vec<&str> = marker.splitn(3, ':').collect();
    match parts.first().copied() {
        Some("IMG") if parts.len() == 3 => {
            let id: usize = parts[1].parse().unwrap_or(0);
            let rows: u16 = parts[2].parse().unwrap_or(3);
            lines.push(Line::PendingImage {
                id,
                estimated_rows: rows,
            });
        }
        Some("DETAILS") if parts.len() >= 2 => {
            let id: usize = parts[1].parse().unwrap_or(0);
            lines.push(Line::DetailsStart { id });
        }
        Some("SUMMARY") if parts.len() >= 3 => {
            let id: usize = parts[1].parse().unwrap_or(0);
            let text = parts[2].to_string();
            lines.push(Line::DetailsSummary { id, text });
        }
        Some("GIF") if parts.len() == 3 => {
            let id: usize = parts[1].parse().unwrap_or(0);
            let rows: u16 = parts[2].parse().unwrap_or(3);
            lines.push(Line::Gif {
                id,
                estimated_rows: rows,
            });
        }
        Some("/DETAILS") if parts.len() >= 2 => {
            let id: usize = parts[1].parse().unwrap_or(0);
            lines.push(Line::DetailsEnd { id });
        }
        _ => {}
    }
}

/// Compute indices of lines that are visible given the current collapsed state.
/// Lines inside collapsed `<details>` blocks are excluded (except the summary).
fn visible_indices(
    lines: &[Line],
    collapsed: &HashSet<usize>,
) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut skip_id: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(sid) = skip_id {
            if matches!(line, Line::DetailsEnd { id } if *id == sid) {
                skip_id = None;
                // Include the DetailsEnd so draw knows the block ended
                visible.push(i);
            }
            continue;
        }
        match line {
            Line::DetailsSummary { id, .. } if collapsed.contains(id) => {
                visible.push(i);
                skip_id = Some(*id);
            }
            _ => visible.push(i),
        }
    }
    visible
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
    let mut pending_gifs = &output.pending_gifs;
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
    crossterm::execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        EnableMouseCapture,
    )
    .unwrap();

    let mut scroll_offset: usize = 0; // index into `visible`
    let mut needs_redraw = true;
    // Start with all details blocks collapsed (matches GitHub behavior)
    let mut collapsed: HashSet<usize> = lines
        .iter()
        .filter_map(|l| match l {
            Line::DetailsSummary { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let mut visible = visible_indices(&lines, &collapsed);

    #[derive(Clone, Copy, PartialEq)]
    enum ScrollDir {
        None,
        Down,
        Up,
    }
    let mut scroll_dir = ScrollDir::None;
    let mut last_draw = DrawResult {
        overflow: None,
        summary_rows: Vec::new(),
    };
    // Per-GIF animation state: (current frame index, next frame deadline)
    let mut gif_state: HashMap<usize, (usize, std::time::Instant)> = HashMap::new();

    loop {
        // Advance any GIFs whose frame deadline has passed
        let now = std::time::Instant::now();
        let mut any_advanced = false;
        for (id, (frame_idx, deadline)) in gif_state.iter_mut() {
            if now >= *deadline
                && let Some(gif) = pending_gifs.get(*id)
            {
                let count = gif.frame_count();
                if count > 1 {
                    *frame_idx = (*frame_idx + 1) % count;
                    if let Some(frame) = gif.frame(*frame_idx) {
                        *deadline = now + Duration::from_millis(frame.delay_ms as u64);
                    }
                    any_advanced = true;
                }
            }
        }
        if any_advanced {
            needs_redraw = true;
        }

        if needs_redraw {
            // Build the frame index map for draw_screen
            let gif_frames: HashMap<usize, usize> =
                gif_state.iter().map(|(id, (idx, _))| (*id, *idx)).collect();

            last_draw = draw_screen(
                &mut stdout,
                &lines,
                &visible,
                &collapsed,
                pending_gifs,
                &gif_frames,
                scroll_offset,
                viewport_rows,
                term_rows,
                watching,
            );

            // Register any newly visible GIFs that aren't tracked yet
            for &vi in &visible[scroll_offset..] {
                if let Line::Gif { id, .. } = &lines[vi] {
                    gif_state.entry(*id).or_insert_with(|| {
                        let delay = pending_gifs
                            .get(*id)
                            .and_then(|g| g.frame(0))
                            .map(|f| f.delay_ms)
                            .unwrap_or(100);
                        (
                            0,
                            std::time::Instant::now() + Duration::from_millis(delay as u64),
                        )
                    });
                }
            }
            if let Some(ref ov) = last_draw.overflow {
                match scroll_dir {
                    ScrollDir::Down => {
                        let mut rows_to_skip = ov.rows;
                        while rows_to_skip > 0 && scroll_offset < visible.len() {
                            let r = lines[visible[scroll_offset]].rows();
                            if r > rows_to_skip {
                                break;
                            }
                            scroll_offset += 1;
                            rows_to_skip -= r;
                        }
                    }
                    ScrollDir::Up => {
                        let image_vis_idx = ov.vis_idx;
                        let mut rows = 0u16;
                        let mut target = image_vis_idx;
                        while target > 0 {
                            target -= 1;
                            rows += lines[visible[target]].rows();
                            if rows >= viewport_rows {
                                break;
                            }
                        }
                        scroll_offset = target;
                    }
                    ScrollDir::None => {}
                }
            }
            scroll_dir = ScrollDir::None;
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
            visible = visible_indices(&lines, &collapsed);
            current_output = Some(new_output);
            pending = &current_output.as_ref().unwrap().pending_images;
            pending_gifs = &current_output.as_ref().unwrap().pending_gifs;
            gif_state.clear();
            if scroll_offset >= visible.len() {
                scroll_offset = visible.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

        // Resolve any pending images that finished encoding into Sixel
        // lines so the scroll math uses their actual height.
        let had_pending = lines.iter().any(|l| matches!(l, Line::PendingImage { .. }));
        if had_pending {
            resolve_ready_images(&mut lines, pending);
            visible = visible_indices(&lines, &collapsed);
            let still_pending = lines.iter().any(|l| matches!(l, Line::PendingImage { .. }));
            if had_pending != still_pending {
                needs_redraw = true;
            }
        }

        // Find the earliest GIF frame deadline
        let next_gif_deadline = gif_state.values().map(|(_, deadline)| *deadline).min();

        let poll_timeout = if let Some(t) = next_gif_deadline {
            t.saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_millis(1))
        } else if watching || had_pending {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(60)
        };

        if !event::poll(poll_timeout).unwrap_or(false) {
            continue;
        }

        let Ok(ev) = event::read() else {
            continue;
        };

        // Handle mouse clicks on details summaries
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            ..
        }) = ev
            && let Some(&(_, id)) = last_draw.summary_rows.iter().find(|(r, _)| *r == row)
        {
            if collapsed.contains(&id) {
                collapsed.remove(&id);
            } else {
                collapsed.insert(id);
            }
            visible = visible_indices(&lines, &collapsed);
            if scroll_offset >= visible.len() {
                scroll_offset = visible.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

        // Handle mouse scroll
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..
        }) = ev
        {
            scroll_offset = advance_lines(&visible, scroll_offset, 3);
            scroll_dir = ScrollDir::Down;
            needs_redraw = true;
            continue;
        }
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..
        }) = ev
        {
            scroll_offset = retreat_lines(scroll_offset, 3);
            scroll_dir = ScrollDir::Up;
            needs_redraw = true;
            continue;
        }

        if let Event::Key(key) = ev {
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
                    scroll_offset = advance_lines(&visible, scroll_offset, 1);
                    scroll_dir = ScrollDir::Down;
                    needs_redraw = true;
                }
                // Scroll up
                KeyEvent {
                    code: KeyCode::Char('k') | KeyCode::Up,
                    ..
                } => {
                    scroll_offset = retreat_lines(scroll_offset, 1);
                    scroll_dir = ScrollDir::Up;
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
                    scroll_offset = advance_lines(&visible, scroll_offset, half);
                    scroll_dir = ScrollDir::Down;
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
                    scroll_dir = ScrollDir::Up;
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
                    scroll_offset = scroll_to_end(&lines, &visible, viewport_rows);
                    scroll_dir = ScrollDir::Down;
                    needs_redraw = true;
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    scroll_offset = advance_lines(&visible, scroll_offset, viewport_rows as usize);
                    scroll_dir = ScrollDir::Down;
                    needs_redraw = true;
                }
                KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                } => {
                    let new_output = render_fn();
                    lines = split_lines(&new_output.text);
                    visible = visible_indices(&lines, &collapsed);
                    current_output = Some(new_output);
                    pending = &current_output.as_ref().unwrap().pending_images;
                    pending_gifs = &current_output.as_ref().unwrap().pending_gifs;
                    gif_state.clear();
                    if scroll_offset >= visible.len() {
                        scroll_offset = visible.len().saturating_sub(1);
                    }
                    needs_redraw = true;
                }

                // Toggle the first visible details block
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    if let Some(id) = first_visible_details(&lines, &visible, scroll_offset) {
                        if collapsed.contains(&id) {
                            collapsed.remove(&id);
                        } else {
                            collapsed.insert(id);
                        }
                        visible = visible_indices(&lines, &collapsed);
                        // Clamp scroll offset to new visible range
                        if scroll_offset >= visible.len() {
                            scroll_offset = visible.len().saturating_sub(1);
                        }
                        needs_redraw = true;
                    }
                }

                _ => {}
            }
        }
    }

    crossterm::execute!(
        stdout,
        crossterm::event::DisableMouseCapture,
        cursor::Show,
        terminal::LeaveAlternateScreen,
    )
    .unwrap();
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
            Line::Gif { id, .. } => {
                // Non-pager: just show first frame
                if let Some(gif) = output.pending_gifs.get(*id) {
                    while !gif.is_done() && gif.frame_count() == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    if let Some(frame) = gif.frame(0) {
                        println!("{}", frame.sixel);
                    }
                }
            }
            Line::DetailsSummary { text, .. } => {
                println!("\x1b[1m\u{25BC} {text}\x1b[0m");
            }
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => {}
        }
    }
}

/// Info about a sixel overflow that occurred during drawing.
struct Overflow {
    /// Number of rows the terminal auto-scrolled.
    rows: u16,
    /// Index into `visible` of the sixel that caused the overflow.
    vis_idx: usize,
}

struct DrawResult {
    overflow: Option<Overflow>,
    /// Maps terminal row → details block ID for click handling.
    summary_rows: Vec<(u16, usize)>,
}

/// Draw the current view.
/// `scroll_offset` indexes into `visible`, which maps to actual line indices.
#[allow(clippy::too_many_arguments)]
fn draw_screen(
    stdout: &mut io::Stdout,
    lines: &[Line],
    visible: &[usize],
    collapsed: &HashSet<usize>,
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &HashMap<usize, usize>,
    scroll_offset: usize,
    viewport_rows: u16,
    term_rows: u16,
    watching: bool,
) -> DrawResult {
    write!(stdout, "\x1b[?2026h").unwrap();

    crossterm::execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
    )
    .unwrap();

    let mut rows_used: u16 = 0;
    let mut vis_idx = scroll_offset;
    let mut overflow: Option<Overflow> = None;
    let mut summary_rows: Vec<(u16, usize)> = Vec::new();

    while vis_idx < visible.len() && rows_used < viewport_rows {
        let line_idx = visible[vis_idx];
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
                if rows_used > viewport_rows {
                    overflow = Some(Overflow {
                        rows: rows_used - viewport_rows,
                        vis_idx,
                    });
                    vis_idx += 1;
                    break;
                }
            }
            Line::PendingImage { .. } => {
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                write!(stdout, "\x1b[2m  [loading image...]\x1b[0m\r").unwrap();
                rows_used += 1;
            }
            Line::Gif { id, estimated_rows } => {
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                let frame_idx = gif_frames.get(id).copied().unwrap_or(0);
                if let Some(gif) = gifs.get(*id) {
                    if let Some(frame) = gif.frame(frame_idx) {
                        write!(stdout, "{}", frame.sixel).unwrap();
                        rows_used += estimated_rows;
                        if rows_used > viewport_rows {
                            overflow = Some(Overflow {
                                rows: rows_used - viewport_rows,
                                vis_idx,
                            });
                            vis_idx += 1;
                            break;
                        }
                    } else {
                        write!(stdout, "\x1b[2m  [loading gif...]\x1b[0m\r").unwrap();
                        rows_used += 1;
                    }
                } else {
                    rows_used += estimated_rows;
                }
            }
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => {
                // Invisible markers, zero height
            }
            Line::DetailsSummary { id, text } => {
                let is_collapsed = collapsed.contains(id);
                let triangle = if is_collapsed { "\u{25B6}" } else { "\u{25BC}" };
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                write!(stdout, "\x1b[1m{triangle} {text}\x1b[0m\r").unwrap();
                summary_rows.push((rows_used, *id));
                rows_used += 1;
            }
        }
        vis_idx += 1;
    }

    // Status bar
    let progress = if visible.is_empty() {
        100
    } else {
        (vis_idx * 100) / visible.len()
    };
    let watch_indicator = if watching { " [watching]" } else { "" };
    let status = format!(
        " [{progress}%]{watch_indicator} j/k:scroll  d/u:half-page  g/G:top/bottom  enter:toggle  \
         r:reload  q:quit "
    );
    crossterm::execute!(stdout, cursor::MoveTo(0, term_rows - 1)).unwrap();
    write!(stdout, "\x1b[7m{status:<width$}\x1b[0m", width = 80).unwrap();

    // End synchronized update — terminal flushes the buffered frame at once.
    write!(stdout, "\x1b[?2026l").unwrap();
    stdout.flush().unwrap();

    DrawResult {
        overflow,
        summary_rows,
    }
}

/// Find the details block to toggle.
///
/// First checks if `scroll_offset` is inside an expanded details block
/// (i.e. past its summary but before its end) by scanning backward.
/// If so, returns that block's ID. Otherwise returns the first
/// `DetailsSummary` visible from `scroll_offset` onward.
fn first_visible_details(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    // Check if we're inside a details block by scanning backward for an
    // unmatched DetailsStart/DetailsSummary before the current position.
    let current_line_idx = visible.get(scroll_offset).copied().unwrap_or(0);
    let mut depth: Vec<usize> = Vec::new();
    for &idx in visible.iter().take(scroll_offset + 1) {
        match &lines[idx] {
            Line::DetailsSummary { id, .. } => {
                depth.push(*id);
            }
            Line::DetailsEnd { id } => {
                depth.retain(|d| d != id);
            }
            _ => {}
        }
    }
    // If we're inside a block (depth is non-empty), return the innermost one
    if let Some(&id) = depth.last() {
        // Only if we're actually past the summary (not ON it)
        let on_summary =
            matches!(&lines[current_line_idx], Line::DetailsSummary { id: sid, .. } if *sid == id);
        if !on_summary {
            return Some(id);
        }
    }

    // Otherwise, find the first summary at or after scroll_offset
    for &idx in &visible[scroll_offset..] {
        if let Line::DetailsSummary { id, .. } = &lines[idx] {
            return Some(*id);
        }
    }
    None
}

fn advance_lines(
    visible: &[usize],
    offset: usize,
    count: usize,
) -> usize {
    (offset + count).min(visible.len().saturating_sub(1))
}

fn retreat_lines(
    offset: usize,
    count: usize,
) -> usize {
    offset.saturating_sub(count)
}

fn scroll_to_end(
    lines: &[Line],
    visible: &[usize],
    viewport_rows: u16,
) -> usize {
    let mut rows: u16 = 0;
    for (i, &line_idx) in visible.iter().enumerate().rev() {
        rows += lines[line_idx].rows();
        if rows > viewport_rows {
            return i + 1;
        }
    }
    0
}
