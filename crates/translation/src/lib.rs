//! VENDORED — do not edit here.
//!
//! Source: https://github.com/MarioCakeDev/cosmic-screenshot.git at 5a5c1987659adbec8d2d686f034a753ca6bb81bb (crates/translation).
//! Edit there, then run contrib/sync-engine.sh — see crates/VENDORED.md.
//!
//! Vendor-neutral machine translation.
//!
//! The overlay pipeline only ever talks to [`Translator`]. [`Provider`] names
//! the engines the configuration can express. The default is a local NLLB-200
//! sidecar ([`LocalTranslator`]) — purpose-built NMT, no API key, no cloud.
//! The DeepL client is kept behind the same trait for anyone who has a paid
//! DeepL plan. Adding another provider means adding one implementation of the
//! trait — nothing in the capture/OCR/overlay path changes.

use std::fmt;

pub mod deepl;
pub mod local;
pub mod testing;

pub use deepl::{DeepLHost, DeepLTranslator};
pub use local::LocalTranslator;

/// The engines this crate's configuration understands. [`Provider::LocalNmt`]
/// is the default; Google and Azure remain placeholders so the choice is
/// expressed in one enum rather than being spread through the pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    DeepL,
    GoogleCloud,
    Azure,
    #[default]
    LocalNmt,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::DeepL,
        Provider::GoogleCloud,
        Provider::Azure,
        Provider::LocalNmt,
    ];

    /// Stable identifier used by `TRANSLATE_PROVIDER`.
    pub fn id(self) -> &'static str {
        match self {
            Provider::DeepL => "deepl",
            Provider::GoogleCloud => "google",
            Provider::Azure => "azure",
            Provider::LocalNmt => "local-nmt",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::DeepL => "DeepL API",
            Provider::GoogleCloud => "Google Cloud Translation v3",
            Provider::Azure => "Azure AI Translator",
            Provider::LocalNmt => "local NMT (NLLB-200 / CTranslate2)",
        }
    }

    pub fn from_id(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deepl" | "deepl-api" | "deepl_api" => Some(Provider::DeepL),
            "google" | "google-cloud" | "google-translate" | "gcp" => Some(Provider::GoogleCloud),
            "azure" | "azure-translator" | "microsoft" => Some(Provider::Azure),
            "local" | "local-nmt" | "nllb" | "nllb-200" | "ctranslate2" => Some(Provider::LocalNmt),
            _ => None,
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A language the tool can translate *into*. Canonical form is a lowercase
/// two-letter code; provider-specific codes are derived in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lang(&'static str);

/// Canonical codes accepted by [`Lang::parse`]. The display names, Tesseract
/// packs and overlay scripts for these live in the application crate; this
/// list is the translation-side vocabulary.
pub const SUPPORTED: &[&str] = &[
    "en", "de", "fr", "es", "it", "pt", "nl", "pl", "ru", "uk", "tr", "cs", "sv", "da", "fi", "el",
    "hu", "ro", "ja", "zh", "ko", "ar", "id",
];

impl Lang {
    pub fn parse(value: &str) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        // Accept regional variants ("en-GB", "pt_BR") for the base language.
        let base = lower.split(['-', '_']).next().unwrap_or("");
        SUPPORTED
            .iter()
            .find(|code| **code == base)
            .map(|code| Lang(code))
    }

    pub fn code(self) -> &'static str {
        self.0
    }

    pub fn english_name(self) -> &'static str {
        match self.0 {
            "en" => "English",
            "de" => "German",
            "fr" => "French",
            "es" => "Spanish",
            "it" => "Italian",
            "pt" => "Portuguese",
            "nl" => "Dutch",
            "pl" => "Polish",
            "ru" => "Russian",
            "uk" => "Ukrainian",
            "tr" => "Turkish",
            "cs" => "Czech",
            "sv" => "Swedish",
            "da" => "Danish",
            "fi" => "Finnish",
            "el" => "Greek",
            "hu" => "Hungarian",
            "ro" => "Romanian",
            "ja" => "Japanese",
            "zh" => "Chinese",
            "ko" => "Korean",
            "ar" => "Arabic",
            "id" => "Indonesian",
            other => other,
        }
    }

    /// DeepL's `target_lang` / `source_lang` value.
    pub fn deepl_code(self) -> &'static str {
        match self.0 {
            "en" => "EN-US",
            "pt" => "PT-PT",
            "zh" => "ZH",
            "de" => "DE",
            "fr" => "FR",
            "es" => "ES",
            "it" => "IT",
            "nl" => "NL",
            "pl" => "PL",
            "ru" => "RU",
            "uk" => "UK",
            "tr" => "TR",
            "cs" => "CS",
            "sv" => "SV",
            "da" => "DA",
            "fi" => "FI",
            "el" => "EL",
            "hu" => "HU",
            "ro" => "RO",
            "ja" => "JA",
            "ko" => "KO",
            "ar" => "AR",
            "id" => "ID",
            other => other,
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// One translated text item, with the language the engine saw in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Translated {
    pub text: String,
    pub detected_source: Option<Lang>,
}

