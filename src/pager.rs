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

/// Identifies an image group (for sixel replacement of preview rows).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ImageGroup {
    Sixel(usize),        // index into a vec of sixel data
    PendingImage(usize), // index into pending_images
    Gif(usize),          // index into pending_gifs
}

/// A display line.
#[derive(Clone)]
enum Line {
    /// A regular text line (may contain ANSI escapes).
    Text(String),
    /// One row of a half-block image preview, belonging to an image group.
    /// When all rows of the group are visible, the pager replaces them
    /// with the full sixel image.
    ImageRow {
        group: ImageGroup,
        row_in_group: u16,
        total_rows: u16,
        preview_text: String,
    },
    /// Start of a `<details>` block (invisible marker, zero height).
    #[allow(dead_code)]
    DetailsStart { id: usize },
    /// The `<summary>` line for a details block.
    DetailsSummary { id: usize, text: String },
    /// End of a `<details>` block (invisible marker, zero height).
    DetailsEnd { id: usize },
    /// A horizontally scrollable code block.
    CodeBlock { id: usize, height: u16 },
    /// Video playback control bar (play/pause + progress).
    VideoControls { gif_id: usize },
}

impl Line {
    /// Number of terminal rows this line occupies.
    fn rows(&self) -> u16 {
        match self {
            Line::Text(_) | Line::ImageRow { .. } => 1,
            Line::CodeBlock { height, .. } => *height,
            Line::VideoControls { .. } => 1,
            Line::DetailsSummary { .. } => 1,
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => 0,
        }
    }
}

/// Sixel data stored separately, referenced by ImageGroup.
struct SixelData {
    data: String,
    #[allow(dead_code)]
    height: u16,
}

