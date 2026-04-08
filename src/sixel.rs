use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use a_sixel::dither::NoDither;
use a_sixel::dither::Sierra;
use a_sixel::dither::Sobol;
use image::RgbaImage;

type ImageEncoder = a_sixel::BitMergeSixelEncoderBest<Sierra>;
type TextEncoder = a_sixel::BitSixelEncoder<NoDither>;
/// Fast encoder for animated content (GIFs, videos) where speed > quality.
type AnimEncoder = a_sixel::BitSixelEncoder<Sobol>;

/// Query the terminal's cell pixel height.
/// Falls back to 20px if the query fails or returns 0.
pub fn cell_pixel_height() -> u32 {
    static CELL_HEIGHT: OnceLock<u32> = OnceLock::new();
    *CELL_HEIGHT.get_or_init(|| {
        if let Ok(ws) = crossterm::terminal::window_size()
            && ws.height > 0
            && ws.rows > 0
        {
            return (ws.height / ws.rows) as u32;
        }
        20 // fallback
    })
}

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

/// Query the terminal's cell pixel width. Falls back to 8px.
pub fn cell_pixel_width() -> u32 {
    static CELL_WIDTH: OnceLock<u32> = OnceLock::new();
    *CELL_WIDTH.get_or_init(|| {
        if let Ok(ws) = crossterm::terminal::window_size()
            && ws.width > 0
            && ws.columns > 0
        {
            return ws.width as u32 / ws.columns as u32;
        }
        8
    })
}

/// Compute the preview column width for an image of the given pixel width.
pub fn preview_columns(pixel_width: u32) -> u32 {
    (pixel_width / cell_pixel_width()).max(1)
}

static CACHED_PIXEL_WIDTH: AtomicU32 = AtomicU32::new(0);

/// Query the terminal's pixel width. Cached; call `invalidate_terminal_size()`
/// on resize events to refresh.
pub fn terminal_pixel_width() -> u32 {
    let cached = CACHED_PIXEL_WIDTH.load(Ordering::Relaxed);
    if cached > 0 {
        return cached;
    }
    let width = query_pixel_width();
    CACHED_PIXEL_WIDTH.store(width, Ordering::Relaxed);
    width
}

/// Invalidate the cached terminal pixel width (call on resize).
pub fn invalidate_terminal_size() {
    CACHED_PIXEL_WIDTH.store(0, Ordering::Relaxed);
}

fn query_pixel_width() -> u32 {
    if let Ok(ws) = crossterm::terminal::window_size()
        && ws.width > 0
    {
        return ws.width as u32;
    }
    800
}

/// Convert a pixel height to terminal rows using the actual cell height.
pub fn pixel_height_to_rows(pixel_height: u32) -> u16 {
    let cell_h = cell_pixel_height();
    pixel_height.div_ceil(cell_h).max(1) as u16
}

/// Round a pixel height up to the nearest cell boundary.
/// Sixel images rendered at this height will occupy exactly
/// `pixel_height_to_rows(h)` terminal rows with no overflow.
pub fn snap_height_to_cells(pixel_height: u32) -> u32 {
    let cell_h = cell_pixel_height();
    pixel_height.div_ceil(cell_h) * cell_h
}

/// Encode an RGBA pixel buffer as a sixel string, optimized for
/// single-color rendered text (fast, no dithering).
pub fn encode_rgba(
    width: u32,
    height: u32,
    pixels: &[u8],
) -> String {
    let img =
        RgbaImage::from_raw(width, height, pixels.to_vec()).expect("invalid pixel buffer size");
    TextEncoder::encode(img)
}

/// Cap pixel width for sixel encoding — higher resolution just wastes CPU
/// without visible improvement in terminal output.
const MAX_SIXEL_WIDTH: u32 = 800;

