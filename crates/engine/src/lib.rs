//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at c89de943f3def0cf0884152afc148a6308526949 (crates/engine).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! The translate-the-capture pipeline: OCR, auto-detect, translate, overlay.
//!
//! This crate is the single copy of that logic. Every front end — the
//! `cosmic-screenshot-translate` CLI and the screenshot dialog in
//! `xdg-desktop-portal-cosmic` — builds a [`Request`] and calls
//! [`translate_canvas`]. Neither of them owns an OCR two-pass decision, a
//! language table or an overlay call of its own, so the two cannot drift.
//!
//! The caller supplies the recognizer as a closure. Both front ends happen to
//! use Tesseract through [`cosmic_screenshot_ocr`], but keeping it a parameter
//! means the pipeline can be tested without Tesseract installed.

use std::fmt;

use cosmic_screenshot_ocr::{self as ocr, TextBlock};
use cosmic_screenshot_overlay::{self as overlay, Canvas, FontSet, Options, Rect, TranslatedBlock};
use cosmic_screenshot_translation::{Detected, Lang, TranslateError, Translator};

pub mod lang;

/// Everything the pipeline needs to know before it looks at the pixels.
#[derive(Clone, Debug)]
pub struct Request {
    pub target: Lang,
    /// Explicit source language; `None` means detect it from the capture.
    pub source: Option<Lang>,
    pub ocr: ocr::Config,
    /// Re-read the capture with the detected language's pack alone. Off when
    /// the caller chose the OCR packs itself.
    pub two_pass_ocr: bool,
    pub options: Options,
}

impl Request {
    /// A request that detects the source and uses the full pack list.
    pub fn new(target: Lang, ocr: ocr::Config) -> Self {
        Self {
            target,
            source: None,
            ocr,
            two_pass_ocr: true,
            options: Options::default(),
        }
    }
}

/// What one run did, in the terms a status line needs.
#[derive(Clone, Debug, Default)]
pub struct Applied {
    /// Lines handed to the renderer.
    pub blocks: usize,
    pub report: overlay::Report,
    pub detected_source: Option<Lang>,
    /// The source language came from a low-confidence fallback, not detection.
    pub source_assumed: bool,
}

impl Applied {
    /// "German", "English (assumed)", or "the source language" — one phrase for
    /// the notification and the dialog status line.
    pub fn source_label(&self) -> String {
        source_label(self.detected_source, self.source_assumed)
    }
}

pub fn source_label(detected: Option<Lang>, assumed: bool) -> String {
    match (detected, assumed) {
        (Some(lang), true) => format!("{} (assumed)", lang.english_name()),
        (Some(lang), false) => lang.english_name().to_string(),
        (None, _) => "the source language".to_string(),
    }
}

