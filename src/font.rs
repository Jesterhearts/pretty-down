use std::hash::BuildHasher;
use std::hash::Hasher;
use std::hash::RandomState;

use raqote::DrawOptions;
use raqote::DrawTarget;
use raqote::PathBuilder;
use raqote::SolidSource;
use raqote::Source;
use raqote::Transform;
use rustybuzz::Face;
use rustybuzz::ttf_parser::GlyphId;
use rustybuzz::ttf_parser::RgbaColor;

/// A parsed font face used for text rasterization.
#[allow(dead_code)]
pub struct Font<'a> {
    face: Face<'a>,
    advance: f32,
    id: u64,
}

impl<'a> Font<'a> {
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write(data);

        Face::from_slice(data, 0).map(|face| {
            let advance = face
                .glyph_hor_advance(face.glyph_index('m').unwrap_or_default())
                .unwrap_or_default() as f32;
            Self {
                face,
                advance,
                id: hasher.finish(),
            }
        })
    }

    pub fn face(&self) -> &Face<'_> {
        &self.face
    }

    /// Compute the pixel width of a single character at the given height.
    #[allow(dead_code)]
    pub fn char_width(
        &self,
        height_px: u32,
    ) -> u32 {
        let scale = height_px as f32 / self.face.height() as f32;
        (self.advance * scale) as u32
    }
}

// ---------------------------------------------------------------------------
// Outline builder: converts font glyph outlines into raqote paths
// ---------------------------------------------------------------------------

struct Outline {
    path: PathBuilder,
}

impl Default for Outline {
    fn default() -> Self {
        Self {
            path: PathBuilder::new(),
        }
    }
}

impl Outline {
    fn finish(self) -> raqote::Path {
        self.path.finish()
    }
}

impl rustybuzz::ttf_parser::OutlineBuilder for Outline {
    fn move_to(
        &mut self,
        x: f32,
        y: f32,
    ) {
        self.path.move_to(x, y);
    }

    fn line_to(
        &mut self,
        x: f32,
        y: f32,
    ) {
        self.path.line_to(x, y);
    }

    fn quad_to(
        &mut self,
        x1: f32,
        y1: f32,
        x: f32,
        y: f32,
    ) {
        self.path.quad_to(x1, y1, x, y);
    }

    fn curve_to(
        &mut self,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        x: f32,
        y: f32,
    ) {
        self.path.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.path.close();
    }
}

// ---------------------------------------------------------------------------
// Color glyph painter (COLR table support)
// ---------------------------------------------------------------------------

struct Painter<'f, 'd, 'p> {
    font: &'f Face<'d>,
    target: &'f mut DrawTarget<&'p mut [u32]>,
    outline: Option<raqote::Path>,
    skew: Transform,
    scale: f32,
    y_offset: f32,
    x_offset: f32,
    transforms: Vec<rustybuzz::ttf_parser::Transform>,
}

impl<'f, 'd, 'p> Painter<'f, 'd, 'p> {
    fn new(
        font: &'f Face<'d>,
        target: &'f mut DrawTarget<&'p mut [u32]>,
        skew: Transform,
        scale: f32,
        y_offset: f32,
        x_offset: f32,
    ) -> Self {
        Self {
            font,
            target,
            outline: None,
            skew,
            scale,
            y_offset,
            x_offset,
            transforms: vec![],
        }
    }

    fn compute_transform(&self) -> Transform {
        self.transforms
            .iter()
            .rev()
            .fold(Transform::default(), |tfm, t| {
                tfm.then(&Transform::new(t.a, t.b, t.c, t.d, t.e, t.f))
            })
            .then_scale(self.scale, -self.scale)
            .then(&self.skew)
            .then_translate((self.x_offset, self.y_offset).into())
    }
}

