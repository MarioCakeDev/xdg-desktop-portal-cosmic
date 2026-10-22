//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at 3b7ff2f5b6544da58ab0ab1d4ab1c523c8313ceb (crates/ocr).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! OCR of a captured region.
//!
//! Uses the `tesseract` CLI rather than a native binding: no leptonica build
//! step, the language packs are ordinary apt packages, and the process boundary
//! keeps a misbehaving OCR run from taking the UI with it. The TSV parser is
//! pure so it is tested against fixtures.
//!
//! A screenshot is not a document. The defaults are tuned for one: the crop is
//! binarised before recognition (grey, histogram stretch, hard threshold),
//! because game UI is anti-aliased light text on a dark panel and Tesseract
//! reads the soft edges as strokes — `Advancements` comes back as
//! `AYancernrentS` on the raw pixels; the crop is then enlarged with nearest
//! neighbour, because the font is already a bitmap and interpolation only
//! blurs its strokes; one uniform-block pass (6) reads it, with a sparse-text
//! pass (11) only when that keeps fewer than two lines; and a line made mostly
//! of punctuation is dropped instead of being handed to the translator.
//!
//! Paint boxes are built from **words**, never from Tesseract's line box. A
//! line box can start at a button's border glyph and can span two columns:
//! painting it covers art and drops the translation far left of its label.
//! Words with no letters are dropped, the rest are clustered by horizontal gap,
//! and each cluster is translated and painted on its own.
//!
//! Nothing here invents text: every block comes from a Tesseract pass over the
//! pixels, and the optional corrector in [`spellfix`] only replaces a token
//! with a vocabulary word that is one or two edits away.

mod spellfix;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use image::imageops::FilterType;

pub use spellfix::Correction;

/// Every language pack the tool is prepared to read. The install recipe ships
/// all of them; a narrower set can be selected with
/// `COSMIC_SCREENSHOT_OCR_LANGS=eng+deu` (faster on a small region).
pub const ALL_LANGUAGES: &str = "eng+deu+fra+spa+ita+por+nld+pol+rus+ukr+tur+ces+swe+dan+fin+ell+hun+ron+ind+jpn+chi_sim+chi_tra+kor+ara";

pub const LANGUAGES_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_LANGS";

/// Opt-in dump of everything one run handed to Tesseract, plus the TSV it
/// returned. Set it to a directory; each translation gets its own `run-*`
/// subdirectory there.
pub const DEBUG_DIR_VARIABLE: &str = "COSMIC_SCREENSHOT_DEBUG_DIR";

/// Pin one page segmentation mode instead of the built-in default and its
/// short-read fallback. Mostly for tuning against a capture.
pub const PSM_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_PSM";

/// Enlargement factor applied before recognition (`1` disables it).
pub const SCALE_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_SCALE";

/// Set to `0`/`false`/`off` to hand Tesseract the greyscale crop instead of a
/// binarised one. On by default: binarisation is what makes a game menu
/// readable at all.
pub const BINARIZE_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_BINARIZE";

/// Luma split point for the binarised crop (`1`–`254`). Raise it for a dim
/// capture, lower it when panel shading bleeds into the glyphs.
pub const THRESHOLD_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_THRESHOLD";

/// Set to `0`/`false`/`off` to hand the translator exactly what Tesseract read,
/// without the small UI-vocabulary corrector.
pub const SPELLFIX_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_SPELLFIX";

/// Light UI text on a dark panel separates cleanly at 150.
pub const DEFAULT_THRESHOLD: u8 = 150;

/// Below this mean word confidence a line is dropped.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 30.0;

/// Screenshot default: one uniform-block pass (psm 6). On a binarised menu it
/// reads the labels in ~300 ms; a tournament of page segmentation modes costs
/// six seconds and reads no better.
const DEFAULT_PSM: u8 = 6;

/// Only tried when the default pass keeps fewer than two lines.
const FALLBACK_PSMS: [u8; 1] = [11];

/// Fewer kept lines than this is not a reading worth trusting.
const FALLBACK_BELOW_LINES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bbox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Bbox {
    pub fn right(&self) -> u32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }

    fn union(&mut self, other: Bbox) {
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        self.x = left;
        self.y = top;
        self.width = right - left;
        self.height = bottom - top;
    }

    /// Map a box from the enlarged image back to the capture's pixels.
    fn scaled(self, factor: u32) -> Self {
        if factor <= 1 {
            return self;
        }
        let factor = factor as f32;
        let scale = |value: u32| ((value as f32 / factor).round() as u32).max(1);
        Self {
            x: (self.x as f32 / factor).round() as u32,
            y: (self.y as f32 / factor).round() as u32,
            width: scale(self.width),
            height: scale(self.height),
        }
    }
}

/// One recognised line of text and where it sits in the image.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    pub bbox: Bbox,
    pub text: String,
    /// Tesseract's mean word confidence, 0–100.
    pub confidence: f32,
}

/// What one Tesseract pass found, before and after the credibility filter.
#[derive(Clone, Debug, Default)]
pub struct Attempt {
    pub psm: u8,
    /// Lines Tesseract emitted.
    pub found: usize,
    /// Lines kept after confidence and readability filtering.
    pub kept: usize,
    pub mean_confidence: f32,
    pub elapsed_ms: u128,
}

/// The reading the pipeline should use, plus the counts a status line needs.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    /// Credible lines, in reading order, in the capture's coordinates.
    pub blocks: Vec<TextBlock>,
    /// Lines Tesseract emitted across the chosen pass.
    pub found: usize,
    pub dropped_low_confidence: usize,
    pub dropped_unreadable: usize,
    /// The segmentation mode that produced `blocks`.
    pub psm: u8,
    pub languages: String,
    /// Enlargement applied before recognition.
    pub scale: u32,
    pub elapsed_ms: u128,
    /// Every reading of this image that was tried.
    pub attempts: Vec<Attempt>,
    /// Tokens the UI corrector replaced, in the order it fired.
    pub corrections: Vec<Correction>,
}

