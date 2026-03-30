use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use a_sixel::dither::NoDither;
use a_sixel::dither::Sierra;
use image::RgbaImage;

type ImageEncoder = a_sixel::BitMergeSixelEncoderBest<Sierra>;
type TextEncoder = a_sixel::BitSixelEncoder<NoDither>;

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

/// Query the terminal's pixel width.
/// Falls back to 800px if unavailable.
pub fn terminal_pixel_width() -> u32 {
    static WIDTH: OnceLock<u32> = OnceLock::new();
    *WIDTH.get_or_init(|| {
        if let Ok(ws) = crossterm::terminal::window_size()
            && ws.width > 0
        {
            return ws.width as u32;
        }
        800
    })
}

/// Convert a pixel height to terminal rows using the actual cell height.
pub fn pixel_height_to_rows(pixel_height: u32) -> u16 {
    let cell_h = cell_pixel_height();
    pixel_height.div_ceil(cell_h).max(1) as u16
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

/// Scale an image to fit within max_width.
fn scale_image(
    img: RgbaImage,
    max_width: u32,
) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w > max_width {
        let new_h = (h as f64 * max_width as f64 / w as f64) as u32;
        image::imageops::resize(
            &img,
            max_width,
            new_h,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    }
}

/// A handle to an image being encoded to sixel in a background thread.
pub struct PendingImage {
    result: Arc<OnceLock<String>>,
    /// Estimated terminal rows this image will occupy.
    pub estimated_rows: u16,
}

impl PendingImage {
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

/// Load an image file and start encoding to sixel in a background thread.
pub fn encode_image_file_async(
    path: &std::path::Path,
    max_width: u32,
) -> Option<PendingImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let img = scale_image(img, max_width);

    let estimated_rows = pixel_height_to_rows(img.height());

    let result = Arc::new(OnceLock::new());
    let result_clone = result.clone();

    std::thread::spawn(move || {
        let encoded = ImageEncoder::encode(img);
        let _ = result_clone.set(encoded);
    });

    Some(PendingImage {
        result,
        estimated_rows,
    })
}

/// Synchronous image encoding (used by block HTML).
pub fn encode_image_file(
    path: &std::path::Path,
    max_width: u32,
) -> Option<String> {
    encode_image_file_async(path, max_width).map(|p| p.wait().to_string())
}

/// A single encoded GIF frame.
pub struct GifFrame {
    pub sixel: String,
    /// Delay before the next frame, in milliseconds.
    pub delay_ms: u32,
}

/// A handle to a GIF being decoded and encoded in a background thread.
pub struct PendingGif {
    /// Frames encoded so far. The thread appends to this as it goes.
    frames: Arc<Mutex<Vec<GifFrame>>>,
    /// Set to true when all frames are encoded.
    done: Arc<OnceLock<()>>,
    /// Estimated terminal rows.
    pub estimated_rows: u16,
}

impl PendingGif {
    /// Number of frames encoded so far.
    pub fn frame_count(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    /// Whether all frames have been encoded.
    pub fn is_done(&self) -> bool {
        self.done.get().is_some()
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
            let sixel = ImageEncoder::encode(img);
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
        estimated_rows,
    })
}