impl<'a> rustybuzz::ttf_parser::colr::Painter<'a> for Painter<'_, '_, '_> {
    fn outline_glyph(
        &mut self,
        glyph_id: rustybuzz::ttf_parser::GlyphId,
    ) {
        let mut outline = Outline::default();
        self.outline = self
            .font
            .outline_glyph(glyph_id, &mut outline)
            .map(|_| outline.finish());
    }

    fn paint(
        &mut self,
        paint: rustybuzz::ttf_parser::colr::Paint<'a>,
    ) {
        let paint = match paint {
            rustybuzz::ttf_parser::colr::Paint::Solid(color) => {
                Source::Solid(SolidSource::from_unpremultiplied_argb(
                    color.alpha,
                    color.red,
                    color.green,
                    color.blue,
                ))
            }
            _ => {
                // For simplicity, treat complex gradient paints as white
                Source::Solid(SolidSource::from_unpremultiplied_argb(255, 255, 255, 255))
            }
        };

        let draw_options = DrawOptions {
            antialias: raqote::AntialiasMode::None,
            ..Default::default()
        };

        self.target.set_transform(&Transform::default());
        if let Some(outline) = self.outline.take() {
            let outline = outline.transform(&self.compute_transform());
            self.target.fill(&outline, &paint, &draw_options);
        } else {
            self.target.fill_rect(
                0.,
                0.,
                self.target.width() as f32,
                self.target.height() as f32,
                &paint,
                &draw_options,
            );
        }
    }

    fn push_clip(&mut self) {
        self.target.set_transform(&self.compute_transform());
        self.target.push_clip(
            &self
                .outline
                .take()
                .unwrap_or_else(|| PathBuilder::new().finish()),
        );
    }

    fn push_clip_box(
        &mut self,
        clipbox: rustybuzz::ttf_parser::colr::ClipBox,
    ) {
        let transform = self.compute_transform();
        let xy0 = transform.transform_point((clipbox.x_min, clipbox.y_min).into());
        let xy1 = transform.transform_point((clipbox.x_max, clipbox.y_max).into());
        let xy2 = transform.transform_point((clipbox.x_min, clipbox.y_max).into());
        let xy3 = transform.transform_point((clipbox.x_max, clipbox.y_min).into());
        let min_xy = xy0.min(xy1).min(xy2).min(xy3);
        let max_xy = xy0.max(xy1).max(xy2).max(xy3);

        self.target.push_clip_rect(raqote::IntRect {
            min: min_xy.to_i32(),
            max: max_xy.to_i32(),
        });
    }

    fn pop_clip(&mut self) {
        self.target.pop_clip();
    }

    fn push_layer(
        &mut self,
        mode: rustybuzz::ttf_parser::colr::CompositeMode,
    ) {
        use rustybuzz::ttf_parser::colr::CompositeMode::*;
        self.target.push_layer_with_blend(
            1.0,
            match mode {
                Clear => raqote::BlendMode::Clear,
                Source => raqote::BlendMode::Src,
                Destination => raqote::BlendMode::Dst,
                SourceOver => raqote::BlendMode::SrcOver,
                DestinationOver => raqote::BlendMode::DstOver,
                SourceIn => raqote::BlendMode::SrcIn,
                DestinationIn => raqote::BlendMode::DstIn,
                SourceOut => raqote::BlendMode::SrcOut,
                DestinationOut => raqote::BlendMode::DstOut,
                SourceAtop => raqote::BlendMode::SrcAtop,
                DestinationAtop => raqote::BlendMode::DstAtop,
                Xor => raqote::BlendMode::Xor,
                Plus => raqote::BlendMode::Add,
                Screen => raqote::BlendMode::Screen,
                Overlay => raqote::BlendMode::Overlay,
                Darken => raqote::BlendMode::Darken,
                Lighten => raqote::BlendMode::Lighten,
                ColorDodge => raqote::BlendMode::ColorDodge,
                ColorBurn => raqote::BlendMode::ColorBurn,
                HardLight => raqote::BlendMode::HardLight,
                SoftLight => raqote::BlendMode::SoftLight,
                Difference => raqote::BlendMode::Difference,
                Exclusion => raqote::BlendMode::Exclusion,
                Multiply => raqote::BlendMode::Multiply,
                Hue => raqote::BlendMode::Hue,
                Saturation => raqote::BlendMode::Saturation,
                Color => raqote::BlendMode::Color,
                Luminosity => raqote::BlendMode::Luminosity,
            },
        );
    }

    fn pop_layer(&mut self) {
        self.target.pop_layer();
    }

    fn push_transform(
        &mut self,
        transform: rustybuzz::ttf_parser::Transform,
    ) {
        self.transforms.push(transform);
    }

    fn pop_transform(&mut self) {
        self.transforms.pop();
    }
}