impl Reading {
    pub fn kept(&self) -> usize {
        self.blocks.len()
    }

    pub fn mean_confidence(&self) -> f32 {
        if self.blocks.is_empty() {
            return 0.0;
        }
        let total: f32 = self.blocks.iter().map(|block| block.confidence).sum();
        total / self.blocks.len() as f32
    }

    pub fn texts(&self) -> Vec<&str> {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect()
    }
}

/// An opt-in debug dump directory for one translation run.
///
/// `COSMIC_SCREENSHOT_DEBUG_DIR=/tmp/ocr` creates
/// `/tmp/ocr/run-<millis>-<pid>-<n>/` and every OCR pass writes the exact image
/// it handed to Tesseract plus the raw TSV it got back. The engine writes the
/// final painted crop and a report beside them.
#[derive(Clone, Debug)]
pub struct Debug {
    dir: PathBuf,
    label: String,
}

static RUNS: AtomicU64 = AtomicU64::new(0);

impl Debug {
    /// A fresh run directory, or `None` when the variable is unset or the
    /// directory cannot be created. A missing dump must never fail a capture.
    pub fn from_env() -> Option<Self> {
        Self::in_directory(std::env::var(DEBUG_DIR_VARIABLE).ok()?)
    }

    pub fn in_directory(raw: impl AsRef<str>) -> Option<Self> {
        let raw = raw.as_ref().trim();
        if raw.is_empty() {
            return None;
        }
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(raw).join(format!("run-{millis}-{}-{run}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        Some(Self {
            dir,
            label: "ocr".to_string(),
        })
    }

    /// Name this pass (`probe`, `main`, …) so several dumps in one run sort
    /// and read clearly.
    pub fn labelled(&self, label: impl Into<String>) -> Self {
        Self {
            dir: self.dir.clone(),
            label: label.into(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.dir.join(format!("{}-{suffix}", self.label))
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub binary: PathBuf,
    /// Tesseract `-l` value, e.g. `eng+deu`.
    pub languages: String,
    /// A pinned page segmentation mode; `None` runs the screenshot default and
    /// its single short-read fallback.
    pub psm: Option<u8>,
    /// Enlargement before recognition. Tesseract is happiest around 30 px cap
    /// height; ordinary game UI is not.
    pub scale: u32,
    /// Greyscale, stretch the histogram, then split at [`Config::threshold`]
    /// before recognition. The same crop that reads `AYancernrentS` raw reads
    /// `Advancements` binarised, at a twentieth of the cost of extra passes.
    pub binarize: bool,
    /// Luma split point for [`Config::binarize`], `1..=254`.
    pub threshold: u8,
    /// Repair tokens that are one or two edits from a known UI word. Off
    /// leaves the translator with exactly what Tesseract read.
    pub spellfix: bool,
    /// Lines below this mean confidence are dropped.
    pub min_confidence: f32,
    /// When set, write the OCR input and the raw TSV under this directory.
    pub debug: Option<Debug>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("tesseract"),
            languages: ALL_LANGUAGES.to_string(),
            psm: None,
            scale: 2,
            binarize: true,
            threshold: DEFAULT_THRESHOLD,
            spellfix: true,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            debug: None,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(value) = std::env::var(LANGUAGES_VARIABLE) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                config.languages = trimmed.to_string();
            }
        }
        if let Some(psm) = std::env::var(PSM_VARIABLE)
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok())
            .filter(|psm| *psm <= 13)
        {
            config.psm = Some(psm);
        }
        if let Some(scale) = std::env::var(SCALE_VARIABLE)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|scale| (1..=4).contains(scale))
        {
            config.scale = scale;
        }
        if let Some(binarize) = std::env::var(BINARIZE_VARIABLE)
            .ok()
            .and_then(|value| parse_switch(&value))
        {
            config.binarize = binarize;
        }
        if let Some(threshold) = std::env::var(THRESHOLD_VARIABLE)
            .ok()
            .and_then(|value| value.trim().parse::<u8>().ok())
            .filter(|threshold| (1..=254).contains(threshold))
        {
            config.threshold = threshold;
        }
        if let Some(spellfix) = std::env::var(SPELLFIX_VARIABLE)
            .ok()
            .and_then(|value| parse_switch(&value))
        {
            config.spellfix = spellfix;
        }
        if let Some(debug) = Debug::from_env() {
            config.debug = Some(debug);
        }
        config
    }

    pub fn with_languages(mut self, languages: impl Into<String>) -> Self {
        self.languages = languages.into();
        self
    }

    pub fn with_psm(mut self, psm: u8) -> Self {
        self.psm = Some(psm);
        self
    }

    pub fn with_binarize(mut self, binarize: bool) -> Self {
        self.binarize = binarize;
        self
    }

    pub fn with_threshold(mut self, threshold: u8) -> Self {
        self.threshold = threshold.clamp(1, 254);
        self
    }

    pub fn with_spellfix(mut self, spellfix: bool) -> Self {
        self.spellfix = spellfix;
        self
    }

    pub fn with_debug(mut self, debug: Option<Debug>) -> Self {
        self.debug = debug;
        self
    }
}

/// `1`/`0`, `true`/`false`, `on`/`off`, `yes`/`no` — anything else is ignored
/// so a typo leaves the default in place rather than silently disabling the
/// step that makes the crop readable.
fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug)]
pub enum OcrError {
    /// The binary is not installed.
    NotInstalled { binary: PathBuf },
    /// Tesseract ran but failed.
    Failed { status: Option<i32>, stderr: String },
    /// The process could not be started for another reason.
    Spawn(String),
    /// The capture could not be read or enlarged.
    Image(String),
}