/// Flatten output blocks into display lines for the pager.
/// Each image/sixel is expanded into individual preview rows that scroll
/// naturally. The draw loop replaces them with the full sixel when all
/// rows are visible.
fn flatten_blocks(
    output: &crate::renderer::RenderOutput,
    sixel_store: &mut Vec<SixelData>,
) -> Vec<Line> {
    use crate::renderer::OutputBlock;

    let mut lines = Vec::new();
    sixel_store.clear();

    for block in &output.blocks {
        match block {
            OutputBlock::Text(text) => {
                for line in text.split('\n') {
                    lines.push(Line::Text(line.to_string()));
                }
            }
            OutputBlock::Sixel {
                data,
                height,
                preview,
            } => {
                let group_id = sixel_store.len();
                sixel_store.push(SixelData {
                    data: data.clone(),
                    height: *height,
                });
                let group = ImageGroup::Sixel(group_id);
                let total = *height;
                for (i, pline) in preview.iter().enumerate().take(total as usize) {
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i as u16,
                        total_rows: total,
                        preview_text: pline.clone(),
                    });
                }
                // Pad if preview has fewer lines than height
                for i in preview.len()..total as usize {
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i as u16,
                        total_rows: total,
                        preview_text: String::new(),
                    });
                }
            }
            OutputBlock::Image(id) => {
                let p = output.pending_images.get(*id);
                let rows = p.map(|p| p.estimated_rows).unwrap_or(1);
                let group = ImageGroup::PendingImage(*id);
                let preview = p.map(|p| &p.preview[..]).unwrap_or(&[]);
                for i in 0..rows {
                    let preview_text = preview.get(i as usize).cloned().unwrap_or_default();
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i,
                        total_rows: rows,
                        preview_text,
                    });
                }
            }
            OutputBlock::Gif(id) => {
                let g = output.pending_gifs.get(*id);
                let rows = g.map(|g| g.estimated_rows).unwrap_or(1);
                let preview = g.map(|g| &g.preview[..]).unwrap_or(&[]);
                let is_video = g.is_some_and(|g| g.is_video);
                let group = ImageGroup::Gif(*id);
                for i in 0..rows {
                    let preview_text = preview.get(i as usize).cloned().unwrap_or_default();
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i,
                        total_rows: rows,
                        preview_text,
                    });
                }
                if is_video {
                    lines.push(Line::VideoControls { gif_id: *id });
                }
            }
            OutputBlock::Code(id) => {
                let height = output
                    .code_blocks
                    .get(*id)
                    .map(|b| b.lines.len() as u16 + 1) // +1 for scrollbar
                    .unwrap_or(1);
                lines.push(Line::CodeBlock { id: *id, height });
            }
            OutputBlock::DetailsStart { id } => {
                lines.push(Line::DetailsStart { id: *id });
            }
            OutputBlock::DetailsSummary { id, text } => {
                lines.push(Line::DetailsSummary {
                    id: *id,
                    text: text.clone(),
                });
            }
            OutputBlock::DetailsEnd { id } => {
                lines.push(Line::DetailsEnd { id: *id });
            }
        }
    }

    // Remove trailing empty lines
    while matches!(lines.last(), Some(Line::Text(t)) if t.is_empty()) {
        lines.pop();
    }

    lines
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
    let mut sixel_store = Vec::new();
    let mut lines = flatten_blocks(output, &mut sixel_store);
    let mut pending = &output.pending_images;
    let mut pending_gifs = &output.pending_gifs;
    let mut code_blocks = &output.code_blocks;
    // Owned storage for when we re-render
    #[allow(unused_assignments)]
    let mut current_output: Option<RenderOutput> = None;

    if lines.is_empty() {
        return;
    }

    let mut stdout = io::stdout();

    let (mut term_cols, mut term_rows) = terminal::size().unwrap_or((80, 24));
    let mut viewport_rows = term_rows.saturating_sub(1);

    // If content fits and no watch and no pending images, just print directly
    let total_rows: u16 = lines.iter().map(|l| l.rows()).sum();
    let has_pending = lines.iter().any(|l| {
        matches!(
            l,
            Line::ImageRow {
                group: ImageGroup::PendingImage(_),
                ..
            }
        )
    });
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

    let mut last_draw = DrawResult {
        summary_rows: Vec::new(),
        video_control_rows: Vec::new(),
    };
    // Per-GIF animation state: (current frame index, next frame deadline)
    let mut gif_state: HashMap<usize, (usize, std::time::Instant)> = HashMap::new();
    let mut code_h_scroll: HashMap<usize, usize> = HashMap::new();
    let mut video_paused: HashSet<usize> = HashSet::new();

    loop {
        if advance_gif_frames(&mut gif_state, pending_gifs, &video_paused) {
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
                &sixel_store,
                pending,
                pending_gifs,
                &gif_frames,
                &video_paused,
                code_blocks,
                &code_h_scroll,
                scroll_offset,
                viewport_rows,
                term_cols,
                term_rows,
                watching,
            );

            // Register any newly visible GIFs that aren't tracked yet
            for &vi in &visible[scroll_offset..] {
                if let Line::ImageRow {
                    group: ImageGroup::Gif(id),
                    row_in_group: 0,
                    ..
                } = &lines[vi]
                {
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
            needs_redraw = false;
        }

        // Check for file changes
        if let Some(rx) = watch_rx
            && rx.try_recv().is_ok()
        {
            while rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(50));
            let new_output = render_fn();
            lines = flatten_blocks(&new_output, &mut sixel_store);
            visible = visible_indices(&lines, &collapsed);
            current_output = Some(new_output);
            pending = &current_output.as_ref().unwrap().pending_images;
            pending_gifs = &current_output.as_ref().unwrap().pending_gifs;
            code_blocks = &current_output.as_ref().unwrap().code_blocks;
            gif_state.clear();
            if scroll_offset >= visible.len() {
                scroll_offset = visible.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

        if check_visible_images_ready(&lines, &visible, pending, scroll_offset, viewport_rows) {
            needs_redraw = true;
        }

        // Find the earliest GIF frame deadline
        let next_gif_deadline = gif_state.values().map(|(_, deadline)| *deadline).min();

        let poll_timeout = if let Some(t) = next_gif_deadline {
            t.saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_millis(1))
        } else if watching || has_pending {
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

        // Handle terminal resize — re-render with new dimensions
        if let Event::Resize(cols, rows) = ev {
            term_cols = cols;
            let new_viewport = rows.saturating_sub(1);
            // Invalidate cached pixel width so re-render picks up new size
            crate::sixel::invalidate_terminal_size();
            let new_output = render_fn();
            lines = flatten_blocks(&new_output, &mut sixel_store);
            visible = visible_indices(&lines, &collapsed);
            current_output = Some(new_output);
            pending = &current_output.as_ref().unwrap().pending_images;
            pending_gifs = &current_output.as_ref().unwrap().pending_gifs;
            code_blocks = &current_output.as_ref().unwrap().code_blocks;
            gif_state.clear();
            // Update viewport dimensions
            viewport_rows = new_viewport;
            term_rows = rows;
            if scroll_offset >= visible.len() {
                scroll_offset = visible.len().saturating_sub(1);
            }
            needs_redraw = true;
            continue;
        }

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

        // Handle click on video control bar
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            ..
        }) = ev
            && let Some(&(_, gif_id)) = last_draw.video_control_rows.iter().find(|(r, _)| *r == row)
        {
            if video_paused.contains(&gif_id) {
                video_paused.remove(&gif_id);
            } else {
                video_paused.insert(gif_id);
            }
            needs_redraw = true;
            continue;
        }

        // Handle mouse horizontal scroll on code blocks
        // Supports native ScrollLeft/ScrollRight and Shift+ScrollDown/ScrollUp
        {
            let h_scroll_dir = match &ev {
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollRight,
                    ..
                }) => Some((true, None)),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollLeft,
                    ..
                }) => Some((false, None)),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    row,
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::SHIFT) => Some((true, Some(*row))),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    row,
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::SHIFT) => Some((false, Some(*row))),
                _ => None,
            };
            if let Some((right, row_override)) = h_scroll_dir {
                let row = row_override.unwrap_or({
                    if let Event::Mouse(MouseEvent { row, .. }) = &ev {
                        *row
                    } else {
                        0
                    }
                });
                if let Some(block_id) = find_code_block_at_row(
                    &lines,
                    &visible,
                    code_blocks,
                    scroll_offset,
                    viewport_rows,
                    row,
                ) {
                    let entry = code_h_scroll.entry(block_id).or_insert(0);
                    let max = code_blocks[block_id]
                        .max_width
                        .saturating_sub(term_cols as usize);
                    if right {
                        *entry = (*entry + 4).min(max);
                    } else {
                        *entry = entry.saturating_sub(4);
                    }
                    needs_redraw = true;
                    continue;
                }
            }
        }

        // Handle mouse vertical scroll
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..
        }) = ev
        {
            scroll_offset = advance_lines(&visible, scroll_offset, 3);
            needs_redraw = true;
            continue;
        }
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..
        }) = ev
        {
            scroll_offset = retreat_lines(scroll_offset, 3);
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
                    scroll_offset = advance_lines(&visible, scroll_offset, half);
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
                    scroll_offset = scroll_to_end(&lines, &visible, viewport_rows);
                    needs_redraw = true;
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    scroll_offset = advance_lines(&visible, scroll_offset, viewport_rows as usize);
                    needs_redraw = true;
                }
                // Horizontal scroll code blocks
                KeyEvent {
                    code: KeyCode::Char('h') | KeyCode::Left,
                    ..
                } => {
                    if let Some(block_id) =
                        first_visible_code_block(&lines, &visible, scroll_offset)
                    {
                        let entry = code_h_scroll.entry(block_id).or_insert(0);
                        *entry = entry.saturating_sub(4);
                        needs_redraw = true;
                    }
                }
                KeyEvent {
                    code: KeyCode::Char('l') | KeyCode::Right,
                    ..
                } => {
                    if let Some(block_id) =
                        first_visible_code_block(&lines, &visible, scroll_offset)
                    {
                        let entry = code_h_scroll.entry(block_id).or_insert(0);
                        let max = code_blocks
                            .get(block_id)
                            .map(|b| b.max_width.saturating_sub(term_cols as usize))
                            .unwrap_or(0);
                        *entry = (*entry + 4).min(max);
                        needs_redraw = true;
                    }
                }
                KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                } => {
                    let new_output = render_fn();
                    lines = flatten_blocks(&new_output, &mut sixel_store);
                    visible = visible_indices(&lines, &collapsed);
                    current_output = Some(new_output);
                    pending = &current_output.as_ref().unwrap().pending_images;
                    pending_gifs = &current_output.as_ref().unwrap().pending_gifs;
                    code_blocks = &current_output.as_ref().unwrap().code_blocks;
                    gif_state.clear();
                    if scroll_offset >= visible.len() {
                        scroll_offset = visible.len().saturating_sub(1);
                    }
                    needs_redraw = true;
                }

                // Toggle details or video play/pause
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    // Try toggling a video first
                    let toggled_video =
                        first_visible_video_controls(&lines, &visible, scroll_offset);
                    if let Some(gif_id) = toggled_video {
                        if video_paused.contains(&gif_id) {
                            video_paused.remove(&gif_id);
                        } else {
                            video_paused.insert(gif_id);
                        }
                        needs_redraw = true;
                    } else if let Some(id) = first_visible_details(&lines, &visible, scroll_offset)
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
    let mut sixel_store = Vec::new();
    let lines = flatten_blocks(output, &mut sixel_store);
    // For non-pager output, print sixel for images at row 0 of each group
    let mut printed_groups = std::collections::HashSet::new();
    for line in &lines {
        match line {
            Line::Text(t) => println!("{t}"),
            Line::ImageRow {
                group,
                row_in_group,
                ..
            } => {
                if *row_in_group == 0 && printed_groups.insert(*group) {
                    match group {
                        ImageGroup::Sixel(id) => {
                            if let Some(sd) = sixel_store.get(*id) {
                                println!("{}", sd.data);
                            }
                        }
                        ImageGroup::PendingImage(id) => {
                            if let Some(p) = output.pending_images.get(*id) {
                                let sixel = p.wait();
                                if !sixel.is_empty() {
                                    println!("{sixel}");
                                }
                            }
                        }
                        ImageGroup::Gif(id) => {
                            if let Some(gif) = output.pending_gifs.get(*id) {
                                while !gif.is_done() && gif.frame_count() == 0 {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                                if let Some(frame) = gif.frame(0) {
                                    println!("{}", frame.sixel);
                                }
                            }
                        }
                    }
                }
                // Skip non-first rows of the group (sixel covers them)
            }
            Line::DetailsSummary { text, .. } => {
                println!("\x1b[1m\u{25BC} {text}\x1b[0m");
            }
            Line::CodeBlock { id, .. } => {
                if let Some(block) = output.code_blocks.get(*id) {
                    for line in &block.lines {
                        println!("  {line}");
                    }
                    println!();
                }
            }
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => {}
            Line::VideoControls { .. } => {} // no controls in non-pager mode
        }
    }
}

