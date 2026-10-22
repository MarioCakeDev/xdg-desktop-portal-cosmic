//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at b39cc37678c31ac715f22ccd49d1bdba39cc04cd (crates/overlay).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! Paint translated text over the original glyphs, in place.
//!
//! The approach is the one Google Lens uses: for each recognised line, sample
//! the colour of the paper around it, cover the line's box with that colour,
//! then draw the translation inside the same box at the largest size that fits.
//! Everything is plain pixel work — no compositor, no shaders — so the result
//! is deterministic and testable, and the same buffer is what gets saved.

use std::fmt;
use std::path::Path;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};

pub mod color;
pub mod layout;

use layout::Fitted;

/// A rectangle in image pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn expand(&self, amount: f32) -> Self {
        Self {
            x: self.x - amount,
            y: self.y - amount,
            width: self.width + amount * 2.0,
            height: self.height + amount * 2.0,
        }
    }

    pub fn inset(&self, amount: f32) -> Self {
        self.expand(-amount)
    }

    /// Intersection with the image bounds.
    pub fn clamp(&self, width: u32, height: u32) -> Self {
        let left = self.x.max(0.0);
        let top = self.y.max(0.0);
        let right = self.right().min(width as f32);
        let bottom = self.bottom().min(height as f32);
        Self {
            x: left,
            y: top,
            width: (right - left).max(0.0),
            height: (bottom - top).max(0.0),
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// One line of translated text and the box the original occupied.
#[derive(Clone, Debug, PartialEq)]
pub struct TranslatedBlock {
    pub rect: Rect,
    pub text: String,
}

/// The script of the *target* language, used to pick a font that has glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    Latin,
    Cyrillic,
    Greek,
    Cjk,
    Korean,
    Arabic,
}

impl Script {
    /// Font families to try, best first. System fonts only: no font is bundled.
    pub fn families(self) -> &'static [&'static str] {
        match self {
            Script::Latin | Script::Cyrillic | Script::Greek => &[
                "Noto Sans",
                "DejaVu Sans",
                "Liberation Sans",
                "Ubuntu",
                "Cantarell",
                "Arial",
            ],
            Script::Cjk => &[
                "Noto Sans CJK SC",
                "Noto Sans CJK JP",
                "Noto Sans CJK",
                "Noto Sans SC",
                "WenQuanYi Zen Hei",
                "Droid Sans Fallback",
            ],
            Script::Korean => &[
                "Noto Sans CJK KR",
                "Noto Sans KR",
                "Noto Sans CJK",
                "Droid Sans Fallback",
            ],
            Script::Arabic => &[
                "Noto Sans Arabic",
                "Noto Naskh Arabic",
                "Noto Sans",
                "DejaVu Sans",
            ],
        }
    }
}

/// A resolved font for one script.
pub struct FontSet {
    pub script: Script,
    pub family: String,
    font: FontArc,
}

