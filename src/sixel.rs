use a_sixel::dither::Sierra;
use image::RgbaImage;

type Encoder = a_sixel::BitMergeSixelEncoderBest<Sierra>;

/// Encode an RGBA pixel buffer as a sixel string using a-sixel.
///
/// `pixels` is row-major RGBA u8 data (4 bytes per pixel).
pub fn encode_rgba(
    width: u32,
    height: u32,
    pixels: &[u8],
) -> String {
    let img =
        RgbaImage::from_raw(width, height, pixels.to_vec()).expect("invalid pixel buffer size");
    Encoder::encode(img)
}

/// Load an image file and encode it as sixel, scaling to fit within
/// `max_width` pixels wide.
pub fn encode_image_file(
    path: &std::path::Path,
    max_width: u32,
) -> Option<String> {
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
    Some(Encoder::encode(img))
}
