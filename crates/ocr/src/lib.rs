//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at 9865c731661914eea056d2fda4ef3db1cc50f52e (crates/ocr).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! OCR of a captured region.
//!
//! Uses the `tesseract` CLI rather than a native binding: no leptonica build
//! step, the language packs are ordinary apt packages, and the process boundary
//! keeps a misbehaving OCR run from taking the UI with it. The TSV parser is
//! pure so it is tested against fixtures.
//!
//! A screenshot is not a document. The defaults are tuned for one: lines are
//! found with the sparse-text page segmentation mode (11), the uniform-block
//! mode (6) and sparse text with orientation detection (12) — whichever keeps
//! the most credible text; the crop is enlarged before recognition, because
//! Tesseract wants roughly 30 px tall glyphs and a 20 px menu label is below
//! that; and a line made mostly of punctuation is dropped instead of being
//! handed to the translator. Nothing here invents text: every block comes from
//! a Tesseract pass over the pixels.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use image::imageops::FilterType;

/// Every language pack the tool is prepared to read. The install recipe ships
/// all of them; a narrower set can be selected with
/// `COSMIC_SCREENSHOT_OCR_LANGS=eng+deu` (faster on a small region).
pub const ALL_LANGUAGES: &str = "eng+deu+fra+spa+ita+por+nld+pol+rus+ukr+tur+ces+swe+dan+fin+ell+hun+ron+ind+jpn+chi_sim+chi_tra+kor+ara";

pub const LANGUAGES_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_LANGS";

/// Opt-in dump of everything one run handed to Tesseract, plus the TSV it
/// returned. Set it to a directory; each translation gets its own `run-*`
/// subdirectory there.
pub const DEBUG_DIR_VARIABLE: &str = "COSMIC_SCREENSHOT_DEBUG_DIR";

/// Pin one page segmentation mode instead of the built-in pair. Mostly for
/// tuning against a capture: the pipeline otherwise picks per image.
pub const PSM_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_PSM";

/// Enlargement factor applied before recognition (`1` disables it).
pub const SCALE_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_SCALE";

/// Below this mean word confidence a line is dropped.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 30.0;

/// Screenshot defaults: sparse text first, uniform block second, then sparse
/// text with orientation detection. The first pass that reads a full menu
/// confidently ends the search; otherwise all three are compared.
const DEFAULT_PSMS: [u8; 3] = [11, 6, 12];