pub fn scale_image(
    img: RgbaImage,
    max_width: u32,
) -> RgbaImage {
    let cap = max_width.min(MAX_SIXEL_WIDTH);
    let (w, h) = img.dimensions();
    if w > cap {
        let new_h = (h as f64 * cap as f64 / w as f64) as u32;
        image::imageops::resize(&img, cap, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    }
}

/// Check if a path looks like an SVG file.
pub fn is_svg(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// Render an SVG file to an RGBA image, scaling to fit `max_width` pixels.
pub fn render_svg_file(
    path: &std::path::Path,
    max_width: u32,
) -> Option<RgbaImage> {
    let data = std::fs::read(path).ok()?;
    render_svg_bytes(&data, max_width)
}

/// Render SVG from raw bytes to an RGBA image, scaling down to fit
/// `max_width` pixels if the SVG is wider. Small SVGs are rendered at
/// their intrinsic size.
pub fn render_svg_bytes(
    data: &[u8],
    max_width: u32,
) -> Option<RgbaImage> {
    let mut opts = resvg::usvg::Options::default();

    // Load system fonts so <text> elements render correctly.
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    opts.fontdb = std::sync::Arc::new(fontdb);

    let tree = resvg::usvg::Tree::from_data(data, &opts).ok()?;
    let svg_size = tree.size();

    // Only scale down, never up — prevents tiny badges from being blown up.
    let cap = max_width.min(MAX_SIXEL_WIDTH);
    let intrinsic_w = svg_size.width() as u32;
    let width = intrinsic_w.min(cap);
    let scale = width as f32 / svg_size.width();
    let height = (svg_size.height() * scale).max(1.0) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny_skia uses premultiplied RGBA; convert to straight RGBA
    let mut pixels = pixmap.take();
    for chunk in pixels.chunks_exact_mut(4) {
        let a = chunk[3] as f32 / 255.0;
        if a > 0.0 {
            chunk[0] = (chunk[0] as f32 / a).min(255.0) as u8;
            chunk[1] = (chunk[1] as f32 / a).min(255.0) as u8;
            chunk[2] = (chunk[2] as f32 / a).min(255.0) as u8;
        }
    }

    RgbaImage::from_raw(width, height, pixels)
}

/// A handle to an image being encoded to sixel in a background thread.
pub struct PendingImage {
    result: Arc<OnceLock<String>>,
    /// Estimated terminal rows (may be updated by background thread for URL
    /// images).
    estimated_rows: Arc<std::sync::atomic::AtomicU16>,
    /// Half-block preview (may be updated by background thread for URL images).
    preview: Arc<Mutex<Vec<String>>>,
}

impl PendingImage {
    /// Create a PendingImage with the given dimensions and preview.
    pub fn new(
        result: Arc<OnceLock<String>>,
        estimated_rows: u16,
        preview: Vec<String>,
    ) -> Self {
        Self {
            result,
            estimated_rows: Arc::new(std::sync::atomic::AtomicU16::new(estimated_rows)),
            preview: Arc::new(Mutex::new(preview)),
        }
    }

    /// Create a PendingImage for a URL download where dimensions are unknown.
    /// Returns the image plus handles the background thread can use to update
    /// the estimated rows and preview once the download completes.
    pub fn new_deferred(
        result: Arc<OnceLock<String>>
    ) -> (
        Self,
        Arc<std::sync::atomic::AtomicU16>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let rows = Arc::new(std::sync::atomic::AtomicU16::new(1));
        let preview = Arc::new(Mutex::new(vec![]));
        let img = Self {
            result,
            estimated_rows: rows.clone(),
            preview: preview.clone(),
        };
        (img, rows, preview)
    }

    pub fn estimated_rows(&self) -> u16 {
        self.estimated_rows
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn preview(&self) -> Vec<String> {
        self.preview.lock().map(|p| p.clone()).unwrap_or_default()
    }

    /// Check if encoding is complete.
    pub fn is_ready(&self) -> bool {
        self.result.get().is_some()
    }

    /// Get the encoded sixel data, blocking until ready.
    pub fn wait(&self) -> &str {
        while !self.is_ready() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.result.get().unwrap()
    }
}

/// Generate a half-block (▀) preview of an image.
///
/// Each character cell represents 2 vertical pixels: foreground = top pixel,
/// background = bottom pixel. The image is downscaled with nearest-neighbor
/// to fit `cols` columns × `rows` character rows.
pub fn half_block_preview(
    img: &RgbaImage,
    cols: u32,
    rows: u16,
) -> Vec<String> {
    if cols == 0 || rows == 0 || img.width() == 0 || img.height() == 0 {
        return vec![];
    }

    let pixel_rows = rows as u32 * 2; // 2 pixel rows per character row
    let mut lines = Vec::with_capacity(rows as usize);

    for row in 0..rows as u32 {
        let mut line = String::new();
        for col in 0..cols {
            // Map to source pixel coordinates (nearest neighbor)
            let sx = (col * img.width() / cols).min(img.width() - 1);
            let sy_top = (row * 2 * img.height() / pixel_rows).min(img.height() - 1);
            let sy_bot = ((row * 2 + 1) * img.height() / pixel_rows).min(img.height() - 1);

            let top = img.get_pixel(sx, sy_top);
            let bot = img.get_pixel(sx, sy_bot);

            // Skip fully transparent pixels
            if top[3] == 0 && bot[3] == 0 {
                line.push(' ');
                continue;
            }

            line.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m\u{2580}",
                top[0], top[1], top[2], bot[0], bot[1], bot[2]
            ));
        }
        line.push_str("\x1b[0m");
        lines.push(line);
    }

    lines
}

/// Generate a half-block preview from raw RGBA pixel data.
pub fn preview_from_pixels(
    pixels: &[u8],
    w: u32,
    h: u32,
    estimated_rows: u16,
) -> Vec<String> {
    if let Some(img) = RgbaImage::from_raw(w, h, pixels.to_vec()) {
        half_block_preview(&img, preview_columns(w), estimated_rows)
    } else {
        vec![]
    }
}

/// Load an image file and start encoding to sixel in a background thread.
pub fn encode_image_file_async(
    path: &std::path::Path,
    max_width: u32,
) -> Option<PendingImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let img = scale_image(img, max_width);

    let estimated_rows = pixel_height_to_rows(img.height());

    let preview = half_block_preview(&img, preview_columns(img.width()), estimated_rows);

    let result = Arc::new(OnceLock::new());
    let result_clone = result.clone();

    std::thread::spawn(move || {
        let encoded = ImageEncoder::encode(img);
        let _ = result_clone.set(encoded);
    });

    Some(PendingImage::new(result, estimated_rows, preview))
}

