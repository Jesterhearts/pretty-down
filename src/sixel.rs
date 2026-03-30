use std::sync::Arc;
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
///
/// Returns `None` if the image can't be loaded. The actual sixel encoding
/// happens on a spawned thread; call `.wait()` to get the result.
pub fn encode_image_file_async(
    path: &std::path::Path,
    max_width: u32,
) -> Option<PendingImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let img = if w > max_width {
        let new_h = (h as f64 * max_width as f64 / w as f64) as u32;
        image::imageops::resize(
            &img,
            max_width,
            new_h,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

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

/// Synchronous image encoding (used by block HTML where we need the result
/// immediately).
pub fn encode_image_file(
    path: &std::path::Path,
    max_width: u32,
) -> Option<String> {
    encode_image_file_async(path, max_width).map(|p| p.wait().to_string())
}
