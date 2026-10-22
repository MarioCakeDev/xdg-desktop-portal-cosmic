//! Application-side language table.
//!
//! One row per language the tool can translate *into*: the canonical code used
//! by the translation layer, the display name, the Tesseract pack that reads the
//! same language as a source, and the script the overlay font must cover.

use cosmic_screenshot_overlay::Script;
use cosmic_screenshot_translation::Lang;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LangInfo {
    pub code: &'static str,
    pub name: &'static str,
    /// Tesseract model name (as installed by the `tesseract-ocr-<name>` package).
    pub tesseract: &'static str,
    pub script: Script,
}

/// The dropdown shows one row per language; its label is the name.
impl std::fmt::Display for LangInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// `cosmic::widget::dropdown` renders any `T: AsRef<str>`, which is how the
/// dialog lists the 23 targets without building a `Vec<String>` every frame.
impl AsRef<str> for LangInfo {
    fn as_ref(&self) -> &str {
        self.name
    }
}

/// Keep this in step with `cosmic_screenshot_translation::SUPPORTED`; the test
/// at the bottom fails if the two drift apart.
pub const LANGS: &[LangInfo] = &[
    LangInfo {
        code: "en",
        name: "English",
        tesseract: "eng",
        script: Script::Latin,
    },
    LangInfo {
        code: "de",
        name: "German",
        tesseract: "deu",
        script: Script::Latin,
    },
    LangInfo {
        code: "fr",
        name: "French",
        tesseract: "fra",
        script: Script::Latin,
    },
    LangInfo {
        code: "es",
        name: "Spanish",
        tesseract: "spa",
        script: Script::Latin,
    },
    LangInfo {
        code: "it",
        name: "Italian",
        tesseract: "ita",
        script: Script::Latin,
    },
    LangInfo {
        code: "pt",
        name: "Portuguese",
        tesseract: "por",
        script: Script::Latin,
    },
    LangInfo {
        code: "nl",
        name: "Dutch",
        tesseract: "nld",
        script: Script::Latin,
    },
    LangInfo {
        code: "pl",
        name: "Polish",
        tesseract: "pol",
        script: Script::Latin,
    },
    LangInfo {
        code: "ru",
        name: "Russian",
        tesseract: "rus",
        script: Script::Cyrillic,
    },
    LangInfo {
        code: "uk",
        name: "Ukrainian",
        tesseract: "ukr",
        script: Script::Cyrillic,
    },
    LangInfo {
        code: "tr",
        name: "Turkish",
        tesseract: "tur",
        script: Script::Latin,
    },
    LangInfo {
        code: "cs",
        name: "Czech",
        tesseract: "ces",
        script: Script::Latin,
    },
    LangInfo {
        code: "sv",
        name: "Swedish",
        tesseract: "swe",
        script: Script::Latin,
    },
    LangInfo {
        code: "da",
        name: "Danish",
        tesseract: "dan",
        script: Script::Latin,
    },
    LangInfo {
        code: "fi",
        name: "Finnish",
        tesseract: "fin",
        script: Script::Latin,
    },
    LangInfo {
        code: "el",
        name: "Greek",
        tesseract: "ell",
        script: Script::Greek,
    },
    LangInfo {
        code: "hu",
        name: "Hungarian",
        tesseract: "hun",
        script: Script::Latin,
    },
    LangInfo {
        code: "ro",
        name: "Romanian",
        tesseract: "ron",
        script: Script::Latin,
    },
    LangInfo {
        code: "ja",
        name: "Japanese",
        tesseract: "jpn",
        script: Script::Cjk,
    },
    LangInfo {
        code: "zh",
        name: "Chinese",
        tesseract: "chi_sim+chi_tra",
        script: Script::Cjk,
    },
    LangInfo {
        code: "ko",
        name: "Korean",
        tesseract: "kor",
        script: Script::Korean,
    },
    LangInfo {
        code: "ar",
        name: "Arabic",
        tesseract: "ara",
        script: Script::Arabic,
    },
    LangInfo {
        code: "id",
        name: "Indonesian",
        tesseract: "ind",
        script: Script::Latin,
    },
];

pub fn find(code: &str) -> Option<&'static LangInfo> {
    LANGS.iter().find(|info| info.code == code)
}

/// Accept a code (`de`, `DE`, `pt-BR`) or an English name (`German`).
pub fn from_argument(value: &str) -> Option<&'static LangInfo> {
    Lang::parse(value)
        .and_then(|parsed| find(parsed.code()))
        .or_else(|| {
            let wanted = value.trim().to_ascii_lowercase();
            LANGS
                .iter()
                .find(|info| info.name.to_ascii_lowercase() == wanted)
        })
}

/// The translation-layer handle for a table row.
pub fn translation_lang(info: &LangInfo) -> Lang {
    Lang::parse(info.code).expect("language table entries are valid codes")
}

/// One line naming every accepted language, for error messages.
pub fn supported_list() -> String {
    LANGS
        .iter()
        .map(|info| format!("{} ({})", info.code, info.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Guess the user's language from the locale (`de_DE.UTF-8` → German).
pub fn from_locale() -> Option<&'static LangInfo> {
    let value = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .ok()?;
    from_locale_value(&value)
}

pub fn from_locale_value(value: &str) -> Option<&'static LangInfo> {
    let language = value
        .split(['.', '@'])
        .next()?
        .split(['_', '-'])
        .next()?
        .to_ascii_lowercase();
    find(&language)
}

/// The target language last picked in the portal's translate toolbar.
///
/// The dialog writes it into its own per-user state — `com.system76.CosmicPortal`,
/// version 1, key `screenshot.target_language` — beside `last_rectangle`, so it
/// survives a dialog close, a portal restart and a reboot. The CLI has no
/// cosmic-config dependency and must not grow a second writer, so it reads that
/// one key out of the same file. A missing file, a missing key, or a code that
/// is no longer in [`LANGS`] is `None`: the caller falls back to the locale,
/// exactly like a dialog that has never been used.
pub fn remembered_target() -> Option<&'static LangInfo> {
    remembered_target_in(&remembered_target_path()?)
}