/// A single encoded GIF frame.
pub struct GifFrame {
    pub sixel: String,
    /// Delay before the next frame, in milliseconds.
    pub delay_ms: u32,
}

/// A handle to a GIF/video being decoded and encoded in a background thread.
pub struct PendingGif {
    /// Frames encoded so far. The thread appends to this as it goes.
    frames: Arc<Mutex<Vec<GifFrame>>>,
    /// Set to true when all frames are encoded (or first loop complete for
    /// videos).
    done: Arc<OnceLock<()>>,
    /// Number of frames in one full cycle. Set after the first loop completes.
    /// For GIFs this equals frame_count() once done. For videos this is set
    /// by the thread when it reaches the end of the first pass.
    pub cycle_len: Arc<std::sync::atomic::AtomicUsize>,
    /// The index of the frame currently being displayed by the pager.
    pub playback_idx: Arc<std::sync::atomic::AtomicUsize>,
    /// Estimated terminal rows.
    pub estimated_rows: u16,
    /// Half-block preview of the first frame.
    pub preview: Vec<String>,
    /// Whether this is a video (has playback controls) vs a GIF (loops
    /// silently).
    pub is_video: bool,
}

impl PendingGif {
    /// Number of frames encoded so far.
    pub fn frame_count(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    /// Whether all frames have been encoded (or first loop done for videos).
    pub fn is_done(&self) -> bool {
        self.done.get().is_some()
    }

    /// Number of frames in one full playback cycle (0 if not yet known).
    pub fn cycle_length(&self) -> usize {
        let v = self.cycle_len.load(std::sync::atomic::Ordering::Relaxed);
        if v > 0 {
            v
        } else {
            // Not known yet — use frame_count as estimate
            self.frame_count()
        }
    }

    /// Get the sixel data for a specific frame (if available).
    pub fn frame(
        &self,
        idx: usize,
    ) -> Option<GifFrame> {
        let frames = self.frames.lock().unwrap();
        frames.get(idx).map(|f| GifFrame {
            sixel: f.sixel.clone(),
            delay_ms: f.delay_ms,
        })
    }
}

/// Load a GIF file and start encoding all frames in a background thread.
/// Returns `None` if the file is not a valid GIF.
pub fn encode_gif_async(
    path: &std::path::Path,
    max_width: u32,
) -> Option<PendingGif> {
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let decoder = GifDecoder::new(reader).ok()?;
    let raw_frames: Vec<_> = decoder.into_frames().collect();

    if raw_frames.is_empty() {
        return None;
    }

    // Get dimensions from the first frame to estimate rows
    let first = raw_frames.first()?.as_ref().ok()?;
    let first_buf = first.buffer();
    let scaled = scale_image(first_buf.clone(), max_width);
    let estimated_rows = pixel_height_to_rows(scaled.height());

    let preview = half_block_preview(&scaled, preview_columns(scaled.width()), estimated_rows);

    let frames = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(OnceLock::new());
    let frames_clone = frames.clone();
    let done_clone = done.clone();

    std::thread::spawn(move || {
        for frame_result in raw_frames {
            let Ok(frame): Result<image::Frame, _> = frame_result else {
                continue;
            };
            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms: u32 = if denom == 0 {
                100
            } else {
                (numer / denom).max(10)
            };
            let img = scale_image(frame.into_buffer(), max_width);
            let sixel = AnimEncoder::encode(img);
            frames_clone
                .lock()
                .unwrap()
                .push(GifFrame { sixel, delay_ms });
        }
        let _ = done_clone.set(());
    });

    Some(PendingGif {
        frames,
        done,
        cycle_len: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        playback_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        estimated_rows,
        preview,
        is_video: false,
    })
}

/// Check if a path looks like a video file (by extension).
pub fn is_video(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|ext| {
        matches!(
            ext.to_ascii_lowercase().to_str(),
            Some("mp4" | "webm" | "mkv" | "avi" | "mov" | "m4v" | "ogv")
        )
    })
}