struct DrawResult {
    /// Maps terminal row → details block ID for click handling.
    summary_rows: Vec<(u16, usize)>,
    /// Maps terminal row → gif_id for video control click handling.
    video_control_rows: Vec<(u16, usize)>,
}

/// Draw the current view.
/// `scroll_offset` indexes into `visible`, which maps to actual line indices.
/// Try to render a full sixel image for an image group. Returns true if
/// rendered.
fn advance_gif_frames(
    gif_state: &mut HashMap<usize, (usize, std::time::Instant)>,
    pending_gifs: &[crate::sixel::PendingGif],
    video_paused: &HashSet<usize>,
) -> bool {
    let now = std::time::Instant::now();
    let mut any_advanced = false;
    for (id, (frame_idx, deadline)) in gif_state.iter_mut() {
        if now >= *deadline
            && !video_paused.contains(id)
            && let Some(gif) = pending_gifs.get(*id)
        {
            let count = gif.frame_count();
            if count > 1 {
                *frame_idx += 1;
                if gif.is_done() {
                    *frame_idx %= count;
                }
                gif.playback_idx
                    .store(*frame_idx, std::sync::atomic::Ordering::Relaxed);
                if let Some(frame) = gif.frame(*frame_idx) {
                    *deadline = now + Duration::from_millis(frame.delay_ms as u64);
                }
                any_advanced = true;
            }
        }
    }
    any_advanced
}