/// Only tried when the screenshot modes find nothing at all.
const FALLBACK_PSMS: [u8; 1] = [3];

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
    /// A pinned page segmentation mode; `None` tries the screenshot pair and
    /// keeps the better reading.
    pub psm: Option<u8>,
    /// Enlargement before recognition. Tesseract is happiest around 30 px cap
    /// height; ordinary game UI is not.
    pub scale: u32,
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

    pub fn with_debug(mut self, debug: Option<Debug>) -> Self {
        self.debug = debug;
        self
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

/// Read `image`, trying the screenshot page segmentation modes and keeping the
/// most credible reading.
pub fn recognize_reading(image: &Path, config: &Config) -> Result<Reading, OcrError> {
    let prepared = Prepared::open(image, config.scale)?;
    let candidates: Vec<u8> = match config.psm {
        Some(psm) => vec![psm],
        None => DEFAULT_PSMS.to_vec(),
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
    if config.psm.is_none() && attempts.iter().all(|reading| reading.blocks.is_empty()) {
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
    fn open(image: &Path, scale: u32) -> Result<Self, OcrError> {
        let scale = scale.max(1);
        if scale == 1 {
            // Still decode once: it validates the capture before Tesseract is
            // asked to, and gives the dump its dimensions.
            let opened = image::open(image).map_err(|error| OcrError::Image(error.to_string()))?;
            return Ok(Self {
                path: image.to_path_buf(),
                _temp: None,
                width: opened.width(),
                height: opened.height(),
                scale,
            });
        }

        let opened = image::open(image).map_err(|error| OcrError::Image(error.to_string()))?;
        let (width, height) = (opened.width(), opened.height());
        let enlarged = opened.resize_exact(width * scale, height * scale, FilterType::Lanczos3);
        let mut temp = tempfile::Builder::new()
            .prefix("cosmic-screenshot-ocr-")
            .suffix(".png")
            .tempfile()
            .map_err(|error| OcrError::Image(error.to_string()))?;
        enlarged
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
    let parsed = parse_tsv(&tsv);
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
    };

    if let Some(debug) = &config.debug {
        let stem = format!("psm{psm}");
        let crop = debug.path(&format!("{stem}-crop.png"));
        std::fs::copy(&prepared.path, &crop).map_err(|error| OcrError::Image(error.to_string()))?;
        std::fs::write(debug.path(&format!("{stem}.tsv")), tsv.as_bytes())
            .map_err(|error| OcrError::Image(error.to_string()))?;
        let summary = format!(
            "languages: {}\npsm: {psm}\nscale: {}\ninput: {}x{} (enlarged {}x{})\nfound: {}\nkept: {}\ndropped (confidence < {}): {}\ndropped (unreadable): {}\nelapsed: {} ms\n",
            config.languages,
            prepared.scale,
            prepared.width,
            prepared.height,
            prepared.width * prepared.scale,
            prepared.height * prepared.scale,
            reading.found,
            reading.kept(),
            config.min_confidence,
            reading.dropped_low_confidence,
            reading.dropped_unreadable,
            started.elapsed().as_millis(),
        );
        std::fs::write(debug.path(&format!("{stem}.txt")), summary)
            .map_err(|error| OcrError::Image(error.to_string()))?;
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

/// Is this line worth translating? Real UI text has letters; a textured button
/// can make Tesseract emit `|||` or `}{`, which must not become "text".
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
    letters >= 2 && other * 2 <= letters + other
}

#[derive(Default)]
struct LineAccumulator {
    bbox: Option<Bbox>,
    words: Vec<(u32, String)>,
    confidence: f32,
}

/// Parse Tesseract's TSV output (one row per word) into line-level blocks.
pub fn parse_tsv(tsv: &str) -> Vec<TextBlock> {
    let mut lines: Vec<((u32, u32, u32, u32), LineAccumulator)> = Vec::new();
    let mut index: HashMap<(u32, u32, u32, u32), usize> = HashMap::new();

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
            lines.push((key, LineAccumulator::default()));
            lines.len() - 1
        });
        let accumulator = &mut lines[position].1;
        let bbox = Bbox {
            x,
            y,
            width,
            height,
        };
        match &mut accumulator.bbox {
            Some(existing) => existing.union(bbox),
            None => accumulator.bbox = Some(bbox),
        }
        accumulator.words.push((word, text.to_string()));
        accumulator.confidence += confidence;
    }

    let mut blocks: Vec<TextBlock> = lines
        .into_iter()
        .filter_map(|(_, mut accumulator)| {
            let bbox = accumulator.bbox?;
            accumulator.words.sort_by_key(|(word, _)| *word);
            let words: Vec<&str> = accumulator
                .words
                .iter()
                .map(|(_, word)| word.as_str())
                .collect();
            let count = words.len().max(1) as f32;
            Some(TextBlock {
                bbox,
                text: join_words(&words),
                confidence: accumulator.confidence / count,
            })
        })
        .collect();

    // Reading order: top to bottom, then left to right.
    blocks.sort_by_key(|block| (block.bbox.y, block.bbox.x));
    blocks
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
    fn a_debug_directory_is_optional_and_never_panics() {
        assert!(Debug::in_directory("").is_none());
        let dir = tempfile::tempdir().expect("temp dir");
        let debug = Debug::in_directory(dir.path().to_str().unwrap()).expect("debug dir");
        assert!(debug.dir().is_dir());
        assert!(debug.dir().starts_with(dir.path()));
    }
}
