//! The Translate button's work: repaint the selected region in place.
//!
//! The screenshot dialog already holds the captured image, the selection and
//! the toolbar. This module takes one region of one captured output, runs the
//! shared OCR → detect → translate → overlay engine on it, and hands back the
//! repainted pixels. `screenshot.rs` blits them into the copy of the capture
//! that Save and Copy export, so no second window is involved.
//!
//! Nothing here implements OCR, colour sampling or text fitting: that is
//! `cosmic-screenshot-engine`, vendored from the cosmic-screenshot fork (see
//! `crates/VENDORED.md`). This file only knows about rectangles and pixels.

use std::path::PathBuf;
use std::sync::Arc;

use cosmic_screenshot_engine::{self as engine, lang};
use cosmic_screenshot_ocr as ocr;
use cosmic_screenshot_overlay::Canvas;
use cosmic_screenshot_translation::{LocalTranslator, Translator};
use image::RgbaImage;

/// A rectangle in the compositor's logical layout space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl LogicalRect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }

    pub fn height(self) -> i32 {
        self.bottom - self.top
    }

    /// The overlap of two rectangles, or `None` when they do not touch.
    pub fn intersect(self, other: Self) -> Option<Self> {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let right = self.right.min(other.right);
        let bottom = self.bottom.min(other.bottom);
        (left < right && top < bottom).then_some(Self {
            left,
            top,
            right,
            bottom,
        })
    }
}

/// The pixels of one captured output that Translate rewrites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn whole(image: (u32, u32)) -> Self {
        Self {
            x: 0,
            y: 0,
            width: image.0,
            height: image.1,
        }
    }
}

/// Translate a region and hand back the repainted pixels.
#[derive(Clone, Debug)]
pub struct Translated {
    /// Which captured output these pixels belong to.
    pub output: String,
    /// Where they came from, in that output's pixels.
    pub region: Region,
    /// The repainted region, same dimensions as `region`.
    pub image: Arc<RgbaImage>,
    pub target: &'static lang::LangInfo,
    /// "German", or "English (assumed)" when detection was not confident.
    pub source: String,
    /// Credible OCR lines handed to the renderer, and how many were actually
    /// drawn.
    pub drawn: usize,
    /// Lines Tesseract emitted, and how many survived the credibility filter.
    pub found: usize,
    pub kept: usize,
    /// Lines the engine returned non-empty text for.
    pub translated: usize,
    /// The capture was already in the target language; nothing was painted.
    pub same_language: bool,
    /// The opt-in `COSMIC_SCREENSHOT_DEBUG_DIR` run directory, when enabled.
    pub debug_dir: Option<PathBuf>,
}

/// Where a selection sits inside one output's captured pixels.
///
/// Uses the same scaling as `combined_image` in `screenshot.rs`, so the area
/// Translate repaints is exactly the area Capture would export.
pub fn region_for_selection(
    selection: LogicalRect,
    output: LogicalRect,
    image: (u32, u32),
) -> Option<Region> {
    if output.width() <= 0 || output.height() <= 0 || image.0 == 0 || image.1 == 0 {
        return None;
    }
    let overlap = selection.intersect(output)?;
    let scale_x = image.0 as f64 / output.width() as f64;
    let scale_y = image.1 as f64 / output.height() as f64;

    let x = ((overlap.left - output.left) as f64 * scale_x).max(0.0) as u32;
    let y = ((overlap.top - output.top) as f64 * scale_y).max(0.0) as u32;
    let width = (overlap.width() as f64 * scale_x).max(0.0) as u32;
    let height = (overlap.height() as f64 * scale_y).max(0.0) as u32;

    if width == 0 || height == 0 {
        return None;
    }
    Some(clamp_region(
        Region {
            x,
            y,
            width,
            height,
        },
        image,
    ))
}

