//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at b39cc37678c31ac715f22ccd49d1bdba39cc04cd (crates/engine).
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
use std::path::{Path, PathBuf};

use cosmic_screenshot_ocr::{self as ocr, TextBlock};
use cosmic_screenshot_overlay::{self as overlay, Canvas, FontSet, Options, Rect, TranslatedBlock};
use cosmic_screenshot_translation::{Detected, Lang, TranslateError, Translator};

pub mod lang;

/// The first, cheap reading exists only to name the language. English is the
/// most likely source of Latin UI text and reading it with one pack avoids
/// Tesseract picking Arabic or CJK glyphs for a Latin word.
pub const PROBE_LANGUAGES: &str = "eng";

/// A probe is trusted to name the source only when it found real text.
const PROBE_MIN_LINES: usize = 2;
const PROBE_MIN_CONFIDENCE: f32 = 60.0;

/// Everything the pipeline needs to know before it looks at the pixels.
#[derive(Clone, Debug)]
pub struct Request {
    pub target: Lang,
    /// Explicit source language; `None` means detect it from the capture.
    pub source: Option<Lang>,
    pub ocr: ocr::Config,
    /// Probe, detect, then re-read with the detected language's packs. Off when
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
    /// Credible lines handed to the translator.
    pub blocks: usize,
    /// Lines Tesseract emitted, before the confidence/readability filter.
    pub found: usize,
    /// Lines that survived the filter (same as `blocks`, named for reports).
    pub kept: usize,
    /// Lines the engine returned non-empty text for.
    pub translated: usize,
    /// Lines left exactly as they were because they are code, not prose.
    pub skipped_code: usize,
    /// Lines the opposite-polarity pass recovered from a block the crop's own
    /// polarity had folded into the background.
    pub recovered: usize,
    pub report: overlay::Report,
    pub detected_source: Option<Lang>,
    /// The source language came from a low-confidence fallback, not detection.
    pub source_assumed: bool,
    /// The capture was already in the target language, so nothing was painted.
    /// The front end must say so instead of reporting a silent success.
    pub same_language: bool,
    /// The requested target, for the explanation line.
    pub target: Option<Lang>,
    /// The opt-in debug directory for this run, when one was requested.
    pub debug_dir: Option<PathBuf>,
}

impl Applied {
    /// "German", "English (assumed)", or "the source language" — one phrase for
    /// the notification and the dialog status line.
    pub fn source_label(&self) -> String {
        source_label(self.detected_source, self.source_assumed)
    }

    /// A complete, honest accounting: found, kept, translated, drawn.
    ///
    /// One translated line out of ten must never read as success, and a run
    /// that found text but judged none of it readable must say so. Code lines
    /// are named separately: they were read, and deliberately not painted.
    pub fn counts(&self) -> String {
        let mut counts = format!(
            "found {}, kept {}, translated {}, drawn {}",
            self.found, self.kept, self.translated, self.report.drawn
        );
        if self.recovered > 0 {
            counts.push_str(&format!(" · {} recovered", self.recovered));
        }
        if self.skipped_code > 0 {
            counts.push_str(&format!(" · {} code line(s) left as-is", self.skipped_code));
        }
        counts
    }

    /// The line to show when the run did nothing, or `None` when it painted.
    pub fn explanation(&self) -> Option<String> {
        if self.same_language {
            return Some(format!(
                "the capture already looks like {} — pick another target language",
                self.target_name()
            ));
        }
        if self.kept == 0 {
            return Some(if self.found == 0 {
                "no text found in the captured area — nothing to translate".to_string()
            } else {
                format!(
                    "found {} line(s) but none readable — nothing translated",
                    self.found
                )
            });
        }
        if self.translated == 0 && self.report.drawn == 0 && self.skipped_code > 0 {
            return Some(format!(
                "the capture is code — {} line(s) left as they are",
                self.skipped_code
            ));
        }
        None
    }