/// Extract an RGBA image from an ffmpeg frame, accounting for stride padding.
fn frame_to_rgba(
    frame: &ffmpeg_next::frame::Video,
    width: u32,
    height: u32,
) -> Option<RgbaImage> {
    let data = frame.data(0);
    let stride = frame.stride(0);
    let row_bytes = width as usize * 4;

    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        let end = start + row_bytes;
        if end > data.len() {
            return None;
        }
        pixels.extend_from_slice(&data[start..end]);
    }

    RgbaImage::from_raw(width, height, pixels)
}

/// Decode the first video frame and generate a half-block preview.
fn decode_first_frame_preview(
    path: &std::path::Path,
    dst_width: u32,
    dst_height: u32,
    estimated_rows: u16,
) -> Vec<String> {
    use ffmpeg_next as ffmpeg;

    let try_decode = || -> Option<Vec<String>> {
        let mut input = ffmpeg::format::input(path).ok()?;
        let stream = input.streams().best(ffmpeg::media::Type::Video)?;
        let stream_index = stream.index();
        let codec_params = stream.parameters();

        let mut decoder = ffmpeg::codec::context::Context::from_parameters(codec_params)
            .ok()?
            .decoder()
            .video()
            .ok()?;

        // Scaler is created lazily after first frame, using the frame's
        // actual pixel format (which may differ from decoder defaults).
        let mut scaler: Option<ffmpeg::software::scaling::context::Context> = None;

        for (s, packet) in input.packets() {
            if s.index() != stream_index {
                continue;
            }
            let _ = decoder.send_packet(&packet);
            let mut decoded = ffmpeg::frame::Video::empty();
            if decoder.receive_frame(&mut decoded).is_ok() {
                // Create scaler from the actual frame format
                let sws = scaler.get_or_insert_with(|| {
                    ffmpeg::software::scaling::context::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::format::Pixel::RGBA,
                        dst_width,
                        dst_height,
                        ffmpeg::software::scaling::Flags::BILINEAR,
                    )
                    .expect("scaler")
                });
                let mut rgb_frame = ffmpeg::frame::Video::empty();
                if sws.run(&decoded, &mut rgb_frame).is_err() {
                    continue;
                }
                if let Some(img) = frame_to_rgba(&rgb_frame, dst_width, dst_height) {
                    return Some(half_block_preview(
                        &img,
                        preview_columns(dst_width),
                        estimated_rows,
                    ));
                }
            }
        }
        None
    };

    try_decode().unwrap_or_default()
}

