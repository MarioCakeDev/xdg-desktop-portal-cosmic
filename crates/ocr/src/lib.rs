//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at c89de943f3def0cf0884152afc148a6308526949 (crates/ocr).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! OCR of a captured region.
//!
//! Uses the `tesseract` CLI rather than a native binding: no leptonica build
//! step, the language packs are ordinary apt packages, and the process boundary
//! keeps a misbehaving OCR run from taking the UI with it. The TSV parser is
//! pure so it is tested against fixtures.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every language pack the tool is prepared to read. The install recipe ships
/// all of them; a narrower set can be selected with
/// `COSMIC_SCREENSHOT_OCR_LANGS=eng+deu` (faster on a small region).
pub const ALL_LANGUAGES: &str = "eng+deu+fra+spa+ita+por+nld+pol+rus+ukr+tur+ces+swe+dan+fin+ell+hun+ron+ind+jpn+chi_sim+chi_tra+kor+ara";

pub const LANGUAGES_VARIABLE: &str = "COSMIC_SCREENSHOT_OCR_LANGS";

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
}

/// One recognised line of text and where it sits in the image.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    pub bbox: Bbox,
    pub text: String,
    /// Tesseract's mean word confidence, 0–100.
    pub confidence: f32,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub binary: PathBuf,
    /// Tesseract `-l` value, e.g. `eng+deu`.
    pub languages: String,
    /// Tesseract page segmentation mode.
    pub psm: u8,
    /// Lines below this mean confidence are dropped.
    pub min_confidence: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("tesseract"),
            languages: ALL_LANGUAGES.to_string(),
            psm: 3,
            min_confidence: 30.0,
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
        config
    }

    pub fn with_languages(mut self, languages: impl Into<String>) -> Self {
        self.languages = languages.into();
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
        }
    }
}

impl std::error::Error for OcrError {}

/// Run Tesseract over `image` and return the recognised lines, in reading order.
pub fn recognize(image: &Path, config: &Config) -> Result<Vec<TextBlock>, OcrError> {
    let output = Command::new(&config.binary)
        .arg(image)
        .arg("stdout")
        .arg("-l")
        .arg(&config.languages)
        .arg("--psm")
        .arg(config.psm.to_string())
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
    Ok(parse_tsv(&tsv)
        .into_iter()
        .filter(|block| block.confidence >= config.min_confidence)
        .filter(|block| !block.text.trim().is_empty())
        .collect())
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
    fn missing_binary_is_a_clear_one_liner() {
        let config = Config {
            binary: PathBuf::from("tesseract-does-not-exist"),
            ..Config::default()
        };
        let error = recognize(Path::new("/tmp/nope.png"), &config).unwrap_err();
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
}