    fn target_name(&self) -> String {
        self.target
            .and_then(|lang| lang::find(lang.code()))
            .map(|info| info.name.to_string())
            .unwrap_or_else(|| "the target language".to_string())
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
    recognize: &dyn Fn(&ocr::Config) -> Result<ocr::Reading, ocr::OcrError>,
) -> Result<Applied, Error> {
    // One debug directory per translation, shared by every pass below.
    let mut effective = request.clone();
    if effective.ocr.debug.is_none() {
        effective.ocr.debug = ocr::Debug::from_env();
    }
    let debug_dir = effective
        .ocr
        .debug
        .as_ref()
        .map(|debug| debug.dir().to_path_buf());

    let (reading, source, mut trace) = recognize_blocks(&effective, translator, recognize)?;
    for correction in &reading.corrections {
        trace.push(format!(
            "corrected: '{}' -> '{}' ({})",
            correction.from,
            correction.to,
            if correction.distance == 1 {
                "1 edit".to_string()
            } else {
                format!("{} edits", correction.distance)
            }
        ));
    }

    if reading.blocks.is_empty() {
        // No text at all is the existing one-line error. Text that Tesseract
        // found but none of it credible is a different, honest outcome: the
        // status must say "found 10, kept 0", not pretend the run succeeded.
        if reading.found == 0 {
            return Err(Error::NoText);
        }
        let applied = Applied {
            found: reading.found,
            kept: 0,
            debug_dir: debug_dir.clone(),
            detected_source: source,
            target: Some(request.target),
            ..Applied::default()
        };
        trace.push(format!(
            "kept 0 of {} found — nothing painted (psm {}, languages {})",
            reading.found, reading.psm, reading.languages
        ));
        write_report(debug_dir.as_deref(), &trace);
        return Ok(applied);
    }

    let mut applied = apply(
        canvas,
        &reading.blocks,
        request.target,
        source,
        translator,
        &request.options,
    )?;
    applied.found = reading.found;
    applied.kept = reading.kept();
    applied.blocks = reading.kept();
    applied.recovered = reading.recovered;
    applied.debug_dir = debug_dir.clone();

    trace.push(format!(
        "chosen: languages {}, psm {}, scale {} — found {}, kept {}, dropped {} (confidence), {} (unreadable)",
        reading.languages,
        reading.psm,
        reading.scale,
        reading.found,
        reading.kept(),
        reading.dropped_low_confidence,
        reading.dropped_unreadable
    ));
    if applied.recovered > 0 {
        trace.push(format!(
            "opposite polarity: {} line(s) recovered from a block the crop's own polarity had folded away",
            applied.recovered
        ));
    }
    trace.push(format!(
        "result: {} · {}",
        applied.counts(),
        applied.source_label()
    ));
    if let Some(explanation) = applied.explanation() {
        trace.push(explanation);
    }
    write_report(debug_dir.as_deref(), &trace);

    if let Some(dir) = &applied.debug_dir {
        // The exact bytes the dialog would show for this crop, after painting.
        let _ = canvas.save(&dir.join("final.png"));
    }

    Ok(applied)
}

fn write_report(dir: Option<&Path>, lines: &[String]) {
    let Some(dir) = dir else { return };
    let mut report = String::new();
    for line in lines {
        report.push_str(line);
        report.push('\n');
    }
    let _ = std::fs::write(dir.join("report.txt"), report);
}

/// The packs to read a detected language with: its own, plus English for the
/// Latin/Cyrillic/Greek screens that mix English labels into a local menu.
///
/// This is the narrowing the old two-pass code did only *after* a full
/// 24-pack read, when Tesseract had already had the chance to read `Game` as
/// `سس`. The probe names the script first, so the main pass never sees the
/// packs of an unrelated script.
pub fn main_languages(detected: Lang) -> String {
    let Some(info) = lang::find(detected.code()) else {
        return lang::all_ocr_languages();
    };
    let mut packs: Vec<&str> = info.tesseract.split('+').collect();
    let mixes_english = matches!(
        info.script,
        overlay::Script::Latin | overlay::Script::Cyrillic | overlay::Script::Greek
    );
    if mixes_english && !packs.contains(&"eng") {
        packs.insert(0, "eng");
    }
    packs.join("+")
}

/// OCR the capture: a cheap English probe names the language, then the main
/// pass reads with that language's packs. Falls back to every pack only when
/// the English pass kept fewer than two lines or read no Latin text, keeping
/// whichever reading is better.
fn recognize_blocks(
    request: &Request,
    translator: &dyn Translator,
    recognize: &dyn Fn(&ocr::Config) -> Result<ocr::Reading, ocr::OcrError>,
) -> Result<(ocr::Reading, Option<Lang>, Vec<String>), Error> {
    let config = request.ocr.clone();
    let mut trace = Vec::new();

    if request.source.is_some() || !request.two_pass_ocr {
        let reading = recognize(&labelled(&config, "main")).map_err(Error::Ocr)?;
        trace.push(format!(
            "single pass: languages {}, psm {}",
            reading.languages, reading.psm
        ));
        return Ok((reading, request.source, trace));
    }

    let probe = recognize(&labelled(&config, "probe").with_languages(PROBE_LANGUAGES))
        .map_err(Error::Ocr)?;
    trace.push(format!(
        "probe: languages {}, psm {}, kept {} of {} (mean confidence {:.0})",
        probe.languages,
        probe.psm,
        probe.kept(),
        probe.found,
        probe.mean_confidence()
    ));

    let source = trusted_detection(&probe, translator);

    // The 24-pack read is ~10x the cost of the binarised English pass, so it is
    // kept for the one case the probe cannot serve: a capture whose script the
    // English model cannot read at all. Low confidence on Latin text is not
    // that case — the probe's lines are still worth translating, and the
    // translator detects the source again when it is handed `None`.
    if source.is_none() && needs_every_pack(&probe) {
        trace.push("detected: nothing trustworthy; reading with every pack".to_string());
        // Mixed or non-Latin capture: read every pack, then keep the better of
        // the two readings. The probe is never worse than the old first pass
        // because it is judged on the same score.
        let all = match recognize(&labelled(&config, "all")) {
            Ok(reading) => reading,
            Err(error) => {
                trace.push(format!(
                    "all-pack fallback failed ({error}); using the probe"
                ));
                return Ok((probe, None, trace));
            }
        };
        trace.push(format!(
            "all packs: kept {} of {} (psm {})",
            all.kept(),
            all.found,
            all.psm
        ));
        let chosen = ocr::choose(&[probe.clone(), all]).unwrap_or(probe);
        trace.push(format!(
            "chosen: languages {}, psm {}, kept {}",
            chosen.languages,
            chosen.psm,
            chosen.kept()
        ));
        return Ok((chosen, None, trace));
    }

    let Some(source) = source else {
        trace.push(format!(
            "detected: nothing trustworthy; keeping the probe ({} credible lines, no full-pack read)",
            probe.kept()
        ));
        return Ok((probe, None, trace));
    };
    trace.push(format!(
        "detected: {} ({})",
        source.english_name(),
        main_languages(source)
    ));

    let languages = main_languages(source);
    if languages == PROBE_LANGUAGES {
        return Ok((probe, Some(source), trace));
    }

    let main = match recognize(&labelled(&config, "main").with_languages(&languages)) {
        Ok(reading) if !reading.blocks.is_empty() => reading,
        Ok(_) => {
            trace.push(format!(
                "main pass ({languages}) found nothing; keeping the probe"
            ));
            return Ok((probe, Some(source), trace));
        }
        Err(error) => {
            trace.push(format!(
                "main pass ({languages}) failed ({error}); keeping the probe"
            ));
            return Ok((probe, Some(source), trace));
        }
    };
    trace.push(format!(
        "main: languages {}, psm {}, kept {} of {}",
        main.languages,
        main.psm,
        main.kept(),
        main.found
    ));
    Ok((main, Some(source), trace))
}

/// The probe's language, but only when the probe actually read the screen and
/// the sidecar was confident. An assumed language must not narrow the packs.
fn trusted_detection(probe: &ocr::Reading, translator: &dyn Translator) -> Option<Lang> {
    if probe.kept() < PROBE_MIN_LINES
        || probe.mean_confidence() < PROBE_MIN_CONFIDENCE
        || !mostly_latin(&probe.texts())
    {
        return None;
    }
    match translator.detect_source(&probe.texts()) {
        Ok(Some(Detected {
            lang,
            assumed: false,
        })) => Some(lang),
        _ => None,
    }
}

/// Does this probe justify the 24-pack read? Only when the English pass kept
/// too little to translate, or read no Latin text at all — a genuinely
/// non-Latin capture. A low-confidence but readable Latin probe is used as-is:
/// the translator detects the language again from its text.
fn needs_every_pack(probe: &ocr::Reading) -> bool {
    probe.kept() < PROBE_MIN_LINES || !mostly_latin(&probe.texts())
}

/// Is the probe's text letter-based and overwhelmingly Latin? A CJK or Cyrillic
/// screen read with the English model yields stray Latin fragments, which must
/// not be mistaken for a Latin capture.
fn mostly_latin(texts: &[&str]) -> bool {
    let mut latin = 0usize;
    let mut other = 0usize;
    for character in texts.iter().flat_map(|text| text.chars()) {
        if !character.is_alphabetic() {
            continue;
        }
        let is_latin =
            character.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&character);
        if is_latin {
            latin += 1;
        } else {
            other += 1;
        }
    }
    latin >= 8 && latin * 10 >= (latin + other) * 9
}