/// The path the portal's `cosmic-config` uses for the screenshot state.
pub fn remembered_target_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("cosmic/com.system76.CosmicPortal/v1/screenshot"))
}

/// [`remembered_target`] against an explicit file, for tests and for callers
/// that already resolved the config path.
pub fn remembered_target_in(path: &std::path::Path) -> Option<&'static LangInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    find(stored_target_code(&text)?)
}

/// Pull `target_language: Some("de")` out of the config's RON.
///
/// Deliberately narrow: it reads one key, tolerates any whitespace the writer
/// chose, and returns `None` for anything else rather than guessing.
fn stored_target_code(text: &str) -> Option<&str> {
    let rest = text.split("target_language:").nth(1)?.trim_start();
    let rest = rest.strip_prefix("Some(")?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// Tesseract's `-l` value for all supported source languages. `eng` comes first
/// because Tesseract weights the first language; the rest is sorted for a
/// stable list.
pub fn all_ocr_languages() -> String {
    let mut names: Vec<&str> = LANGS.iter().map(|info| info.tesseract).collect();
    names.sort_unstable();
    names.dedup();
    if let Some(position) = names.iter().position(|name| *name == "eng") {
        let english = names.remove(position);
        names.insert(0, english);
    }
    names.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_screenshot_translation::SUPPORTED;

    #[test]
    fn every_supported_language_has_a_row() {
        assert_eq!(LANGS.len(), SUPPORTED.len());
        for code in SUPPORTED {
            assert!(find(code).is_some(), "no table row for {code}");
        }
    }

    #[test]
    fn codes_are_unique_and_parse() {
        let mut codes: Vec<&str> = LANGS.iter().map(|info| info.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), LANGS.len());
        for info in LANGS {
            assert!(Lang::parse(info.code).is_some(), "{}", info.code);
            assert!(!info.tesseract.is_empty(), "{}", info.code);
        }
    }

    #[test]
    fn arguments_accept_codes_and_names() {
        assert_eq!(from_argument("de").unwrap().code, "de");
        assert_eq!(from_argument("German").unwrap().code, "de");
        assert_eq!(from_argument("PT-br").unwrap().code, "pt");
        assert!(from_argument("klingon").is_none());
    }

    #[test]
    fn ocr_language_list_has_no_duplicates() {
        let list = all_ocr_languages();
        let parts: Vec<&str> = list.split('+').collect();
        let mut unique = parts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(parts.len(), unique.len(), "{list}");
        assert!(list.contains("eng") && list.contains("chi_tra"));
    }

    #[test]
    fn english_is_the_primary_ocr_language() {
        // Tesseract weighs the first `-l` entry; sorting alphabetically made
        // Arabic the primary language and degraded English captures.
        let list = all_ocr_languages();
        assert!(list.starts_with("eng+"), "{list}");
    }

    #[test]
    fn the_ocr_crate_default_matches_the_table() {
        let table_languages = all_ocr_languages();
        let mut table: Vec<&str> = table_languages.split('+').collect();
        let mut crate_default: Vec<&str> =
            cosmic_screenshot_ocr::ALL_LANGUAGES.split('+').collect();
        table.sort_unstable();
        crate_default.sort_unstable();
        assert_eq!(table, crate_default);
    }

    #[test]
    fn locale_guesses_a_supported_language() {
        assert_eq!(from_locale_value("de_DE.UTF-8").unwrap().code, "de");
        assert_eq!(from_locale_value("pt_BR.UTF-8").unwrap().code, "pt");
        assert_eq!(from_locale_value("ja_JP.UTF-8").unwrap().code, "ja");
        assert!(from_locale_value("C.UTF-8").is_none());
    }

    #[test]
    fn the_remembered_target_is_read_out_of_the_portal_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("screenshot");

        // The exact shape cosmic-config writes on maripop, with the new key.
        std::fs::write(
            &path,
            "(save_location: Clipboard, choice: Rectangle, last_rectangle: Some((left: 3001, \
             top: 65, right: 3840, bottom: 956)), target_language: Some(\"de\"))",
        )
        .expect("write config");
        assert_eq!(
            remembered_target_in(&path).map(|info| info.code),
            Some("de")
        );

        // Any spacing the writer chooses, and a never-picked language.
        std::fs::write(&path, "target_language:None").expect("write config");
        assert!(remembered_target_in(&path).is_none());
        std::fs::write(&path, "target_language: Some( \"ja\" )").expect("write config");
        assert_eq!(
            remembered_target_in(&path).map(|info| info.code),
            Some("ja")
        );

        // A code that is no longer in the table, and a file that is not there.
        std::fs::write(&path, "target_language: Some(\"tlh\")").expect("write config");
        assert!(remembered_target_in(&path).is_none(), "no stale index");
        assert!(remembered_target_in(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn a_stored_code_never_panics_on_garbage() {
        for garbage in [
            "",
            "target_language:",
            "target_language: Some(",
            "target_language: Some(\"",
            "last_rectangle: None",
        ] {
            assert!(stored_target_code(garbage).is_none(), "{garbage:?}");
        }
    }
}