fn check_visible_images_ready(
    lines: &[Line],
    visible: &[usize],
    pending: &[crate::sixel::PendingImage],
    scroll_offset: usize,
    viewport_rows: u16,
) -> bool {
    let has_pending = lines.iter().any(|l| {
        matches!(
            l,
            Line::ImageRow {
                group: ImageGroup::PendingImage(_),
                ..
            }
        )
    });
    if !has_pending {
        return false;
    }
    let mut rows = 0u16;
    for &vi in &visible[scroll_offset..] {
        if rows >= viewport_rows {
            break;
        }
        if let Line::ImageRow {
            group: ImageGroup::PendingImage(id),
            row_in_group: 0,
            ..
        } = &lines[vi]
            && pending.get(*id).is_some_and(|p| p.is_ready())
        {
            return true;
        }
        rows += lines[vi].rows();
    }
    false
}

fn try_render_full_image(
    stdout: &mut io::Stdout,
    group: ImageGroup,
    row: u16,
    sixel_store: &[SixelData],
    pending_images: &[crate::sixel::PendingImage],
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &HashMap<usize, usize>,
) -> bool {
    crossterm::execute!(stdout, cursor::MoveTo(0, row)).unwrap();
    match group {
        ImageGroup::Sixel(id) => {
            if let Some(sd) = sixel_store.get(id) {
                write!(stdout, "{}", sd.data).unwrap();
                return true;
            }
        }
        ImageGroup::PendingImage(id) => {
            if let Some(p) = pending_images.get(id)
                && p.is_ready()
            {
                let sixel = p.wait();
                if !sixel.is_empty() {
                    write!(stdout, "{sixel}").unwrap();
                    return true;
                }
            }
        }
        ImageGroup::Gif(id) => {
            if let Some(gif) = gifs.get(id) {
                let frame_idx = gif_frames.get(&id).copied().unwrap_or(0);
                if let Some(frame) = gif.frame(frame_idx) {
                    write!(stdout, "{}", frame.sixel).unwrap();
                    return true;
                }
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn draw_screen(
    stdout: &mut io::Stdout,
    lines: &[Line],
    visible: &[usize],
    collapsed: &HashSet<usize>,
    sixel_store: &[SixelData],
    pending_images: &[crate::sixel::PendingImage],
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &HashMap<usize, usize>,
    video_paused: &HashSet<usize>,
    code_blocks: &[crate::renderer::CodeBlock],
    code_h_scroll: &HashMap<usize, usize>,
    scroll_offset: usize,
    viewport_rows: u16,
    term_cols: u16,
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
    let mut summary_rows: Vec<(u16, usize)> = Vec::new();
    let mut video_control_rows: Vec<(u16, usize)> = Vec::new();

    while vis_idx < visible.len() && rows_used < viewport_rows {
        let line_idx = visible[vis_idx];
        match &lines[line_idx] {
            Line::Text(text) => {
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                write!(stdout, "{text}\r").unwrap();
                rows_used += 1;
            }
            Line::ImageRow {
                group,
                row_in_group,
                total_rows,
                preview_text,
            } => {
                // Check if this is the first row AND all rows of the
                // group fit — if so, render the full sixel/gif.
                let render_full = *row_in_group == 0
                    && rows_used + total_rows <= viewport_rows
                    && try_render_full_image(
                        stdout,
                        *group,
                        rows_used,
                        sixel_store,
                        pending_images,
                        gifs,
                        gif_frames,
                    );

                if render_full {
                    // Skip the remaining ImageRows of this group
                    rows_used += total_rows;
                    vis_idx += 1;
                    // Advance past remaining rows of this group
                    while vis_idx < visible.len() {
                        let next = visible[vis_idx];
                        if let Line::ImageRow { group: g, .. } = &lines[next]
                            && g == group
                        {
                            vis_idx += 1;
                            continue;
                        }
                        break;
                    }
                    continue; // skip the vis_idx += 1 at bottom
                } else {
                    // Render one preview row
                    crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                    if preview_text.is_empty() {
                        write!(stdout, "\x1b[2m  [loading...]\x1b[0m\r").unwrap();
                    } else {
                        write!(stdout, "{preview_text}\x1b[0m\r").unwrap();
                    }
                    rows_used += 1;
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
            Line::CodeBlock { id, height } => {
                if let Some(block) = code_blocks.get(*id) {
                    let h_offset = code_h_scroll.get(id).copied().unwrap_or(0);
                    let avail_cols = term_cols as usize;

                    // Render each code line with horizontal scroll
                    for line in &block.lines {
                        if rows_used >= viewport_rows {
                            break;
                        }
                        crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                        let sliced =
                            crate::renderer::ansi::visible_slice(line, h_offset, avail_cols);
                        write!(stdout, "  {sliced}\x1b[0m\r").unwrap();
                        rows_used += 1;
                    }

                    // Scrollbar row
                    if rows_used < viewport_rows {
                        crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                        if block.max_width > avail_cols {
                            let bar = render_scrollbar(h_offset, block.max_width, avail_cols);
                            write!(stdout, "\x1b[2m{bar}\x1b[0m\r").unwrap();
                        }
                        rows_used += 1;
                    }
                } else {
                    rows_used += height;
                }
            }
            Line::VideoControls { gif_id } => {
                crossterm::execute!(stdout, cursor::MoveTo(0, rows_used)).unwrap();
                let paused = video_paused.contains(gif_id);
                let btn = if paused { "\u{25B6}" } else { "\u{23F8}" }; // ▶ or ⏸
                let frame_idx = gif_frames.get(gif_id).copied().unwrap_or(0);
                let cycle = gifs.get(*gif_id).map(|g| g.cycle_length()).unwrap_or(0);
                // Match the video's column width (from preview line length)
                let video_cols = gifs
                    .get(*gif_id)
                    .and_then(|g| g.preview.first())
                    .map(|p| crate::renderer::ansi::visible_len(p))
                    .unwrap_or(term_cols as usize);
                let bar_width = video_cols.saturating_sub(5); // " ⏸  " + " " = ~5 chars
                let progress_pos = if cycle > 1 && bar_width > 0 {
                    let pos_in_cycle = frame_idx % cycle;
                    (pos_in_cycle * bar_width / cycle).min(bar_width - 1)
                } else {
                    0
                };
                let mut bar = String::new();
                for i in 0..bar_width {
                    if i == progress_pos {
                        bar.push('\u{25CF}'); // ●
                    } else {
                        bar.push('\u{2500}'); // ─
                    }
                }
                write!(stdout, "\x1b[2m {btn}  {bar} \x1b[0m\r").unwrap();
                video_control_rows.push((rows_used, *gif_id));
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
    let cols = term_cols as usize;
    let status = format!(
        " [{progress}%]{watch_indicator} j/k:scroll  h/l:code  d/u:half  g/G:top/end  \
         enter:toggle  r:reload  q:quit"
    );
    // Truncate to terminal width to prevent wrapping
    let status: String = status.chars().take(cols).collect();
    crossterm::execute!(stdout, cursor::MoveTo(0, term_rows - 1)).unwrap();
    write!(stdout, "\x1b[7m{status:<width$}\x1b[0m", width = cols).unwrap();

    // End synchronized update — terminal flushes the buffered frame at once.
    write!(stdout, "\x1b[?2026l").unwrap();
    stdout.flush().unwrap();

    DrawResult {
        summary_rows,
        video_control_rows,
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

/// Find which code block (if any) occupies the given terminal row.
/// Find the first visible video control bar from scroll_offset.
fn first_visible_video_controls(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    for &vi in &visible[scroll_offset..] {
        if let Line::VideoControls { gif_id } = &lines[vi] {
            return Some(*gif_id);
        }
    }
    None
}

/// Find the first visible code block from scroll_offset.
fn first_visible_code_block(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    for &vi in &visible[scroll_offset..] {
        if let Line::CodeBlock { id, .. } = &lines[vi] {
            return Some(*id);
        }
    }
    None
}

fn find_code_block_at_row(
    lines: &[Line],
    visible: &[usize],
    _code_blocks: &[crate::renderer::CodeBlock],
    scroll_offset: usize,
    viewport_rows: u16,
    target_row: u16,
) -> Option<usize> {
    let mut rows_used: u16 = 0;
    for &vi in &visible[scroll_offset..] {
        if rows_used >= viewport_rows {
            break;
        }
        let line = &lines[vi];
        let line_rows = line.rows();
        if let Line::CodeBlock { id, .. } = line
            && target_row >= rows_used
            && target_row < rows_used + line_rows
        {
            return Some(*id);
        }
        rows_used += line_rows;
    }
    None
}

fn render_scrollbar(
    offset: usize,
    content_width: usize,
    view_width: usize,
) -> String {
    if content_width <= view_width {
        return String::new();
    }
    let bar_width = view_width.saturating_sub(2); // leave space for arrows
    if bar_width == 0 {
        return String::new();
    }

    // Thumb size proportional to visible portion
    let thumb_size = ((view_width as f64 / content_width as f64) * bar_width as f64)
        .ceil()
        .max(1.0) as usize;
    let max_offset = content_width.saturating_sub(view_width);
    let thumb_pos = if max_offset > 0 {
        ((offset as f64 / max_offset as f64) * (bar_width - thumb_size) as f64) as usize
    } else {
        0
    };

    let mut bar = String::with_capacity(view_width);
    bar.push('\u{25C0}'); // ◀
    for i in 0..bar_width {
        if i >= thumb_pos && i < thumb_pos + thumb_size {
            bar.push('\u{2588}'); // █
        } else {
            bar.push('\u{2500}'); // ─
        }
    }
    bar.push('\u{25B6}'); // ▶
    bar
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