/// The source language an engine named, before any translation ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detected {
    pub lang: Lang,
    /// Detection was not trusted and this is the engine's configured default.
    /// The pipeline must not use an assumed language to narrow its OCR packs.
    pub assumed: bool,
}

/// A translation response, in the same order as the texts that were sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub items: Vec<Translated>,
    /// True when the engine could not confidently detect the source and used
    /// its default instead. The UI must say "assumed", not present it as a
    /// detection.
    pub source_assumed: bool,
}

impl Batch {
    /// The source language the engine detected most often across the batch.
    pub fn detected_source(&self) -> Option<Lang> {
        let mut counts: Vec<(Lang, usize)> = Vec::new();
        for item in &self.items {
            if let Some(lang) = item.detected_source {
                match counts.iter_mut().find(|(existing, _)| *existing == lang) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((lang, 1)),
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(lang, _)| lang)
    }
}

/// DeepL accepts at most 50 `text` items per request.
pub const MAX_ITEMS_PER_REQUEST: usize = 50;
/// DeepL rejects request bodies above 128 KiB; stay a little below.
pub const MAX_BYTES_PER_REQUEST: usize = 120 * 1024;

/// Split texts into request-sized ranges, respecting both the item count and
/// the body-size limit. A single text larger than the byte budget still gets
/// its own chunk so it fails at the engine rather than silently disappearing.
pub fn plan_chunks(texts: &[&str]) -> Vec<(usize, usize)> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < texts.len() {
        let mut end = start;
        let mut bytes = 0usize;
        while end < texts.len() && end - start < MAX_ITEMS_PER_REQUEST {
            let len = texts[end].len() + 8; // quotes + comma + escaping headroom
            if end > start && bytes + len > MAX_BYTES_PER_REQUEST {
                break;
            }
            bytes += len;
            end += 1;
        }
        chunks.push((start, end));
        start = end;
    }
    chunks
}

/// Anything the pipeline can hand back to the UI as one line.
#[derive(Debug)]
pub enum TranslateError {
    /// No credential in the environment: the engine cannot be used at all.
    NotConfigured { variable: &'static str },
    /// The provider is expressed in the enum but has no implementation yet.
    Unsupported { provider: Provider },
    /// Bad configuration value (not a secret).
    BadConfig {
        variable: &'static str,
        value: String,
        hint: &'static str,
    },
    /// The engine refused the credential (401/403).
    Auth { status: u16, detail: String },
    /// Character quota exhausted (DeepL: 456).
    Quota { detail: String },
    /// Rate limited (429).
    RateLimited,
    /// The engine is not running or cannot serve (sidecar down, model missing).
    Unavailable { detail: String },
    /// Any other non-success status.
    Rejected { status: u16, detail: String },
    /// Could not reach the engine.
    Network(String),
    /// The response could not be understood.
    Decode(String),
    /// The engine answered with a different number of items than requested.
    CountMismatch { requested: usize, received: usize },
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranslateError::NotConfigured { variable } => {
                write!(f, "translation engine not configured: set {variable}")
            }
            TranslateError::Unsupported { provider } => write!(
                f,
                "translation provider '{}' is not implemented yet; use TRANSLATE_PROVIDER=local-nmt (default) or TRANSLATE_PROVIDER=deepl",
                provider.id()
            ),
            TranslateError::BadConfig {
                variable,
                value,
                hint,
            } => write!(f, "invalid {variable}='{value}': {hint}"),
            TranslateError::Auth { status, detail } => {
                write!(
                    f,
                    "translation engine rejected the credential ({status}): {detail}"
                )
            }
            TranslateError::Quota { detail } => {
                write!(
                    f,
                    "translation engine character quota exhausted (456): {detail}"
                )
            }
            TranslateError::RateLimited => {
                f.write_str("translation engine rate limit hit (429); retry in a few seconds")
            }
            TranslateError::Unavailable { detail } => {
                write!(f, "translation engine not available: {detail}")
            }
            TranslateError::Rejected { status, detail } => {
                write!(
                    f,
                    "translation engine rejected the request ({status}): {detail}"
                )
            }
            TranslateError::Network(message) => {
                write!(f, "translation engine unreachable: {message}")
            }
            TranslateError::Decode(message) => {
                write!(
                    f,
                    "translation engine returned an unreadable response: {message}"
                )
            }
            TranslateError::CountMismatch {
                requested,
                received,
            } => write!(
                f,
                "translation engine returned {received} translations for {requested} text items"
            ),
        }
    }
}