impl FontSet {
    /// Resolve a font from the system font database.
    pub fn system(script: Script) -> Result<Self, OverlayError> {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();

        for family in script.families() {
            let families = [fontdb::Family::Name(family)];
            let query = fontdb::Query {
                families: &families,
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = database.query(&query)
                && let Some(font) = load_face(&database, id)
            {
                return Ok(Self {
                    script,
                    family: (*family).to_string(),
                    font,
                });
            }
        }

        // Last resort: any sans-serif face at all.
        let families = [fontdb::Family::SansSerif];
        let query = fontdb::Query {
            families: &families,
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        if let Some(id) = database.query(&query)
            && let Some(font) = load_face(&database, id)
        {
            return Ok(Self {
                script,
                family: "sans-serif".to_string(),
                font,
            });
        }

        Err(OverlayError::NoFont { script })
    }

    /// Use a specific font file (tests, or a user-supplied font).
    pub fn from_file(path: &Path, script: Script) -> Result<Self, OverlayError> {
        let data = std::fs::read(path).map_err(|error| OverlayError::Io(error.to_string()))?;
        let font =
            FontArc::try_from_vec(data).map_err(|error| OverlayError::Font(error.to_string()))?;
        Ok(Self {
            script,
            family: path.display().to_string(),
            font,
        })
    }
}

fn load_face(database: &fontdb::Database, id: fontdb::ID) -> Option<FontArc> {
    database
        .with_face_data(id, |data, _index| FontArc::try_from_vec(data.to_vec()))
        .and_then(Result::ok)
}

/// RGBA8 image buffer.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn filled(width: u32, height: u32, pixel: [u8; 4]) -> Self {
        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            rgba.extend_from_slice(&pixel);
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn open(path: &Path) -> Result<Self, OverlayError> {
        let image = image::open(path).map_err(|error| OverlayError::Image(error.to_string()))?;
        let rgba = image.to_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), OverlayError> {
        let buffer = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| OverlayError::Image("buffer size does not match dimensions".into()))?;
        buffer
            .save(path)
            .map_err(|error| OverlayError::Image(error.to_string()))
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y as usize) * (self.width as usize) + x as usize) * 4;
        [
            self.rgba[offset],
            self.rgba[offset + 1],
            self.rgba[offset + 2],
            self.rgba[offset + 3],
        ]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, pixel: [u8; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = ((y as usize) * (self.width as usize) + x as usize) * 4;
        self.rgba[offset..offset + 4].copy_from_slice(&pixel);
    }

    fn blend(&mut self, x: u32, y: u32, color: [u8; 4], coverage: f32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let alpha = (coverage.clamp(0.0, 1.0) * (color[3] as f32 / 255.0)).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }
        let [sr, sg, sb, _] = color;
        let offset = ((y as usize) * (self.width as usize) + x as usize) * 4;
        for (channel, source) in [sr, sg, sb].into_iter().enumerate() {
            let destination = self.rgba[offset + channel] as f32;
            let blended = source as f32 * alpha + destination * (1.0 - alpha);
            self.rgba[offset + channel] = blended.round().clamp(0.0, 255.0) as u8;
        }
        self.rgba[offset + 3] = 255;
    }
}

/// Knobs for the overlay.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Minimum cover margin past the recognised box, in pixels.
    pub cover_padding: f32,
    /// Extra horizontal cover margin, as a fraction of the line height.
    pub cover_padding_x_ratio: f32,
    /// Extra vertical cover margin, as a fraction of the line height. Tesseract
    /// boxes are tight around the glyphs; the em box of the original line is
    /// taller, and the cover has to hide all of it.
    pub cover_padding_y_ratio: f32,
    /// Inner padding between the cover edge and the text.
    pub text_padding: f32,
    pub min_font_px: f32,
    pub max_font_px: f32,
    /// Line advance as a multiple of the font size.
    pub line_height: f32,
    /// Minimum luminance difference for a pixel to count as text colour.
    pub min_contrast: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cover_padding: 3.0,
            cover_padding_x_ratio: 0.12,
            cover_padding_y_ratio: 0.22,
            text_padding: 2.0,
            min_font_px: 8.0,
            max_font_px: 48.0,
            line_height: 1.18,
            min_contrast: 40.0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Blocks handed to the renderer.
    pub blocks: usize,
    /// Blocks with drawn translated text.
    pub drawn: usize,
    /// Blocks whose translation was empty, so only covered.
    pub covered_only: usize,
    /// Blocks outside the image or too small to touch.
    pub skipped: usize,
    pub max_font_px: f32,
}

#[derive(Debug)]
pub enum OverlayError {
    Io(String),
    Image(String),
    Font(String),
    NoFont { script: Script },
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlayError::Io(message) => write!(f, "could not read or write the image: {message}"),
            OverlayError::Image(message) => write!(f, "could not decode the image: {message}"),
            OverlayError::Font(message) => write!(f, "could not load a font: {message}"),
            OverlayError::NoFont { script } => write!(
                f,
                "no system font for {script:?} text; install fonts-noto-core (and fonts-noto-cjk for Chinese/Japanese/Korean)"
            ),
        }
    }
}

impl std::error::Error for OverlayError {}

/// The area that gets painted for one block: the recognised box plus enough
/// margin to hide the original glyphs' ascenders, descenders and antialiasing.
pub fn cover_rect(rect: Rect, options: &Options, width: u32, height: u32) -> Rect {
    let padding_x = options
        .cover_padding
        .max(rect.height * options.cover_padding_x_ratio);
    let padding_y = options
        .cover_padding
        .max(rect.height * options.cover_padding_y_ratio);
    Rect::new(
        rect.x - padding_x,
        rect.y - padding_y,
        rect.width + padding_x * 2.0,
        rect.height + padding_y * 2.0,
    )
    .clamp(width, height)
}