fn labelled(config: &ocr::Config, label: &str) -> ocr::Config {
    let mut config = config.clone();
    config.debug = config.debug.as_ref().map(|debug| debug.labelled(label));
    config
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
    // A code block is not prose: `cd ~/src/…` came back as "Trinkfisch ~/src/…"
    // and `target/` as "Ziel/Dateigabe". Leave those lines exactly as they are
    // instead of painting a guess over them.
    let prose: Vec<&TextBlock> = blocks
        .iter()
        .filter(|block| !looks_like_code(&block.text))
        .collect();
    let skipped_code = blocks.len() - prose.len();
    if prose.is_empty() {
        return Ok(Applied {
            blocks: blocks.len(),
            found: blocks.len(),
            kept: blocks.len(),
            skipped_code,
            target: Some(target),
            ..Applied::default()
        });
    }
    let texts: Vec<&str> = prose.iter().map(|block| block.text.as_str()).collect();
    let batch = translator
        .translate(&texts, target, source)
        .map_err(Error::Translation)?;
    if batch.items.len() != prose.len() {
        return Err(Error::Translation(TranslateError::CountMismatch {
            requested: prose.len(),
            received: batch.items.len(),
        }));
    }

    let detected_source = batch.detected_source();
    let source_assumed = batch.source_assumed;
    // The default target is the session language, so pressing Translate on a
    // screen in that language used to repaint the same words and look like a
    // success. Name it instead.
    let same_language =
        source == Some(target) || (detected_source == Some(target) && !source_assumed);
    if same_language {
        return Ok(Applied {
            blocks: blocks.len(),
            found: blocks.len(),
            kept: blocks.len(),
            translated: blocks.len() - skipped_code,
            skipped_code,
            detected_source,
            source_assumed,
            same_language: true,
            target: Some(target),
            ..Applied::default()
        });
    }

    let translated: Vec<TranslatedBlock> = prose
        .iter()
        .zip(&batch.items)
        .map(|(block, item)| TranslatedBlock {
            rect: rect_of(block),
            text: item.text.clone(),
        })
        .collect();
    let written = batch
        .items
        .iter()
        .filter(|item| !item.text.trim().is_empty())
        .count();

    let script = lang::find(target.code())
        .map(|info| info.script)
        .unwrap_or(overlay::Script::Latin);
    let fonts = FontSet::system(script).map_err(Error::Overlay)?;
    let report = overlay::overlay(canvas, &translated, &fonts, options);

    Ok(Applied {
        blocks: blocks.len(),
        found: blocks.len(),
        kept: blocks.len(),
        translated: written,
        skipped_code,
        recovered: 0,
        report,
        detected_source,
        source_assumed,
        same_language: false,
        target: Some(target),
        debug_dir: None,
    })
}