/// Load a video file and start decoding/encoding frames in a background thread.
/// Returns `None` if the file doesn't look like a video or can't be opened.
/// Probes dimensions and decodes the first frame for a half-block preview
/// on the main thread (fast), then spawns a background thread for full
/// frame-by-frame encoding.
pub fn encode_video_async(
    path: &std::path::Path,
    max_width: u32,
) -> Option<PendingGif> {
    use ffmpeg_next as ffmpeg;

    if !is_video(path) {
        return None;
    }

    ffmpeg::init().ok()?;

    // Probe dimensions and frame rate
    let input = ffmpeg::format::input(path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;

    let codec_params = stream.parameters();
    let decoder = ffmpeg::codec::context::Context::from_parameters(codec_params)
        .ok()?
        .decoder()
        .video()
        .ok()?;

    let src_width = decoder.width();
    let src_height = decoder.height();
    let (dst_width, dst_height) = if src_width > max_width {
        let h = (src_height as f64 * max_width as f64 / src_width as f64) as u32;
        (max_width, h)
    } else {
        (src_width, src_height)
    };

    let estimated_rows = pixel_height_to_rows(dst_height);

    // Decode first frame for half-block preview
    let preview = decode_first_frame_preview(path, dst_width, dst_height, estimated_rows);

    let frames = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(OnceLock::new());
    let cycle_len = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let playback_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frames_clone = frames.clone();
    let done_clone = done.clone();
    let cycle_len_clone = cycle_len.clone();
    let playback_clone = playback_idx.clone();
    let path = path.to_owned();

    std::thread::spawn(move || {
        let Ok(mut input) = ffmpeg::format::input(&path) else {
            let _ = done_clone.set(());
            return;
        };

        let Some(stream) = input.streams().best(ffmpeg::media::Type::Video) else {
            let _ = done_clone.set(());
            return;
        };
        let stream_index = stream.index();

        let rate = stream.avg_frame_rate();
        let delay_ms = if rate.numerator() > 0 && rate.denominator() > 0 {
            (1000 * rate.denominator() as u64 / rate.numerator() as u64) as u32
        } else {
            33
        };

        let codec_params = stream.parameters();
        let Ok(ctx) = ffmpeg::codec::context::Context::from_parameters(codec_params) else {
            let _ = done_clone.set(());
            return;
        };
        let Ok(mut decoder) = ctx.decoder().video() else {
            let _ = done_clone.set(());
            return;
        };

        // Scaler created lazily from actual frame format
        let mut scaler: Option<ffmpeg::software::scaling::context::Context> = None;

        const MAX_AHEAD: usize = 30;

        for (s, packet) in input.packets() {
            if s.index() != stream_index {
                continue;
            }
            let _ = decoder.send_packet(&packet);
            let mut decoded = ffmpeg::frame::Video::empty();
            while decoder.receive_frame(&mut decoded).is_ok() {
                let sws = scaler.get_or_insert_with(|| {
                    ffmpeg::software::scaling::context::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        ffmpeg::format::Pixel::RGBA,
                        dst_width,
                        dst_height,
                        ffmpeg::software::scaling::Flags::BILINEAR,
                    )
                    .expect("scaler")
                });
                let mut rgb_frame = ffmpeg::frame::Video::empty();
                if sws.run(&decoded, &mut rgb_frame).is_err() {
                    continue;
                }
                if let Some(img) = frame_to_rgba(&rgb_frame, dst_width, dst_height) {
                    // Throttle: wait if we're too far ahead of playback
                    while frames_clone.lock().unwrap().len()
                        > playback_clone.load(std::sync::atomic::Ordering::Relaxed) + MAX_AHEAD
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    let sixel = AnimEncoder::encode(img);
                    frames_clone
                        .lock()
                        .unwrap()
                        .push(GifFrame { sixel, delay_ms });
                }
            }
        }

        // Record cycle length after encoding completes.
        let count = frames_clone.lock().unwrap().len();
        if count > 0 {
            cycle_len_clone.store(count, std::sync::atomic::Ordering::Relaxed);
        }

        let _ = done_clone.set(());
    });

    Some(PendingGif {
        frames,
        done,
        cycle_len,
        playback_idx,
        estimated_rows,
        preview,
        is_video: true,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    // ── is_svg ───────────────────────────────────────────────────

    #[test]
    fn is_svg_recognizes_svg() {
        assert!(is_svg(Path::new("image.svg")));
        assert!(is_svg(Path::new("/path/to/file.SVG")));
        assert!(is_svg(Path::new("chart.Svg")));
    }

    #[test]
    fn is_svg_rejects_non_svg() {
        assert!(!is_svg(Path::new("image.png")));
        assert!(!is_svg(Path::new("noext")));
        assert!(!is_svg(Path::new("svg"))); // no extension
    }

    // ── is_video ─────────────────────────────────────────────────

    #[test]
    fn is_video_recognizes_video_formats() {
        for ext in &["mp4", "webm", "mkv", "avi", "mov", "m4v", "ogv"] {
            assert!(
                is_video(Path::new(&format!("video.{ext}"))),
                "should recognize .{ext}"
            );
        }
    }

    #[test]
    fn is_video_case_insensitive() {
        assert!(is_video(Path::new("video.MP4")));
        assert!(is_video(Path::new("clip.MKV")));
    }

    #[test]
    fn is_video_rejects_non_video() {
        assert!(!is_video(Path::new("image.png")));
        assert!(!is_video(Path::new("doc.pdf")));
        assert!(!is_video(Path::new("noext")));
    }

    // ── scale_image ──────────────────────────────────────────────

    #[test]
    fn scale_image_no_resize_when_small() {
        let img = RgbaImage::new(100, 50);
        let result = scale_image(img, 200);
        assert_eq!(result.dimensions(), (100, 50));
    }

    #[test]
    fn scale_image_downscales_when_wider() {
        let img = RgbaImage::new(1600, 800);
        let result = scale_image(img, 400);
        assert_eq!(result.width(), 400);
        // Height should be proportionally scaled
        assert_eq!(result.height(), 200);
    }

    #[test]
    fn scale_image_capped_at_max_sixel_width() {
        let img = RgbaImage::new(2000, 1000);
        // Even though max_width is 1500, the MAX_SIXEL_WIDTH cap (800) wins
        let result = scale_image(img, 1500);
        assert_eq!(result.width(), 800);
    }

    // ── half_block_preview ───────────────────────────────────────

    #[test]
    fn half_block_preview_empty_on_zero_dims() {
        let img = RgbaImage::new(10, 10);
        assert!(half_block_preview(&img, 0, 1).is_empty());
        assert!(half_block_preview(&img, 1, 0).is_empty());
    }

    #[test]
    fn half_block_preview_produces_correct_row_count() {
        let img = RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 255]));
        let lines = half_block_preview(&img, 5, 3);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn half_block_preview_transparent_pixels_are_spaces() {
        // Fully transparent image
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 0]));
        let lines = half_block_preview(&img, 4, 2);
        for line in &lines {
            // Should contain spaces (not half-block chars) before the reset
            let before_reset = line.strip_suffix("\x1b[0m").unwrap_or(line);
            assert!(before_reset.chars().all(|c| c == ' '));
        }
    }

    // ── encode_rgba ──────────────────────────────────────────────

    #[test]
    fn encode_rgba_produces_sixel() {
        // Small 2x2 red image
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let sixel = encode_rgba(2, 2, &pixels);
        // Sixel data starts with DCS escape
        assert!(sixel.starts_with("\x1bP"));
    }
}