/// Cover every block's original text and draw the translation in its place.
pub fn overlay(
    canvas: &mut Canvas,
    blocks: &[TranslatedBlock],
    fonts: &FontSet,
    options: &Options,
) -> Report {
    let mut report = Report {
        blocks: blocks.len(),
        ..Report::default()
    };

    for block in blocks {
        let cover = cover_rect(block.rect, options, canvas.width, canvas.height);
        if cover.width < 2.0 || cover.height < 2.0 {
            report.skipped += 1;
            continue;
        }

        // Sample colours before the original glyphs are painted over. The pair
        // is checked against WCAG contrast here, not left to chance: a box that
        // spans two papers used to get a mid-grey cover and light ink on it.
        let (background, foreground) = color::pair(canvas, cover, options.min_contrast);
        fill(canvas, cover, background);

        if block.text.trim().is_empty() {
            report.covered_only += 1;
            continue;
        }

        let inner = cover.inset(options.text_padding);
        let (inner, clip) = if inner.width >= 4.0 && inner.height >= 4.0 {
            (inner, cover)
        } else {
            (cover, cover)
        };

        let fitted = layout::fit(
            &block.text,
            &fonts.font,
            inner.width,
            inner.height,
            options.min_font_px,
            options.max_font_px,
            options.line_height,
        );

        let top = inner.y + ((inner.height - fitted.height) / 2.0).max(0.0);
        draw(canvas, &fitted, inner.x, top, &fonts.font, foreground, clip);
        report.drawn += 1;
        report.max_font_px = report.max_font_px.max(fitted.size);
    }

    report
}

fn fill(canvas: &mut Canvas, rect: Rect, color: [u8; 4]) {
    let left = rect.x.floor().max(0.0) as u32;
    let top = rect.y.floor().max(0.0) as u32;
    let right = rect.right().ceil().min(canvas.width as f32).max(0.0) as u32;
    let bottom = rect.bottom().ceil().min(canvas.height as f32).max(0.0) as u32;
    for y in top..bottom {
        for x in left..right {
            canvas.set_pixel(x, y, color);
        }
    }
}

fn draw(
    canvas: &mut Canvas,
    fitted: &Fitted,
    left: f32,
    top: f32,
    font: &FontArc,
    color: [u8; 4],
    clip: Rect,
) {
    let scaled = font.as_scaled(PxScale::from(fitted.size));
    let ascent = scaled.ascent();

    for (index, line) in fitted.lines.iter().enumerate() {
        let baseline = top + ascent + index as f32 * fitted.line_height;
        let mut pen = left;
        for character in line.chars() {
            let glyph_id = scaled.glyph_id(character);
            if let Some(outline) = font.outline_glyph(scaled.scaled_glyph(character)) {
                let bounds = outline.px_bounds();
                let origin_x = pen + bounds.min.x;
                let origin_y = baseline + bounds.min.y;
                outline.draw(|offset_x, offset_y, coverage| {
                    let x = origin_x + offset_x as f32;
                    let y = origin_y + offset_y as f32;
                    if clip.contains(x, y) {
                        canvas.blend(x as u32, y as u32, color, coverage);
                    }
                });
            }
            pen += scaled.h_advance(glyph_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_helpers() {
        let rect = Rect::new(10.0, 10.0, 20.0, 5.0);
        assert_eq!(rect.expand(2.0), Rect::new(8.0, 8.0, 24.0, 9.0));
        assert_eq!(rect.inset(2.0), Rect::new(12.0, 12.0, 16.0, 1.0));
        assert_eq!(
            Rect::new(-5.0, -5.0, 20.0, 20.0).clamp(100, 100),
            Rect::new(0.0, 0.0, 15.0, 15.0)
        );
        assert!(rect.contains(10.0, 10.0));
        assert!(!rect.contains(30.0, 10.0));
    }

    #[test]
    fn solid_canvas_round_trips_through_pixels() {
        let mut canvas = Canvas::filled(4, 4, [1, 2, 3, 255]);
        assert_eq!(canvas.pixel(3, 3), [1, 2, 3, 255]);
        canvas.set_pixel(0, 0, [9, 9, 9, 255]);
        assert_eq!(canvas.pixel(0, 0), [9, 9, 9, 255]);
        canvas.set_pixel(99, 99, [0, 0, 0, 0]); // out of bounds is a no-op
        assert_eq!(canvas.pixel(1, 1), [1, 2, 3, 255]);
    }
}