/// Does this line look like code rather than prose?
///
/// Machine translation has no idea what a command is: `cd ~/src/x` came back as
/// "Trinkfisch ~/src/x" and `target/` as "Ziel/Dateigabe", which is worse than
/// leaving the line alone. The signals below are the cheap, high-precision
/// ones — a path, a flag, a shell verb, shell punctuation — and a line needs
/// two before it is treated as code, so ordinary prose that happens to contain
/// a slash or an `install` still gets translated.
pub fn looks_like_code(text: &str) -> bool {
    /// Signals needed before a line is treated as code.
    const CODE_SIGNALS: usize = 2;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut signals = 0;

    // A shell verb opens the line — or follows an `ENV=value` prefix. Commands
    // that never open a sentence are two signals on their own; verbs that are
    // also ordinary English words are one, so "install the portal build" stays
    // prose while `git status` does not.
    for (index, token) in trimmed.split_whitespace().take(2).enumerate() {
        let word = token.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if COMMAND_VERBS.contains(&word) {
            signals += if index == 0 { 2 } else { 1 };
        } else if SHELL_VERBS.contains(&word) {
            signals += 1;
        }
    }
    if trimmed.starts_with('$') {
        signals += 1;
    }
    if trimmed.contains(" && ")
        || trimmed.contains(" || ")
        || trimmed.contains(" | ")
        || trimmed.contains("$(")
        || trimmed.contains("${")
        || trimmed.contains("2>&1")
    {
        signals += 1;
    }
    if trimmed.contains('{')
        || trimmed.contains('}')
        || trimmed.contains("=>")
        || trimmed.contains("->")
        || trimmed.contains("::")
        || trimmed.contains("==")
        || trimmed.contains("!=")
    {
        signals += 2;
    }
    // A trailing `;` ends a statement; a semicolon inside a sentence does not.
    // One mid-sentence `;` has to stay below the threshold, or "Save; then
    // close the dialog." would stop being translated.
    if trimmed.ends_with(';') {
        signals += 2;
    } else if trimmed.contains(';') {
        signals += 1;
    }
    // Ratios and the word "and/or" are prose, so the relative-slash family
    // counts once per line: "Health 80/100, Mana 40/50" stays prose.
    let mut relative_slash = false;
    for token in trimmed.split_whitespace() {
        if token.starts_with('/')
            || token.starts_with("~/")
            || token.starts_with("./")
            || token.starts_with("../")
            || token.contains("://")
        {
            signals += 2;
        } else if token.contains('/') && token.len() > 2 && !relative_slash {
            signals += 1;
            relative_slash = true;
        }
        // Any dash token with a letter after the dash is a flag: `-m`, `-x`,
        // `-rf`, `-la`. Negative numbers and `-` on its own are not.
        if (token.starts_with("--") && token.len() > 2)
            || (token.starts_with('-')
                && token.len() > 1
                && token
                    .chars()
                    .nth(1)
                    .is_some_and(|c| c.is_ascii_alphabetic()))
        {
            signals += 1;
        }
        // ENV=value at the start of a command.
        if token.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }) {
            signals += 1;
        }
        if signals >= CODE_SIGNALS {
            return true;
        }
    }
    signals >= CODE_SIGNALS
}