#[derive(Debug)]
pub enum Error {
    Ocr(ocr::OcrError),
    Translation(TranslateError),
    Overlay(overlay::OverlayError),
    NoText,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ocr(error) => write!(f, "{error}"),
            Error::Translation(error) => write!(f, "{error}"),
            Error::Overlay(error) => write!(f, "{error}"),
            Error::NoText => {
                f.write_str("no text found in the captured area — nothing to translate")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Recognise, translate and paint the result back onto `canvas`, in place.
///
/// `recognize` reads the image the caller prepared; the pipeline calls it twice
/// when it has to detect the source language first.
pub fn translate_canvas(
    canvas: &mut Canvas,
    request: &Request,
    translator: &dyn Translator,
    recognize: &dyn Fn(&ocr::Config) -> Result<Vec<TextBlock>, ocr::OcrError>,
) -> Result<Applied, Error> {
    let (blocks, source) = recognize_blocks(request, translator, recognize)?;
    apply(
        canvas,
        &blocks,
        request.target,
        source,
        translator,
        &request.options,
    )
}

/// The packs for a confirmation OCR pass, or `None` when there is no point.
///
/// All packs at once is the only way to read text whose language is unknown,
/// but it is worse at the language it eventually turns out to be: a 24-pack
/// Tesseract reads `für` as `fur` and `Rücksendungen` as `Ricksendungen`. Once
/// detection has named the language, re-read the capture with that pack alone.
/// A low-confidence fallback is not a detection, and a language with no row in
/// [`lang::LANGS`] has no pack to name.
pub fn confirmation_languages(detected: Detected) -> Option<&'static str> {
    if detected.assumed {
        return None;
    }
    lang::find(detected.lang.code()).map(|info| info.tesseract)
}

/// OCR the capture, with the second pass when the source has to be detected.
fn recognize_blocks(
    request: &Request,
    translator: &dyn Translator,
    recognize: &dyn Fn(&ocr::Config) -> Result<Vec<TextBlock>, ocr::OcrError>,
) -> Result<(Vec<TextBlock>, Option<Lang>), Error> {
    let blocks = recognize(&request.ocr).map_err(Error::Ocr)?;
    if !request.two_pass_ocr || request.source.is_some() || blocks.is_empty() {
        return Ok((blocks, request.source));
    }

    let texts: Vec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
    let detected = translator
        .detect_source(&texts)
        .map_err(Error::Translation)?;
    let Some(languages) = detected.and_then(confirmation_languages) else {
        return Ok((blocks, None));
    };

    let narrowed = request.ocr.clone().with_languages(languages);
    // A narrower pack can find nothing, or be missing from the install; the
    // first pass already suffices then. A mixed-script capture can come back
    // partial: the other script's lines keep their original pixels, which is
    // safe but leaves them untranslated.
    match recognize(&narrowed) {
        Ok(second) if !second.is_empty() => Ok((second, detected.map(|detected| detected.lang))),
        _ => Ok((blocks, None)),
    }
}

/// Translate already-recognised blocks and paint them onto `canvas`.
///
/// Separate from [`translate_canvas`] so the overlay step can be exercised
/// without Tesseract.
pub fn apply(
    canvas: &mut Canvas,
    blocks: &[TextBlock],
    target: Lang,
    source: Option<Lang>,
    translator: &dyn Translator,
    options: &Options,
) -> Result<Applied, Error> {
    if blocks.is_empty() {
        return Err(Error::NoText);
    }
    let texts: Vec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
    let batch = translator
        .translate(&texts, target, source)
        .map_err(Error::Translation)?;
    if batch.items.len() != blocks.len() {
        return Err(Error::Translation(TranslateError::CountMismatch {
            requested: blocks.len(),
            received: batch.items.len(),
        }));
    }

    let detected_source = batch.detected_source();
    let source_assumed = batch.source_assumed;
    let translated: Vec<TranslatedBlock> = blocks
        .iter()
        .zip(batch.items)
        .map(|(block, item)| TranslatedBlock {
            rect: rect_of(block),
            text: item.text,
        })
        .collect();

    let script = lang::find(target.code())
        .map(|info| info.script)
        .unwrap_or(overlay::Script::Latin);
    let fonts = FontSet::system(script).map_err(Error::Overlay)?;
    let report = overlay::overlay(canvas, &translated, &fonts, options);

    Ok(Applied {
        blocks: blocks.len(),
        report,
        detected_source,
        source_assumed,
    })
}

fn rect_of(block: &TextBlock) -> Rect {
    Rect::new(
        block.bbox.x as f32,
        block.bbox.y as f32,
        block.bbox.width as f32,
        block.bbox.height as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_screenshot_ocr::Bbox;
    use cosmic_screenshot_translation::testing::FakeTranslator;

    fn detected(code: &str, assumed: bool) -> Detected {
        Detected {
            lang: Lang::parse(code).expect("test language"),
            assumed,
        }
    }

    fn line(text: &str) -> TextBlock {
        TextBlock {
            bbox: Bbox {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            text: text.to_string(),
            confidence: 95.0,
        }
    }

    fn request() -> Request {
        Request::new(
            Lang::parse("en").unwrap(),
            ocr::Config::default().with_languages(lang::all_ocr_languages()),
        )
    }

    /// Run `recognize_blocks` with a fake recognizer, recording each `-l` list.
    fn passes(
        request: &Request,
        translator: &FakeTranslator,
        second: Result<Vec<TextBlock>, ocr::OcrError>,
    ) -> (Vec<TextBlock>, Option<Lang>, Vec<String>) {
        let configs = std::cell::RefCell::new(Vec::new());
        let second = std::cell::RefCell::new(Some(second));
        let result = recognize_blocks(request, translator, &|config| {
            let first = configs.borrow().is_empty();
            configs.borrow_mut().push(config.languages.clone());
            if first {
                Ok(vec![line("für")])
            } else {
                second
                    .borrow_mut()
                    .take()
                    .expect("the second pass runs at most once")
            }
        });
        let (blocks, source) = result.expect("recognizing succeeds");
        (blocks, source, configs.into_inner())
    }

    #[test]
    fn the_second_pass_re_reads_with_the_detected_pack_alone() {
        let request = request();
        let all = request.ocr.languages.clone();
        let translator = FakeTranslator::new("").with_detection("de");

        let (blocks, source, configs) = passes(&request, &translator, Ok(vec![line("für")]));

        assert_eq!(configs, vec![all, "deu".to_string()]);
        assert_eq!(source.map(Lang::code), Some("de"));
        assert_eq!(blocks[0].text, "für");
    }

    #[test]
    fn an_assumed_detection_leaves_the_single_pass_alone() {
        let request = request();
        let translator = FakeTranslator::new("").assuming_the_source_on_detection("en");

        let (_, source, configs) = passes(&request, &translator, Ok(vec![line("x")]));

        assert_eq!(configs.len(), 1, "an assumed language must not re-read");
        assert_eq!(source, None, "the sidecar detects again when translating");
    }

    #[test]
    fn an_explicit_source_skips_detection_entirely() {
        let mut request = request();
        request.source = Some(Lang::parse("de").unwrap());
        let translator = FakeTranslator::new("").with_detection("ja");

        let (_, source, configs) = passes(&request, &translator, Ok(vec![line("x")]));

        assert_eq!(configs.len(), 1);
        assert_eq!(source.map(Lang::code), Some("de"));
    }

    #[test]
    fn chosen_ocr_packs_disable_the_second_pass() {
        let mut request = request();
        request.ocr.languages = "eng+deu".to_string();
        request.two_pass_ocr = false;
        let translator = FakeTranslator::new("").with_detection("ja");

        let (_, source, configs) = passes(&request, &translator, Ok(vec![line("x")]));

        assert_eq!(configs, vec!["eng+deu".to_string()]);
        assert_eq!(source, None);
    }

    #[test]
    fn a_failed_or_empty_second_pass_keeps_the_first_reading() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let failed = passes(
            &request,
            &translator,
            Err(ocr::OcrError::Spawn("no deu".into())),
        );
        assert_eq!(failed.0[0].text, "für");
        assert_eq!(failed.1, None, "the first pass has no detection yet");

        let empty = passes(&request, &translator, Ok(Vec::new()));
        assert_eq!(empty.0[0].text, "für");
        assert_eq!(empty.1, None);
    }

    #[test]
    fn an_empty_capture_costs_no_detection() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");
        let configs = std::cell::RefCell::new(Vec::new());

        let result = recognize_blocks(&request, &translator, &|config| {
            configs.borrow_mut().push(config.languages.clone());
            Ok(Vec::new())
        })
        .expect("recognizing succeeds");

        assert!(result.0.is_empty());
        assert_eq!(configs.into_inner().len(), 1, "nothing to detect");
    }

    #[test]
    fn the_confirmation_pass_uses_the_detected_pack_alone() {
        assert_eq!(confirmation_languages(detected("de", false)), Some("deu"));
        assert_eq!(confirmation_languages(detected("ja", false)), Some("jpn"));
        assert_eq!(
            confirmation_languages(detected("zh", false)),
            Some("chi_sim+chi_tra"),
            "the Chinese row carries both packs"
        );
    }

    #[test]
    fn an_assumed_language_never_narrows_the_ocr_packs() {
        assert_eq!(confirmation_languages(detected("en", true)), None);
    }

    #[test]
    fn the_source_label_distinguishes_assumed_from_detected() {
        assert_eq!(
            source_label(Some(Lang::parse("de").unwrap()), false),
            "German"
        );
        assert_eq!(
            source_label(Some(Lang::parse("en").unwrap()), true),
            "English (assumed)",
            "a low-confidence fallback must not look like a detection"
        );
        assert_eq!(source_label(None, false), "the source language");
    }

    #[test]
    fn an_empty_block_list_is_a_clear_one_liner() {
        let mut canvas = Canvas::filled(4, 4, [255, 255, 255, 255]);
        let translator = FakeTranslator::echoing();
        let error = apply(
            &mut canvas,
            &[],
            Lang::parse("en").unwrap(),
            None,
            &translator,
            &Options::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "no text found in the captured area — nothing to translate"
        );
    }

    #[test]
    fn apply_paints_the_translation_onto_the_same_canvas() {
        let mut canvas = Canvas::filled(240, 60, [255, 255, 255, 255]);
        let translator = FakeTranslator::new("Hallo");
        let block = TextBlock {
            bbox: Bbox {
                x: 10,
                y: 10,
                width: 120,
                height: 10,
            },
            text: "Hello".to_string(),
            confidence: 95.0,
        };

        let applied = apply(
            &mut canvas,
            &[block],
            Lang::parse("de").unwrap(),
            None,
            &translator,
            &Options::default(),
        )
        .expect("painting succeeds");

        assert_eq!(applied.blocks, 1);
        assert_eq!(applied.report.drawn, 1);
        // The original glyphs sat in an empty white canvas, so any non-white
        // pixel inside the box is drawn text.
        let boxed = applied.report.blocks;
        assert_eq!(boxed, 1);
        assert!(
            canvas
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel[0] != 255 || pixel[1] != 255 || pixel[2] != 255),
            "the translated text must be visible in the buffer that gets saved"
        );
    }
}