/// Keep a region inside the image, so the crop can never panic.
pub fn clamp_region(region: Region, image: (u32, u32)) -> Region {
    if image.0 == 0 || image.1 == 0 {
        return Region {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let x = region.x.min(image.0 - 1);
    let y = region.y.min(image.1 - 1);
    Region {
        x,
        y,
        width: region.width.min(image.0 - x).max(1),
        height: region.height.min(image.1 - y).max(1),
    }
}

/// Run the engine on one region, with the configured translator.
///
/// The translator is built (and health-checked) before any OCR work, so a
/// stopped sidecar costs a fraction of a second and never touches the image.
pub fn run(
    output: String,
    source: RgbaImage,
    region: Region,
    target: &'static lang::LangInfo,
) -> Result<Translated, String> {
    let translator = LocalTranslator::from_env().map_err(|error| error.to_string())?;
    translate_with(output, &source, region, target, &translator)
}

fn translate_with(
    output: String,
    source: &RgbaImage,
    region: Region,
    target: &'static lang::LangInfo,
    translator: &dyn Translator,
) -> Result<Translated, String> {
    let region = clamp_region(region, source.dimensions());
    if region.width == 0 || region.height == 0 {
        return Err("the selected area is empty — nothing to translate".to_string());
    }
    let cropped =
        image::imageops::crop_imm(source, region.x, region.y, region.width, region.height)
            .to_image();

    // Tesseract reads a path, not a buffer. The temporary is removed when it
    // goes out of scope and never lands in the user's Pictures directory.
    let file = tempfile::Builder::new()
        .prefix("portal-translate-")
        .suffix(".png")
        .tempfile()
        .map_err(|error| format!("could not prepare the OCR input: {error}"))?;
    cropped
        .save(file.path())
        .map_err(|error| format!("could not prepare the OCR input: {error}"))?;

    let request = engine::Request::new(
        lang::translation_lang(target),
        ocr::Config::default().with_languages(lang::all_ocr_languages()),
    );
    let mut canvas = Canvas::new(cropped.width(), cropped.height(), cropped.into_raw());
    let applied = engine::translate_canvas(&mut canvas, &request, translator, &|config| {
        ocr::recognize_reading(file.path(), config)
    })
    .map_err(|error| error.to_string())?;

    let image = RgbaImage::from_raw(canvas.width, canvas.height, canvas.rgba)
        .ok_or_else(|| "the translated image has inconsistent dimensions".to_string())?;

    Ok(Translated {
        output,
        region,
        image: Arc::new(image),
        target,
        source: applied.source_label(),
        drawn: applied.report.drawn,
        found: applied.found,
        kept: applied.kept,
        translated: applied.translated,
        same_language: applied.same_language,
        debug_dir: applied.debug_dir,
    })
}

/// Put the repainted region back onto the untouched capture.
pub fn composite(base: &RgbaImage, translated: &Translated) -> RgbaImage {
    let mut out = base.clone();
    image::imageops::overlay(
        &mut out,
        translated.image.as_ref(),
        translated.region.x as i64,
        translated.region.y as i64,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> LogicalRect {
        LogicalRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_selection_maps_to_pixels_at_2x() {
        let region = region_for_selection(
            rect(100, 50, 300, 150),
            rect(0, 0, 1920, 1080),
            (3840, 2160),
        )
        .expect("a selection inside the output");
        assert_eq!(
            region,
            Region {
                x: 200,
                y: 100,
                width: 400,
                height: 200
            }
        );
    }

    #[test]
    fn a_selection_is_clipped_to_its_output() {
        // The output starts at x=1920; the drag started on the screen to its
        // left. Only the part inside this output belongs to its pixels.
        let region = region_for_selection(
            rect(1800, 10, 2000, 110),
            rect(1920, 0, 3840, 1080),
            (1920, 1080),
        )
        .expect("the overlap is non-empty");
        assert_eq!(region.x, 0);
        assert_eq!(region.width, 80);
        assert_eq!(region.y, 10);
    }

    #[test]
    fn a_selection_on_another_output_has_no_region() {
        assert_eq!(
            region_for_selection(
                rect(0, 0, 100, 100),
                rect(1920, 0, 3840, 1080),
                (1920, 1080)
            ),
            None
        );
    }

    #[test]
    fn an_empty_selection_has_no_region() {
        assert_eq!(
            region_for_selection(rect(10, 10, 10, 40), rect(0, 0, 800, 600), (800, 600)),
            None
        );
    }

    #[test]
    fn a_region_never_leaves_the_image() {
        let image = (100, 50);
        let clamped = clamp_region(
            Region {
                x: 90,
                y: 40,
                width: 40,
                height: 40,
            },
            image,
        );
        assert_eq!(
            clamped,
            Region {
                x: 90,
                y: 40,
                width: 10,
                height: 10
            }
        );
        // Enough for a crop of the whole (tiny) image, never zero-sized.
        assert_eq!(
            clamp_region(
                Region {
                    x: 5,
                    y: 5,
                    width: 0,
                    height: 0
                },
                image
            )
            .width,
            1
        );
    }

    #[test]
    fn compositing_puts_the_translation_back_where_it_came_from() {
        let base = RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 255]));
        let patch = RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        let translated = Translated {
            output: "DP-1".into(),
            region: Region {
                x: 3,
                y: 4,
                width: 2,
                height: 3,
            },
            image: Arc::new(patch),
            target: lang::find("de").expect("German is in the table"),
            source: "English".into(),
            drawn: 1,
            found: 1,
            kept: 1,
            translated: 1,
            same_language: false,
            debug_dir: None,
        };

        let out = composite(&base, &translated);

        assert_eq!(out.get_pixel(3, 4), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(out.get_pixel(4, 6), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(
            out.get_pixel(2, 4),
            &image::Rgba([0, 0, 0, 255]),
            "outside the region the capture is untouched"
        );
        assert_eq!(
            base.get_pixel(3, 4),
            &image::Rgba([0, 0, 0, 255]),
            "the pristine capture is never modified in place"
        );
    }

    #[test]
    fn a_stopped_sidecar_is_one_line_and_never_touches_the_image() {
        // A closed port: the health check fails before OCR runs, so this does
        // not need Tesseract or the model.
        unsafe {
            std::env::set_var("COSMIC_SCREENSHOT_NLLB_URL", "http://127.0.0.1:9");
        }
        let source = RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        let error = run(
            "DP-1".into(),
            source.clone(),
            Region::whole((20, 20)),
            lang::find("de").expect("German is in the table"),
        )
        .unwrap_err();
        unsafe {
            std::env::remove_var("COSMIC_SCREENSHOT_NLLB_URL");
        }

        assert!(
            error.starts_with("translation engine not available:"),
            "{error}"
        );
        assert_eq!(
            error.lines().count(),
            1,
            "the dialog shows exactly one line"
        );
        assert_eq!(
            source,
            RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255])),
            "a failed translation must leave the capture alone"
        );
    }

    /// A capture with readable text, drawn with the same overlay crate the
    /// engine paints with, so the test needs no binary fixture.
    fn text_capture() -> Option<RgbaImage> {
        use cosmic_screenshot_overlay::{
            FontSet, Options as OverlayOptions, Rect as OverlayRect, Script, TranslatedBlock,
            overlay as paint,
        };

        let fonts = FontSet::system(Script::Latin).ok()?;
        let mut canvas = Canvas::filled(420, 80, [255, 255, 255, 255]);
        paint(
            &mut canvas,
            &[TranslatedBlock {
                rect: OverlayRect::new(12.0, 24.0, 300.0, 30.0),
                text: "Hello world".to_string(),
            }],
            &fonts,
            &OverlayOptions::default(),
        );
        RgbaImage::from_raw(canvas.width, canvas.height, canvas.rgba)
    }

    #[test]
    fn the_whole_pipeline_repaints_the_region_and_leaves_the_rest_alone() {
        let tesseract = std::process::Command::new("tesseract")
            .arg("--version")
            .output();
        if tesseract.is_err() {
            eprintln!("skipping: tesseract is not installed");
            return;
        }
        let Some(source) = text_capture() else {
            eprintln!("skipping: no system font for Latin text");
            return;
        };

        let translator = cosmic_screenshot_translation::testing::FakeTranslator::new("Hallo");
        let target = lang::find("de").expect("German is in the table");
        let done = translate_with(
            "DP-1".into(),
            &source,
            Region::whole(source.dimensions()),
            target,
            &translator,
        )
        .expect("the pipeline runs end to end");

        let calls = translator.calls();
        assert_eq!(calls.len(), 1, "one batched request");
        assert!(
            calls[0].texts.iter().any(|text| text.contains("Hello")),
            "Tesseract reads the drawn line: {:?}",
            calls[0].texts
        );
        assert!(done.kept >= 1);
        assert_eq!(done.drawn, done.kept, "every line is repainted");
        assert_eq!(done.region, Region::whole(source.dimensions()));

        let composited = composite(&source, &done);
        assert_ne!(
            composited.as_raw(),
            source.as_raw(),
            "the capture the dialog exports must have changed"
        );
        assert_eq!(
            composited.get_pixel(400, 72),
            &image::Rgba([255, 255, 255, 255]),
            "pixels outside the recognised text stay untouched"
        );
    }
}