/// Shell verbs that are also ordinary English words: one signal, so "install
/// the portal build" and "just build it" stay prose.
const SHELL_VERBS: &[&str] = &[
    "install", "make", "find", "export", "source", "echo", "kill", "ps", "just", "mount", "tar",
];

/// Commands that never open an English sentence: two signals on their own, so
/// `git status`, `systemctl restart foo` and `ls -la` are not sent to NLLB.
const COMMAND_VERBS: &[&str] = &[
    "cd",
    "sudo",
    "git",
    "apt",
    "apt-get",
    "dnf",
    "pacman",
    "cargo",
    "rustup",
    "systemctl",
    "journalctl",
    "systemd-run",
    "docker",
    "podman",
    "kubectl",
    "npm",
    "pnpm",
    "yarn",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "mkdir",
    "rmdir",
    "rm",
    "cp",
    "mv",
    "chmod",
    "chown",
    "unzip",
    "grep",
    "sed",
    "awk",
    "ln",
    "ls",
    "pwd",
    "dmesg",
    "nmcli",
    "umount",
];

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

    fn reading(languages: &str, psm: u8, texts: &[&str]) -> ocr::Reading {
        ocr::Reading {
            blocks: texts.iter().map(|text| line(text)).collect(),
            found: texts.len(),
            psm,
            languages: languages.to_string(),
            scale: 2,
            ..ocr::Reading::default()
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
        readings: Vec<ocr::Reading>,
    ) -> (Vec<TextBlock>, Option<Lang>, Vec<String>) {
        let configs = std::cell::RefCell::new(Vec::new());
        let readings = std::cell::RefCell::new(std::collections::VecDeque::from(readings));
        let result = recognize_blocks(request, translator, &|config| {
            configs.borrow_mut().push(config.languages.clone());
            Ok(readings
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| reading(&config.languages, 11, &[])))
        });
        let (reading, source, _) = result.expect("recognizing succeeds");
        (reading.blocks, source, configs.into_inner())
    }

    #[test]
    fn the_probe_names_the_language_before_the_main_pass() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let (blocks, source, configs) = passes(
            &request,
            &translator,
            vec![
                reading("eng", 11, &["Spielemenü", "Zurück zum Spiel"]),
                reading(
                    "eng+deu",
                    11,
                    &["Spielemenü", "Zurück zum Spiel", "Optionen"],
                ),
            ],
        );

        assert_eq!(
            configs,
            vec!["eng".to_string(), "eng+deu".to_string()],
            "the main pass must not see every pack"
        );
        assert_eq!(source.map(Lang::code), Some("de"));
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn an_english_capture_reuses_the_probe_reading() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("en");

        let (blocks, source, configs) = passes(
            &request,
            &translator,
            vec![reading("eng", 11, &["Back to Game", "Options"])],
        );

        assert_eq!(configs, vec!["eng".to_string()]);
        assert_eq!(source.map(Lang::code), Some("en"));
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn an_untrustworthy_probe_falls_back_to_every_pack() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let (_, source, configs) = passes(
            &request,
            &translator,
            vec![
                reading("eng", 11, &["a"]),
                reading(
                    &lang::all_ocr_languages(),
                    11,
                    &["Spielemenü", "Zurück zum Spiel"],
                ),
            ],
        );

        assert_eq!(configs[0], "eng");
        assert_eq!(configs[1], lang::all_ocr_languages());
        assert_eq!(source, None, "the sidecar detects again when translating");
    }

    #[test]
    fn an_assumed_detection_keeps_the_probe_without_a_full_read() {
        let request = request();
        let translator = FakeTranslator::new("").assuming_the_source_on_detection("en");

        let (_, source, configs) = passes(
            &request,
            &translator,
            vec![
                reading("eng", 6, &["Hello there", "Options"]),
                reading(&lang::all_ocr_languages(), 6, &["Hello there", "Options"]),
            ],
        );

        assert_eq!(
            configs,
            vec!["eng".to_string()],
            "readable Latin text must not pay for the 24-pack read"
        );
        assert_eq!(source, None, "the sidecar detects again when translating");
    }

    #[test]
    fn a_low_confidence_latin_probe_is_translated_as_read() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("en");
        let mut dim = reading("eng", 6, &["Game Menu", "Back to Game"]);
        for block in &mut dim.blocks {
            block.confidence = 40.0;
        }

        let (blocks, source, configs) = passes(&request, &translator, vec![dim]);

        assert_eq!(configs, vec!["eng".to_string()]);
        assert_eq!(source, None);
        assert_eq!(blocks.len(), 2, "the probe's text is still usable");
    }

    #[test]
    fn a_non_latin_probe_still_reads_every_pack() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let (_, source, configs) = passes(
            &request,
            &translator,
            vec![
                reading("eng", 6, &["Меню", "Назад в игру"]),
                reading(
                    &lang::all_ocr_languages(),
                    6,
                    &["Меню", "Назад в игру", "Опции"],
                ),
            ],
        );

        assert_eq!(configs[1], lang::all_ocr_languages());
        assert_eq!(source, None);
    }

    #[test]
    fn an_explicit_source_skips_detection_entirely() {
        let mut request = request();
        request.source = Some(Lang::parse("de").unwrap());
        let translator = FakeTranslator::new("").with_detection("ja");

        let (_, source, configs) = passes(
            &request,
            &translator,
            vec![reading("eng+deu", 11, &["Optionen"])],
        );

        assert_eq!(configs.len(), 1);
        assert_eq!(source.map(Lang::code), Some("de"));
    }

    #[test]
    fn chosen_ocr_packs_disable_the_probe() {
        let mut request = request();
        request.ocr.languages = "eng+deu".to_string();
        request.two_pass_ocr = false;
        let translator = FakeTranslator::new("").with_detection("ja");

        let (_, source, configs) = passes(
            &request,
            &translator,
            vec![reading("eng+deu", 11, &["Optionen"])],
        );

        assert_eq!(configs, vec!["eng+deu".to_string()]);
        assert_eq!(source, None);
    }

    #[test]
    fn a_failed_or_empty_main_pass_keeps_the_probe() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let configs = std::cell::RefCell::new(Vec::new());
        let result = recognize_blocks(&request, &translator, &|config| {
            let first = configs.borrow().is_empty();
            configs.borrow_mut().push(config.languages.clone());
            if first {
                Ok(reading("eng", 11, &["Spielemenü", "Optionen"]))
            } else {
                Err(ocr::OcrError::Spawn("no deu".into()))
            }
        })
        .expect("the probe still reads");

        assert_eq!(result.0.blocks.len(), 2);
        assert_eq!(result.1.map(Lang::code), Some("de"));
    }

    #[test]
    fn the_fallback_keeps_the_better_of_the_two_readings() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");

        let (blocks, _, _) = passes(
            &request,
            &translator,
            vec![
                reading("eng", 11, &["Game Menu"]),
                reading(
                    &lang::all_ocr_languages(),
                    3,
                    &["Game Menu", "Back to Game", "Options"],
                ),
            ],
        );

        assert_eq!(
            blocks.len(),
            3,
            "the all-pack pass found more text and must win"
        );
    }

    #[test]
    fn the_main_pass_adds_english_to_a_latin_language() {
        assert_eq!(
            main_languages(Lang::parse("de").unwrap()),
            "eng+deu",
            "a German menu can still carry English labels"
        );
        assert_eq!(main_languages(Lang::parse("en").unwrap()), "eng");
        assert_eq!(
            main_languages(Lang::parse("zh").unwrap()),
            "chi_sim+chi_tra",
            "CJK uses its own packs alone"
        );
        assert_eq!(main_languages(Lang::parse("ru").unwrap()), "eng+rus");
    }

    #[test]
    fn a_latin_probe_is_required_before_narrowing() {
        let translator = FakeTranslator::new("").with_detection("en");
        let cyrillic = reading("eng", 11, &["Меню", "Назад в игру"]);
        assert!(
            trusted_detection(&cyrillic, &translator).is_none(),
            "stray non-Latin text must not be narrowed as if it were English"
        );
        let latin = reading("eng", 11, &["Game Menu", "Back to Game"]);
        assert_eq!(
            trusted_detection(&latin, &translator).map(Lang::code),
            Some("en")
        );
    }

    #[test]
    fn an_empty_capture_costs_no_detection() {
        let request = request();
        let translator = FakeTranslator::new("").with_detection("de");
        let configs = std::cell::RefCell::new(Vec::new());

        let result = recognize_blocks(&request, &translator, &|config| {
            configs.borrow_mut().push(config.languages.clone());
            Ok(reading(&config.languages, 11, &[]))
        })
        .expect("recognizing succeeds");

        assert!(result.0.blocks.is_empty());
        assert!(!configs.into_inner().is_empty());
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
        assert_eq!(applied.translated, 1);
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

    #[test]
    fn a_capture_already_in_the_target_language_is_named_not_painted() {
        let mut canvas = Canvas::filled(240, 60, [255, 255, 255, 255]);
        let before = canvas.rgba.clone();
        let translator = FakeTranslator::new("").with_detected_language("en");

        let applied = apply(
            &mut canvas,
            &[line("Game Menu")],
            Lang::parse("en").unwrap(),
            None,
            &translator,
            &Options::default(),
        )
        .expect("the run succeeds");

        assert!(applied.same_language);
        assert_eq!(applied.report.drawn, 0);
        assert_eq!(applied.found, 1);
        assert_eq!(applied.kept, 1);
        assert_eq!(canvas.rgba, before, "nothing may be repainted");
        assert!(
            applied
                .explanation()
                .expect("an explanation")
                .contains("pick another target"),
            "{:?}",
            applied.explanation()
        );
    }

    #[test]
    fn found_but_unreadable_is_reported_instead_of_translated() {
        let translator = FakeTranslator::new("[de] ");
        let request = request();
        let empty = ocr::Reading {
            blocks: Vec::new(),
            found: 10,
            dropped_unreadable: 10,
            psm: 11,
            languages: "eng".into(),
            scale: 2,
            ..ocr::Reading::default()
        };
        let mut canvas = Canvas::filled(20, 20, [0, 0, 0, 255]);
        let applied = translate_canvas(&mut canvas, &request, &translator, &|_| Ok(empty.clone()))
            .expect("an honest zero is not an error");

        assert_eq!(applied.found, 10);
        assert_eq!(applied.kept, 0);
        assert_eq!(applied.report.drawn, 0);
        assert_eq!(
            applied.explanation().as_deref(),
            Some("found 10 line(s) but none readable — nothing translated")
        );
        assert!(translator.calls().is_empty(), "nothing to translate");
    }

    #[test]
    fn the_counts_line_never_hides_a_partial_run() {
        let applied = Applied {
            found: 10,
            kept: 9,
            translated: 9,
            report: overlay::Report {
                drawn: 1,
                ..Default::default()
            },
            ..Applied::default()
        };
        assert_eq!(applied.counts(), "found 10, kept 9, translated 9, drawn 1");
    }

    #[test]
    fn the_report_names_every_corrected_token() {
        let dir = std::env::temp_dir().join(format!(
            "cosmic-screenshot-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let mut request = request();
        request.ocr.debug = ocr::Debug::in_directory(dir.to_str().unwrap());

        let mut corrected = reading("eng", 6, &["Game Menu", "Back to Game"]);
        corrected.corrections = vec![ocr::Correction {
            from: "Bane".to_string(),
            to: "Game".to_string(),
            distance: 2,
        }];
        let translator = FakeTranslator::new("[de] ");
        let mut canvas = Canvas::filled(120, 60, [255, 255, 255, 255]);
        let applied = translate_canvas(&mut canvas, &request, &translator, &|_| {
            Ok(corrected.clone())
        })
        .expect("the run succeeds");

        let debug_dir = applied.debug_dir.expect("a debug directory");
        let report = std::fs::read_to_string(debug_dir.join("report.txt")).expect("report");
        assert!(
            report.contains("corrected: 'Bane' -> 'Game' (2 edits)"),
            "the dump must show when the corrector fired: {report}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn code_lines_are_recognised_and_prose_is_not() {
        for code in [
            "cd ~/src/xdg-desktop-portal-cosmic",
            "sudo install -m 0755 target/x /usr/libexec/",
            "target/x /usr/libexec/",
            "systemctl --user restart the.service",
            "journalctl --user -f -u portal",
            "cargo test --offline",
            "RUST_LOG=debug just run",
            "let x = 5;",
            "if (a == b) {",
            "https://example.com/x",
            "sudo apt install nvidia-driver",
            "git status",
            "git checkout main",
            "cargo build",
            "systemctl restart foo",
            "curl example.com",
            "rm -rf build",
            "ls -la",
        ] {
            assert!(looks_like_code(code), "{code:?} must be code");
        }
        for prose in [
            "Two commands:",
            "Then: Super+Shift+S → …",
            "install the portal build",
            "restart the impl service",
            "David Leinweber",
            "The overlay still leaves part of the body",
            "in English, and the code block is nonsense.",
            "translated 7 of 8 lines",
            "German · 2.1 s · 7 blocks",
            "and/or",
            "I do not know; maybe later.",
            "Save; then close the dialog.",
            "Menü; Einstellungen",
            "Health 80/100, Mana 40/50",
            "Progress 3/10 · 2/10",
            "Input/Output · Audio/Video",
        ] {
            assert!(!looks_like_code(prose), "{prose:?} must be prose");
        }
    }

    #[test]
    fn a_code_line_is_left_untouched_and_named_in_the_counts() {
        let mut canvas = Canvas::filled(300, 140, [250, 250, 250, 255]);
        let blocks = vec![
            TextBlock {
                bbox: Bbox {
                    x: 10,
                    y: 10,
                    width: 100,
                    height: 20,
                },
                text: "Two commands:".to_string(),
                confidence: 95.0,
            },
            TextBlock {
                bbox: Bbox {
                    x: 10,
                    y: 60,
                    width: 200,
                    height: 20,
                },
                text: "cd ~/src/xdg-desktop-portal-cosmic".to_string(),
                confidence: 95.0,
            },
        ];
        let translator = FakeTranslator::new("[de] ");
        let applied = apply(
            &mut canvas,
            &blocks,
            Lang::parse("de").unwrap(),
            None,
            &translator,
            &Options::default(),
        )
        .expect("the run succeeds");

        assert_eq!(applied.skipped_code, 1);
        assert_eq!(applied.translated, 1);
        assert_eq!(applied.report.drawn, 1, "only the prose is painted");
        assert_eq!(
            translator.calls()[0].texts,
            vec!["Two commands:"],
            "the code line must never reach the translator"
        );
        assert_eq!(
            applied.counts(),
            "found 2, kept 2, translated 1, drawn 1 · 1 code line(s) left as-is"
        );
    }

    #[test]
    fn a_capture_of_pure_code_says_so_instead_of_reporting_success() {
        let mut canvas = Canvas::filled(300, 60, [250, 250, 250, 255]);
        let blocks = vec![TextBlock {
            bbox: Bbox {
                x: 10,
                y: 10,
                width: 200,
                height: 20,
            },
            text: "journalctl --user -f -u portal".to_string(),
            confidence: 95.0,
        }];
        let translator = FakeTranslator::new("[de] ");
        let applied = apply(
            &mut canvas,
            &blocks,
            Lang::parse("de").unwrap(),
            None,
            &translator,
            &Options::default(),
        )
        .expect("the run succeeds");

        assert_eq!(applied.skipped_code, 1);
        assert_eq!(applied.report.drawn, 0);
        assert!(translator.calls().is_empty());
        assert_eq!(
            applied.explanation().as_deref(),
            Some("the capture is code — 1 line(s) left as they are")
        );
    }
}