// ---------------------------------------------------------------------------
// Text rendering to pixel buffer
// ---------------------------------------------------------------------------

/// Render a string of text into an RGBA pixel buffer.
///
/// Returns `(width, height, pixels)` where pixels is row-major RGBA u8 data.
/// The `color` is `[r, g, b]`.
pub fn render_text(
    font: &Font,
    text: &str,
    height_px: u32,
    color: [u8; 3],
) -> (u32, u32, Vec<u8>) {
    if text.is_empty() {
        return (0, 0, vec![]);
    }

    // Shape the text
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    let plan = rustybuzz::ShapePlan::new(
        font.face(),
        rustybuzz::Direction::LeftToRight,
        Some(rustybuzz::script::LATIN),
        None,
        &[],
    );
    let output = rustybuzz::shape_with_plan(font.face(), &plan, buffer);

    let scale = height_px as f32 / font.face().height() as f32;

    // Compute total width from glyph advances
    let total_advance: f32 = output
        .glyph_positions()
        .iter()
        .map(|p| p.x_advance as f32 * scale)
        .sum();

    let img_width = (total_advance.ceil() as u32).max(1);
    let img_height = height_px;

    // Render at 2x for anti-aliasing, then downscale
    let render_w = img_width * 2;
    let render_h = img_height * 2;
    let render_scale = scale * 2.0;

    let mut pixels = vec![0u32; render_w as usize * render_h as usize];

    let y_offset = font.face().ascender() as f32 * render_scale;

    let mut x_cursor = 0.0f32;
    let glyph_infos = output.glyph_infos();
    let glyph_positions = output.glyph_positions();

    for (info, pos) in glyph_infos.iter().zip(glyph_positions.iter()) {
        let glyph_id = GlyphId(info.glyph_id as u16);
        let x_off = x_cursor + pos.x_offset as f32 * render_scale;
        let y_off = y_offset + pos.y_offset as f32 * render_scale;

        // Try color glyph first
        let mut target =
            DrawTarget::from_backing(render_w as i32, render_h as i32, &mut pixels[..]);

        let mut painter = Painter::new(
            font.face(),
            &mut target,
            Transform::default(),
            render_scale,
            y_off,
            x_off,
        );

        if font
            .face()
            .paint_color_glyph(
                glyph_id,
                0,
                RgbaColor::new(255, 255, 255, 255),
                &mut painter,
            )
            .is_none()
        {
            // Render outline glyph
            let mut outline = Outline::default();
            if font.face().outline_glyph(glyph_id, &mut outline).is_some() {
                let path = outline.finish();
                let mut target =
                    DrawTarget::from_backing(render_w as i32, render_h as i32, &mut pixels[..]);
                target.set_transform(
                    &Transform::scale(render_scale, -render_scale)
                        .then_translate((x_off, y_off).into()),
                );
                target.fill(
                    &path,
                    &Source::Solid(SolidSource::from_unpremultiplied_argb(255, 255, 255, 255)),
                    &DrawOptions::default(),
                );
            }
        }

        x_cursor += pos.x_advance as f32 * render_scale;
    }

    // Downscale 2x → 1x with bilinear filtering, and convert to RGBA u8 with
    // the requested color
    let mut output = vec![0u8; img_width as usize * img_height as usize * 4];
    for y in 0..img_height {
        for x in 0..img_width {
            let sx = x as usize * 2;
            let sy = y as usize * 2;
            let rw = render_w as usize;

            // Average 2x2 block
            let samples = [
                pixels[sy * rw + sx],
                pixels[sy * rw + sx + 1],
                pixels[(sy + 1) * rw + sx],
                pixels[(sy + 1) * rw + sx + 1],
            ];

            // raqote stores as ARGB premultiplied in native endian
            let mut alpha_sum = 0u32;
            for s in &samples {
                let [a, _r, _g, _b] = s.to_be_bytes();
                alpha_sum += a as u32;
            }
            let alpha = (alpha_sum / 4) as u8;

            let idx = (y as usize * img_width as usize + x as usize) * 4;
            if alpha > 0 {
                output[idx] = color[0];
                output[idx + 1] = color[1];
                output[idx + 2] = color[2];
                output[idx + 3] = alpha;
            }
        }
    }

    (img_width, img_height, output)
}