impl fmt::Display for OcrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OcrError::NotInstalled { binary } => write!(
                f,
                "OCR engine not configured: '{}' not found in PATH (install tesseract-ocr and the language packs)",
                binary.display()
            ),
            OcrError::Failed { status, stderr } => {
                let status = status
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                if stderr.is_empty() {
                    write!(f, "OCR failed (exit {status})")
                } else {
                    write!(f, "OCR failed (exit {status}): {stderr}")
                }
            }
            OcrError::Spawn(message) => write!(f, "could not run the OCR engine: {message}"),
            OcrError::Image(message) => {
                write!(f, "could not prepare the capture for OCR: {message}")
            }
        }
    }
}

impl std::error::Error for OcrError {}

/// Run Tesseract over `image` and return the recognised lines, in reading order.
///
/// Kept as the simple entry point; [`recognize_reading`] also reports how many
/// lines were found and dropped, which the status line needs.
pub fn recognize(image: &Path, config: &Config) -> Result<Vec<TextBlock>, OcrError> {
    recognize_reading(image, config).map(|reading| reading.blocks)
}

/// Read `image`, running the screenshot page segmentation mode and, only when
/// that keeps too little, the sparse-text fallback.
pub fn recognize_reading(image: &Path, config: &Config) -> Result<Reading, OcrError> {
    let prepared = Prepared::open(image, config)?;
    let candidates: Vec<u8> = match config.psm {
        Some(psm) => vec![psm],
        None => vec![DEFAULT_PSM],
    };
    let mut attempts: Vec<Reading> = Vec::new();
    let mut first_error = None;
    for psm in candidates.iter().copied() {
        // A missing language pack or a Tesseract without `osd` must not fail a
        // capture that another mode can still read.
        match run_pass(&prepared, config, psm) {
            Ok(reading) => attempts.push(reading),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    let kept: usize = attempts.iter().map(Reading::kept).sum();
    if config.psm.is_none() && kept < FALLBACK_BELOW_LINES {
        for psm in FALLBACK_PSMS {
            if let Ok(reading) = run_pass(&prepared, config, psm) {
                attempts.push(reading);
            }
        }
    }
    if attempts.is_empty() {
        return Err(first_error.unwrap_or(OcrError::Spawn("no reading produced".to_string())));
    }

    let mut chosen = choose(&attempts).unwrap_or_default();
    chosen.attempts = attempts.iter().map(Attempt::of).collect();
    Ok(chosen)
}

impl Attempt {
    fn of(reading: &Reading) -> Self {
        Self {
            psm: reading.psm,
            found: reading.found,
            kept: reading.kept(),
            mean_confidence: reading.mean_confidence(),
            elapsed_ms: reading.elapsed_ms,
        }
    }
}

/// Pick the reading with the most credible text: longer, more confident lines
/// count for more than a pile of two-letter fragments.
pub fn choose(readings: &[Reading]) -> Option<Reading> {
    readings
        .iter()
        .max_by(|left, right| {
            score(left)
                .partial_cmp(&score(right))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.kept().cmp(&right.kept()))
                .then_with(|| {
                    left.mean_confidence()
                        .partial_cmp(&right.mean_confidence())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        })
        .cloned()
}

fn score(reading: &Reading) -> f32 {
    reading
        .blocks
        .iter()
        .map(|block| {
            let letters = block.text.chars().filter(|c| c.is_alphanumeric()).count() as f32;
            // Letters count for more than linearly, so a dozen two-letter
            // fragments do not outvote a real menu label.
            (block.confidence.max(0.0) / 100.0) * letters.powf(1.5)
        })
        .sum()
}

/// The image Tesseract is actually pointed at, and how it maps back.
struct Prepared {
    path: PathBuf,
    /// Kept alive for the duration of the passes; removing it would unlink the
    /// file Tesseract is about to read.
    _temp: Option<tempfile::NamedTempFile>,
    width: u32,
    height: u32,
    scale: u32,
}

impl Prepared {
    fn open(image: &Path, config: &Config) -> Result<Self, OcrError> {
        let scale = config.scale.max(1);
        let opened = image::open(image).map_err(|error| OcrError::Image(error.to_string()))?;
        let (width, height) = (opened.width(), opened.height());
        if scale == 1 && !config.binarize {
            // Still decode once: it validates the capture before Tesseract is
            // asked to, and gives the dump its dimensions.
            return Ok(Self {
                path: image.to_path_buf(),
                _temp: None,
                width,
                height,
                scale,
            });
        }

        let prepared = if scale > 1 {
            // Nearest neighbour, not Lanczos: a pixel font is already a grid of
            // hard-edged blocks, and interpolation turns each stroke into a
            // grey ramp that the threshold then erodes unevenly (`B`/`G`/`F`
            // become each other). Exact doubling keeps every block exact.
            opened.resize_exact(width * scale, height * scale, FilterType::Nearest)
        } else {
            opened
        };
        let prepared = if config.binarize {
            // Game UI is anti-aliased light text on a dark panel. Greyscale,
            // stretch the histogram so a dim capture still uses the full range,
            // then split hard: Tesseract reads the edges of soft glyphs as
            // strokes and returns `AYancernrentS` for `Advancements`.
            let mut luma = prepared.to_luma8();
            autocontrast(&mut luma);
            split_at(&mut luma, config.threshold);
            image::DynamicImage::ImageLuma8(luma)
        } else {
            prepared
        };

        let mut temp = tempfile::Builder::new()
            .prefix("cosmic-screenshot-ocr-")
            .suffix(".png")
            .tempfile()
            .map_err(|error| OcrError::Image(error.to_string()))?;
        prepared
            .write_to(temp.as_file_mut(), image::ImageFormat::Png)
            .map_err(|error| OcrError::Image(error.to_string()))?;
        Ok(Self {
            path: temp.path().to_path_buf(),
            _temp: Some(temp),
            width,
            height,
            scale,
        })
    }
}

/// Stretch the luma histogram to the full 0–255 range, so the threshold sees a
/// dim capture with the same contrast as a bright one.
fn autocontrast(image: &mut image::GrayImage) {
    let mut histogram = [0u32; 256];
    for pixel in image.pixels() {
        histogram[pixel.0[0] as usize] += 1;
    }
    let (Some(low), Some(high)) = (
        histogram.iter().position(|count| *count > 0),
        histogram.iter().rposition(|count| *count > 0),
    ) else {
        return;
    };
    if high <= low {
        return;
    }
    let span = (high - low) as f32;
    let mut lut = [0u8; 256];
    for (value, mapped) in lut.iter_mut().enumerate() {
        *mapped = ((value.saturating_sub(low) as f32 / span) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    for pixel in image.pixels_mut() {
        pixel.0[0] = lut[pixel.0[0] as usize];
    }
}

/// Hard split: at or above the threshold is paper, below it is ink.
fn split_at(image: &mut image::GrayImage, threshold: u8) {
    for pixel in image.pixels_mut() {
        pixel.0[0] = if pixel.0[0] >= threshold { 255 } else { 0 };
    }
}

fn run_pass(prepared: &Prepared, config: &Config, psm: u8) -> Result<Reading, OcrError> {
    let started = Instant::now();
    let output = Command::new(&config.binary)
        .arg(&prepared.path)
        .arg("stdout")
        .arg("-l")
        .arg(&config.languages)
        .arg("--psm")
        .arg(psm.to_string())
        .arg("tsv")
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                OcrError::NotInstalled {
                    binary: config.binary.clone(),
                }
            } else {
                OcrError::Spawn(error.to_string())
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(OcrError::Failed {
            status: output.status.code(),
            stderr: last_line(&stderr),
        });
    }

    let tsv = String::from_utf8_lossy(&output.stdout);
    let (parsed, corrections) = parse_tsv_with(&tsv, config.spellfix);
    let found = parsed.len();
    let mut blocks = Vec::with_capacity(found);
    let mut low_confidence = 0;
    let mut unreadable = 0;
    for mut block in parsed {
        block.bbox = block.bbox.scaled(prepared.scale);
        if block.confidence < config.min_confidence {
            low_confidence += 1;
        } else if !is_readable(&block.text) {
            unreadable += 1;
        } else {
            blocks.push(block);
        }
    }

    let reading = Reading {
        blocks,
        found,
        dropped_low_confidence: low_confidence,
        dropped_unreadable: unreadable,
        psm,
        languages: config.languages.clone(),
        scale: prepared.scale,
        elapsed_ms: started.elapsed().as_millis(),
        attempts: Vec::new(),
        corrections,
    };

    if let Some(debug) = &config.debug {
        // Diagnostics must never fail a capture the user asked for: a full
        // disk loses the dump, not the translation.
        let stem = format!("psm{psm}");
        let note = |result: Result<(), std::io::Error>| {
            if let Err(error) = result {
                eprintln!("cosmic-screenshot: could not write the OCR debug dump: {error}");
            }
        };
        note(std::fs::copy(&prepared.path, debug.path(&format!("{stem}-crop.png"))).map(|_| ()));
        note(std::fs::write(
            debug.path(&format!("{stem}.tsv")),
            tsv.as_bytes(),
        ));
        let summary = format!(
            "languages: {}\npsm: {psm}\nscale: {}\npreprocess: {}\ninput: {}x{} (enlarged {}x{})\nfound: {}\nkept: {}\ndropped (confidence < {}): {}\ndropped (unreadable): {}\ncorrected: {}\nelapsed: {} ms\n",
            config.languages,
            prepared.scale,
            if config.binarize {
                format!("binarised at {}", config.threshold)
            } else {
                "greyscale, no binarisation".to_string()
            },
            prepared.width,
            prepared.height,
            prepared.width * prepared.scale,
            prepared.height * prepared.scale,
            reading.found,
            reading.kept(),
            config.min_confidence,
            reading.dropped_low_confidence,
            reading.dropped_unreadable,
            reading.corrections.len(),
            started.elapsed().as_millis(),
        );
        note(std::fs::write(debug.path(&format!("{stem}.txt")), summary));
    }

    Ok(reading)
}

fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// Is this block worth translating? Real UI text has at least three letters
/// (or is a well-known two-letter label such as `OK`); a textured button can
/// make Tesseract emit `|||` or `}{`, and an icon can yield `Ei`, which must
/// not become "text".
fn is_readable(text: &str) -> bool {
    let mut letters = 0usize;
    let mut other = 0usize;
    for character in text.chars() {
        if character.is_alphanumeric() {
            letters += 1;
        } else if !character.is_whitespace() {
            other += 1;
        }
    }
    let enough = letters >= 3 || is_short_ui_label(text);
    enough && other * 2 <= letters + other
}

/// Two-letter UI words that are real labels, not fragments, across the
/// languages the tool translates. An icon read as `Ei` is not on this list.
const SHORT_LABELS: [&str; 13] = [
    "ok", "no", "on", "up", "go", "ja", "sí", "si", "da", "ya", "да", "не", "ні",
];

fn is_short_ui_label(text: &str) -> bool {
    let trimmed = text.trim_matches(|character: char| !character.is_alphanumeric());
    let lower = trimmed.to_lowercase();
    if SHORT_LABELS.contains(&lower.as_str()) {
        return true;
    }
    // Kana, Han and Hangul are written without spaces, so a two-character block
    // is a word (`はい`), not the kind of two-letter fragment `Ei` is.
    let characters: Vec<char> = trimmed.chars().collect();
    characters.len() == 2 && characters.iter().all(|character| is_cjk(*character))
}

/// One word Tesseract returned, kept with its own box so paint areas can be
/// built from words rather than from the line.
#[derive(Clone, Debug)]
struct Word {
    bbox: Bbox,
    /// Tesseract's word index within the line: reading order, which is not the
    /// same as left-to-right for a right-to-left script.
    number: u32,
    text: String,
    confidence: f32,
}

/// Parse Tesseract's TSV output (one row per word) into paint-sized blocks.
///
/// A block is a cluster of words, not a Tesseract line: the line box starts at
/// a button border when the border was read as a glyph, and psm 6 merges two
/// button columns into a single line. See [`parse_tsv_with`].
pub fn parse_tsv(tsv: &str) -> Vec<TextBlock> {
    parse_tsv_with(tsv, false).0
}

/// Parse the TSV, optionally repairing OCR typos, and report the repairs.
///
/// Level-5 rows are grouped by Tesseract's line key, then:
///
/// 1. tokens with no letters (`|`, `}`, `..`) are dropped — the first token on
///    a button line is often its border, which must not anchor the paint box;
/// 2. the remaining words are clustered by horizontal gap, so one line that
///    spans `Give Feedback` and `Report Bugs` becomes two blocks;
/// 3. each cluster's box is the union of its words, and its text is those
///    words in reading order.
pub fn parse_tsv_with(tsv: &str, spellfix: bool) -> (Vec<TextBlock>, Vec<Correction>) {
    type LineKey = (u32, u32, u32, u32);
    let mut lines: Vec<(LineKey, Vec<Word>)> = Vec::new();
    let mut index: HashMap<LineKey, usize> = HashMap::new();

    for row in tsv.lines() {
        let columns: Vec<&str> = row.split('\t').collect();
        if columns.len() < 12 {
            continue;
        }
        let level = match columns[0].parse::<u32>() {
            Ok(level) => level,
            Err(_) => continue, // header row
        };
        if level != 5 {
            continue;
        }
        let confidence = columns[10].parse::<f32>().unwrap_or(-1.0);
        let text = columns[11].trim();
        if text.is_empty() || confidence < 0.0 {
            continue;
        }
        let Ok(x) = columns[6].parse::<u32>() else {
            continue;
        };
        let Ok(y) = columns[7].parse::<u32>() else {
            continue;
        };
        let Ok(width) = columns[8].parse::<u32>() else {
            continue;
        };
        let Ok(height) = columns[9].parse::<u32>() else {
            continue;
        };
        let Ok(page) = columns[1].parse::<u32>() else {
            continue;
        };
        let Ok(block) = columns[2].parse::<u32>() else {
            continue;
        };
        let Ok(paragraph) = columns[3].parse::<u32>() else {
            continue;
        };
        let Ok(line) = columns[4].parse::<u32>() else {
            continue;
        };
        let Ok(word) = columns[5].parse::<u32>() else {
            continue;
        };

        let key = (page, block, paragraph, line);
        let position = *index.entry(key).or_insert_with(|| {
            lines.push((key, Vec::new()));
            lines.len() - 1
        });
        lines[position].1.push(Word {
            bbox: Bbox {
                x,
                y,
                width,
                height,
            },
            number: word,
            text: text.to_string(),
            confidence,
        });
    }

    let mut lines: Vec<Vec<Word>> = lines
        .into_iter()
        .map(|(_, mut words)| {
            // A token without a letter is decoration, not a label.
            words.retain(|word| word.text.chars().any(char::is_alphabetic));
            words
        })
        .filter(|words| !words.is_empty())
        .collect();

    // Two-word lines (one label per column) have no usable local median gap, so
    // they borrow the page's idea of a word gap.
    let global_gap = median_gap(lines.iter().map(Vec::as_slice));

    let mut blocks = Vec::new();
    let mut corrections = Vec::new();
    for words in &mut lines {
        words.sort_by_key(|word| (word.number, word.bbox.x));
        let tokens: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        if spellfix {
            let (repaired, fired) = spellfix::correct_line(&tokens);
            corrections.extend(fired);
            for (word, text) in words.iter_mut().zip(repaired) {
                word.text = text;
            }
        }

        words.sort_by_key(|word| (word.bbox.x, word.number));
        for cluster in cluster_line(words, global_gap) {
            let mut bbox: Option<Bbox> = None;
            let mut confidence = 0.0;
            let mut texts = Vec::with_capacity(cluster.len());
            for word in &cluster {
                match &mut bbox {
                    Some(existing) => existing.union(word.bbox),
                    None => bbox = Some(word.bbox),
                }
                confidence += word.confidence;
                texts.push(word.text.as_str());
            }
            let Some(bbox) = bbox else { continue };
            blocks.push(TextBlock {
                bbox,
                text: join_words(&texts),
                confidence: confidence / cluster.len().max(1) as f32,
            });
        }
    }

    // Reading order: top to bottom, then left to right.
    blocks.sort_by_key(|block| (block.bbox.y, block.bbox.x));
    (blocks, corrections)
}

/// Split a left-to-right-sorted line into label clusters.
///
/// A gap wider than twice the line's median word gap starts a new block; a gap
/// wider than 1.2 line heights always does, so two words on opposite sides of a
/// wide button are never painted as one. When a line has fewer than two gaps
/// (one or two words) there is no local median, so the page median is used,
/// capped at half the line height: on a page of two-column rows the page median
/// is the column gap itself, and 2× it would otherwise outvote the height floor.
fn cluster_line(words: &[Word], global_gap: Option<u32>) -> Vec<Vec<&Word>> {
    if words.is_empty() {
        return Vec::new();
    }
    let gaps: Vec<u32> = words
        .windows(2)
        .map(|pair| pair[1].bbox.x.saturating_sub(pair[0].bbox.right()))
        .collect();
    let height = median_value(words.iter().map(|word| word.bbox.height)).unwrap_or(1);
    let basis = if gaps.len() >= 2 {
        // The line's own word spacing: 2× this is the split point, so a label
        // whose words are widely spaced stays one block (its median is wide
        // too). The lower median keeps one column gap from setting the pace.
        median_value(gaps.iter().copied())
    } else {
        // A page whose rows are mostly two columns has a page median that *is*
        // the column gap. Cap only this borrowed median at half the line
        // height, so 2×basis cannot outvote the 1.2×height floor.
        global_gap.map(|gap| gap.min(height / 2))
    }
    .unwrap_or(height / 2)
    .max(1);
    let split = ((basis as f32 * 2.0).max(height as f32 * 1.2)).round() as u32;

    let mut clusters: Vec<Vec<&Word>> = Vec::new();
    let mut current = vec![&words[0]];
    for (position, pair) in words.windows(2).enumerate() {
        if gaps[position] > split {
            clusters.push(std::mem::take(&mut current));
        }
        current.push(&pair[1]);
    }
    clusters.push(current);
    for cluster in &mut clusters {
        cluster.sort_by_key(|word| word.number);
    }
    clusters
}

fn median_gap<'a>(lines: impl Iterator<Item = &'a [Word]>) -> Option<u32> {
    let mut gaps = Vec::new();
    for words in lines {
        let mut ordered: Vec<&Word> = words.iter().collect();
        ordered.sort_by_key(|word| (word.bbox.x, word.number));
        gaps.extend(
            ordered
                .windows(2)
                .map(|pair| pair[1].bbox.x.saturating_sub(pair[0].bbox.right())),
        );
    }
    median_value(gaps.into_iter())
}

fn median_value(values: impl Iterator<Item = u32>) -> Option<u32> {
    let mut values: Vec<u32> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    // Lower median: with two gaps, one of which is the gap between columns, the
    // smaller one is the word gap we want to measure against.
    Some(values[(values.len() - 1) / 2])
}

/// Join OCR words into a line. CJK scripts have no inter-word spaces, so a
/// space is only inserted when either side of the boundary is non-CJK.
fn join_words(words: &[&str]) -> String {
    let mut joined = String::new();
    for (position, word) in words.iter().enumerate() {
        if position > 0 {
            let previous = joined.chars().next_back();
            let next = word.chars().next();
            if !(previous.is_some_and(is_cjk) && next.is_some_and(is_cjk)) {
                joined.push(' ');
            }
        }
        joined.push_str(word);
    }
    joined
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3040..=0x30FF     // kana
        | 0x3400..=0x4DBF   // CJK extension A
        | 0x4E00..=0x9FFF   // CJK unified ideographs
        | 0xF900..=0xFAFF   // compatibility ideographs
        | 0xFF65..=0xFF9F   // half-width kana
        | 0xAC00..=0xD7AF   // hangul syllables
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, confidence: f32) -> TextBlock {
        TextBlock {
            bbox: Bbox {
                x: 10,
                y: 10,
                width: 80,
                height: 20,
            },
            text: text.to_string(),
            confidence,
        }
    }

    fn reading(psm: u8, lines: Vec<TextBlock>) -> Reading {
        Reading {
            found: lines.len(),
            blocks: lines,
            psm,
            languages: "eng".to_string(),
            scale: 2,
            ..Reading::default()
        }
    }

    #[test]
    fn parses_words_into_lines() {
        let blocks = parse_tsv(include_str!("../tests/fixtures/sample.tsv"));
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert_eq!(blocks[0].text, "Guten Tag");
        assert_eq!(
            blocks[0].bbox,
            Bbox {
                x: 100,
                y: 50,
                width: 180,
                height: 24
            }
        );
        assert!((blocks[0].confidence - 92.5).abs() < 0.01);
        assert_eq!(blocks[1].text, "Wie geht es dir?");
        assert_eq!(blocks[1].bbox.y, 90);
    }

    #[test]
    fn paint_boxes_come_from_words_not_the_line() {
        let blocks = parse_tsv(include_str!("../tests/fixtures/menu-psm6.tsv"));
        let texts: Vec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Bane Menu",
                "Back to Game",
                "Give Feedback",
                "Report Bugs",
                "Advancements",
                "Statistics",
                "Options...",
                "Achievements",
                "Mods",
                "Disconnect",
            ]
        );
        assert!(
            blocks.iter().all(|block| !block.text.contains('|')),
            "a border glyph must never become text: {texts:?}"
        );

        // The dump's line box was x=342 w=1595, anchored on the button border.
        // The words start at 1167 and end at 1937.
        let back = &blocks[1];
        assert_eq!(back.bbox.x, 1167, "paint box must start at the first word");
        assert_eq!(
            back.bbox.width, 770,
            "and not include the border or the gap"
        );

        let on_row = |y: u32| blocks.iter().filter(|block| block.bbox.y == y).count();
        assert_eq!(
            on_row(930),
            2,
            "Give Feedback and Report Bugs are two buttons"
        );
        assert_eq!(
            on_row(1214),
            2,
            "Advancements and Statistics are two columns"
        );
        assert_eq!(on_row(1502), 2, "Options and Achievements are two columns");
    }

    #[test]
    fn a_page_of_two_column_rows_still_splits_them() {
        // Every row is two columns, so the page median gap is the column gap.
        // Without the height cap, 2 × 300 beats 1.2 × 198 and both rows merge.
        let row = |line: u32, y: u32| {
            format!(
                "5\t1\t1\t1\t{line}\t1\t400\t{y}\t800\t198\t79\tAdvancements\n\
                 5\t1\t1\t1\t{line}\t2\t1500\t{y}\t1049\t198\t77\tStatistics\n"
            )
        };
        let tsv = format!(
            "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n{}{}",
            row(1, 1214),
            row(2, 1502)
        );
        let blocks = parse_tsv(&tsv);
        assert_eq!(blocks.len(), 4, "{blocks:#?}");
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>(),
            ["Advancements", "Statistics", "Advancements", "Statistics"]
        );
    }

    #[test]
    fn widely_spaced_words_of_one_label_stay_one_block() {
        // A pixel font can put 240 px between words on a 198 px line; the line's
        // own median gap is that wide, so 2×median keeps `Back to Game` whole.
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t100\t198\t85\tBack\n\
                   5\t1\t1\t1\t1\t2\t340\t0\t90\t198\t91\tto\n\
                   5\t1\t1\t1\t1\t3\t670\t0\t120\t198\t87\tGame\n";
        let blocks = parse_tsv(tsv);
        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        assert_eq!(blocks[0].text, "Back to Game");
    }

    #[test]
    fn three_words_on_one_line_still_split_at_a_column_gap() {
        // Two words of one label and one word of the next column: with only two
        // gaps, the upper median would be the column gap itself and nothing
        // would split. The lower median keeps the word gap as the measure.
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t100\t100\t90\tGive\n\
                   5\t1\t1\t1\t1\t2\t150\t0\t300\t100\t90\tFeedback\n\
                   5\t1\t1\t1\t1\t3\t800\t0\t250\t100\t90\tReport\n";
        let blocks = parse_tsv(tsv);
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert_eq!(blocks[0].text, "Give Feedback");
        assert_eq!(blocks[1].text, "Report");
    }

    #[test]
    fn the_corrector_repairs_menu_tokens_and_is_reported() {
        let tsv = include_str!("../tests/fixtures/menu-psm6.tsv");
        let (blocks, corrections) = parse_tsv_with(tsv, true);
        assert_eq!(blocks[0].text, "Game Menu");
        assert_eq!(
            corrections,
            vec![Correction {
                from: "Bane".to_string(),
                to: "Game".to_string(),
                distance: 2,
            }],
            "only the ambiguous title token needs the phrase to settle it"
        );

        let (plain, none) = parse_tsv_with(tsv, false);
        assert_eq!(plain[0].text, "Bane Menu");
        assert!(none.is_empty());
    }

    #[test]
    fn two_letter_fragments_are_not_text_but_real_short_labels_are() {
        assert!(!is_readable("Ei"), "a mod icon is not a menu label");
        assert!(!is_readable("|"));
        assert!(is_readable("OK"));
        assert!(is_readable("No"));
        assert!(is_readable("Back to Game"));
        // Short labels in the other supported languages are words, not junk.
        for label in ["Ja", "Sí", "Da", "Ya", "Да", "Не", "Ні", "はい", "是的"] {
            assert!(is_readable(label), "{label} is a real label");
        }
    }

    #[test]
    fn the_corrector_can_be_switched_off_from_the_environment() {
        unsafe { std::env::set_var(SPELLFIX_VARIABLE, "off") };
        assert!(!Config::from_env().spellfix);
        unsafe {
            std::env::set_var(SPELLFIX_VARIABLE, "on");
            assert!(Config::from_env().spellfix);
            std::env::remove_var(SPELLFIX_VARIABLE);
        }
        assert!(Config::from_env().spellfix, "on by default");
    }

    #[test]
    fn ignores_header_low_confidence_and_empty_words() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t95\t \n\
                   5\t1\t1\t1\t1\t3\t0\t0\t10\t10\t95\tHi\n";
        let blocks = parse_tsv(tsv);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Hi");
        assert!((blocks[0].confidence - 95.0).abs() < 0.01);
    }

    #[test]
    fn joins_cjk_without_spaces() {
        assert_eq!(join_words(&["日本", "語"]), "日本語");
        assert_eq!(join_words(&["Hello", "世界"]), "Hello 世界");
        assert_eq!(join_words(&["Hello", "world"]), "Hello world");
    }

    #[test]
    fn symbol_soup_is_not_readable_text() {
        assert!(is_readable("Back to Game"));
        assert!(is_readable("OK"));
        assert!(is_readable("Spielemenü"));
        assert!(is_readable("日本語"));
        assert!(is_readable("Options..."));
        assert!(!is_readable("|||"));
        assert!(!is_readable("}{"));
        assert!(!is_readable(".."));
        assert!(!is_readable("o"));
        assert!(
            !is_readable("I}.1.I}..I}"),
            "a textured button must not become a translated line"
        );
    }

    #[test]
    fn the_reading_with_more_credible_text_wins() {
        let sparse_one_line = reading(11, vec![line("Game Menu", 96.0)]);
        let block_nine = reading(
            6,
            vec![
                line("Back to Game", 84.0),
                line("Advancements", 82.0),
                line("Statistics", 80.0),
                line("Give Feedback", 78.0),
                line("Options...", 86.0),
                line("Mods", 74.0),
                line("Disconnect", 83.0),
            ],
        );
        let chosen = choose(&[sparse_one_line, block_nine]).expect("a reading");
        assert_eq!(chosen.psm, 6, "the pass that found the menu labels wins");
    }

    #[test]
    fn fragments_do_not_beat_real_lines() {
        let mut fragments = Vec::new();
        for _ in 0..12 {
            fragments.push(line("oh", 60.0));
        }
        let real = reading(11, vec![line("Back to Game", 80.0), line("Options", 78.0)]);
        let chosen = choose(&[reading(6, fragments), real]).expect("a reading");
        assert_eq!(chosen.kept(), 2);
    }

    #[test]
    fn no_candidates_is_an_empty_reading() {
        assert!(choose(&[]).is_none());
    }

    #[test]
    fn boxes_map_back_from_the_enlarged_image() {
        let boxed = Bbox {
            x: 21,
            y: 41,
            width: 201,
            height: 39,
        };
        assert_eq!(
            boxed.scaled(2),
            Bbox {
                x: 11,
                y: 21,
                width: 101,
                height: 20
            }
        );
        assert_eq!(boxed.scaled(1), boxed);
    }

    #[test]
    fn missing_binary_is_a_clear_one_liner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = dir.path().join("tiny.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 255]))
            .save(&image)
            .expect("write test image");
        let config = Config {
            binary: PathBuf::from("tesseract-does-not-exist"),
            ..Config::default()
        };
        let error = recognize(&image, &config).unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with("OCR engine not configured:"),
            "{message}"
        );
        assert!(!message.contains('\n'));
    }

    #[test]
    fn reads_languages_from_the_environment() {
        unsafe { std::env::set_var(LANGUAGES_VARIABLE, "eng+deu") };
        assert_eq!(Config::from_env().languages, "eng+deu");
        unsafe { std::env::remove_var(LANGUAGES_VARIABLE) };
        assert_eq!(Config::from_env().languages, ALL_LANGUAGES);
    }

    #[test]
    fn scale_and_psm_can_be_pinned_for_tuning() {
        unsafe {
            std::env::set_var(SCALE_VARIABLE, "3");
            std::env::set_var(PSM_VARIABLE, "6");
        }
        let config = Config::from_env();
        assert_eq!(config.scale, 3);
        assert_eq!(config.psm, Some(6));
        unsafe {
            std::env::set_var(SCALE_VARIABLE, "99");
            std::env::set_var(PSM_VARIABLE, "99");
            std::env::remove_var(PSM_VARIABLE);
            std::env::remove_var(SCALE_VARIABLE);
        }
        let config = Config::from_env();
        assert_eq!(config.scale, 2, "out of range values keep the default");
        assert_eq!(config.psm, None);
    }

    #[test]
    fn autocontrast_stretches_the_histogram_before_the_split() {
        let mut image = image::GrayImage::from_raw(4, 1, vec![100, 120, 140, 160]).expect("image");
        autocontrast(&mut image);
        let stretched: Vec<u8> = image.pixels().map(|pixel| pixel.0[0]).collect();
        assert_eq!(stretched, vec![0, 85, 170, 255]);

        split_at(&mut image, DEFAULT_THRESHOLD);
        let split: Vec<u8> = image.pixels().map(|pixel| pixel.0[0]).collect();
        assert_eq!(split, vec![0, 0, 255, 255]);
    }

    #[test]
    fn a_flat_image_survives_autocontrast_unchanged() {
        let mut image = image::GrayImage::from_raw(2, 1, vec![128, 128]).expect("image");
        autocontrast(&mut image);
        assert_eq!(
            image.pixels().map(|pixel| pixel.0[0]).collect::<Vec<_>>(),
            vec![128, 128]
        );
    }

    #[test]
    fn the_prepared_crop_is_binarised_and_enlarged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = dir.path().join("panel.png");
        // Light glyph on a dark panel, like a menu label.
        image::RgbaImage::from_raw(2, 1, vec![40, 40, 40, 255, 220, 220, 220, 255])
            .expect("image")
            .save(&image)
            .expect("write test image");

        let prepared = Prepared::open(&image, &Config::default()).expect("prepare");
        assert_eq!((prepared.width, prepared.height, prepared.scale), (2, 1, 2));
        let opened = image::open(&prepared.path).expect("read prepared");
        assert_eq!((opened.width(), opened.height()), (4, 2));
        let luma = opened.to_luma8();
        assert!(
            luma.pixels()
                .all(|pixel| pixel.0[0] == 0 || pixel.0[0] == 255),
            "every pixel handed to Tesseract must be one side of the threshold"
        );
        assert!(
            luma.pixels().any(|pixel| pixel.0[0] == 255),
            "the light glyph must survive the threshold"
        );
    }

    #[test]
    fn preprocessing_can_be_disabled_and_the_threshold_tuned() {
        unsafe {
            std::env::set_var(BINARIZE_VARIABLE, "off");
            std::env::set_var(THRESHOLD_VARIABLE, "170");
        }
        let config = Config::from_env();
        assert!(!config.binarize);
        assert_eq!(config.threshold, 170);
        unsafe {
            std::env::set_var(BINARIZE_VARIABLE, "maybe");
            std::env::set_var(THRESHOLD_VARIABLE, "300");
        }
        let config = Config::from_env();
        assert!(config.binarize, "an unparseable switch keeps the default");
        assert_eq!(config.threshold, DEFAULT_THRESHOLD);
        unsafe {
            std::env::remove_var(BINARIZE_VARIABLE);
            std::env::remove_var(THRESHOLD_VARIABLE);
        }
    }

    #[test]
    fn no_preprocessing_at_scale_one_hands_over_the_original_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = dir.path().join("shot.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 255]))
            .save(&image)
            .expect("write test image");
        let config = Config::default().with_binarize(false);
        let prepared = Prepared::open(&image, &Config { scale: 1, ..config }).expect("prepare");
        assert_eq!(prepared.path, image);
        assert!(prepared._temp.is_none());
    }

    #[test]
    fn a_debug_directory_is_optional_and_never_panics() {
        assert!(Debug::in_directory("").is_none());
        let dir = tempfile::tempdir().expect("temp dir");
        let debug = Debug::in_directory(dir.path().to_str().unwrap()).expect("debug dir");
        assert!(debug.dir().is_dir());
        assert!(debug.dir().starts_with(dir.path()));
    }
}