impl std::error::Error for TranslateError {}

/// The one interface the overlay pipeline depends on.
pub trait Translator: Send + Sync {
    fn provider(&self) -> Provider;

    /// Translate `texts` into `target`, preserving order. Implementations
    /// batch internally within the provider's request limits.
    fn translate(
        &self,
        texts: &[&str],
        target: Lang,
        source: Option<Lang>,
    ) -> Result<Batch, TranslateError>;

    /// Detect the source language of `texts` without translating them.
    ///
    /// The OCR pipeline uses this for its second pass: once the language is
    /// known, the capture is re-read with that language's pack alone, which
    /// misreads accented words far less than all packs at once. Engines with
    /// no detector of their own (DeepL) return `None`, which skips that pass
    /// and leaves one-pass OCR unchanged.
    fn detect_source(&self, _texts: &[&str]) -> Result<Option<Detected>, TranslateError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_and_regional_codes() {
        assert_eq!(Lang::parse("DE").unwrap().code(), "de");
        assert_eq!(Lang::parse("en-GB").unwrap().code(), "en");
        assert_eq!(Lang::parse("pt_BR").unwrap().code(), "pt");
        assert_eq!(Lang::parse("zh-Hant").unwrap().code(), "zh");
        assert!(Lang::parse("xx").is_none());
        assert!(Lang::parse("").is_none());
    }

    #[test]
    fn every_supported_language_has_a_deepl_code() {
        for code in SUPPORTED {
            let lang = Lang::parse(code).expect("supported code parses");
            let deepl = lang.deepl_code();
            assert!(!deepl.is_empty(), "{code} has no DeepL code");
            assert!(
                deepl.chars().all(|c| c.is_ascii_uppercase() || c == '-'),
                "{code} -> {deepl} is not an uppercase DeepL code"
            );
        }
    }

    #[test]
    fn plans_one_chunk_for_a_small_batch() {
        let texts = ["a", "b", "c"];
        assert_eq!(plan_chunks(&texts), vec![(0, 3)]);
    }

    #[test]
    fn caps_chunks_at_fifty_items() {
        let texts: Vec<String> = (0..51).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        assert_eq!(plan_chunks(&refs), vec![(0, 50), (50, 51)]);
    }

    #[test]
    fn splits_on_the_byte_budget() {
        let big = "x".repeat(MAX_BYTES_PER_REQUEST - 4);
        let texts = [big.as_str(), "small"];
        assert_eq!(plan_chunks(&texts), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn empty_input_needs_no_chunks() {
        assert!(plan_chunks(&[]).is_empty());
    }

    #[test]
    fn provider_ids_round_trip() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_id(provider.id()), Some(provider));
        }
        assert_eq!(Provider::from_id("nllb"), Some(Provider::LocalNmt));
        assert_eq!(Provider::from_id("nope"), None);
    }

    #[test]
    fn the_local_engine_is_the_default() {
        assert_eq!(Provider::default(), Provider::LocalNmt);
        assert_eq!(Provider::default().id(), "local-nmt");
        assert_eq!(Provider::from_id("local-nmt"), Some(Provider::default()));
    }

    #[test]
    fn not_configured_is_one_clear_line() {
        let message = TranslateError::NotConfigured {
            variable: "DEEPL_API_KEY",
        }
        .to_string();
        assert_eq!(
            message,
            "translation engine not configured: set DEEPL_API_KEY"
        );
        assert!(!message.contains('\n'));
    }

    #[test]
    fn an_unavailable_engine_is_one_clear_line() {
        let message = TranslateError::Unavailable {
            detail: "NLLB model not found at /models/nllb".to_string(),
        }
        .to_string();
        assert_eq!(
            message,
            "translation engine not available: NLLB model not found at /models/nllb"
        );
        assert!(!message.contains('\n'));
    }

    #[test]
    fn detected_source_is_the_most_common() {
        let batch = Batch {
            items: vec![
                Translated {
                    text: "eins".into(),
                    detected_source: Lang::parse("de"),
                },
                Translated {
                    text: "zwei".into(),
                    detected_source: Lang::parse("de"),
                },
                Translated {
                    text: "three".into(),
                    detected_source: Lang::parse("en"),
                },
                Translated {
                    text: "?".into(),
                    detected_source: None,
                },
            ],
            source_assumed: false,
        };
        assert_eq!(batch.detected_source().map(Lang::code), Some("de"));
        assert!(!batch.source_assumed);
    }
}
