//! Test doubles for the [`Translator`] trait.
//!
//! These live in the library (not a `cfg(test)` module) so integration tests in
//! the application crate and in this crate can use the same fake. They never
//! touch the network.

use std::sync::Mutex;

use crate::{Batch, Detected, Lang, Provider, TranslateError, Translated, Translator};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCall {
    pub texts: Vec<String>,
    pub target: Lang,
    pub source: Option<Lang>,
}

/// A translator that prefixes each text and records every call.
pub struct FakeTranslator {
    prefix: String,
    detected: Option<Lang>,
    assumed: bool,
    detection: Option<Detected>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl FakeTranslator {
    /// Prefix translations with `prefix`, e.g. `"[de] "`.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            detected: Lang::parse("en"),
            assumed: false,
            detection: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Return texts unchanged.
    pub fn echoing() -> Self {
        Self::new("")
    }

    /// Report a different detected source language.
    pub fn with_detected_language(mut self, lang: &str) -> Self {
        self.detected = Lang::parse(lang);
        self
    }

    /// Report that the source was assumed from a low-confidence detection.
    pub fn assuming_the_default_source(mut self) -> Self {
        self.assumed = true;
        self
    }

    /// Answer [`Translator::detect_source`] with `lang`, as the sidecar's
    /// `/detect` would. Without it the fake cannot detect, like DeepL.
    pub fn with_detection(mut self, lang: &str) -> Self {
        self.detection = Some(Detected {
            lang: Lang::parse(lang).expect("test language is supported"),
            assumed: false,
        });
        self
    }

    /// Answer detection with a low-confidence fallback, as the sidecar does.
    pub fn assuming_the_source_on_detection(mut self, lang: &str) -> Self {
        self.detection = Some(Detected {
            lang: Lang::parse(lang).expect("test language is supported"),
            assumed: true,
        });
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("call log poisoned").clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("call log poisoned").len()
    }
}

impl Translator for FakeTranslator {
    fn provider(&self) -> Provider {
        Provider::LocalNmt
    }

    fn translate(
        &self,
        texts: &[&str],
        target: Lang,
        source: Option<Lang>,
    ) -> Result<Batch, TranslateError> {
        self.calls
            .lock()
            .expect("call log poisoned")
            .push(RecordedCall {
                texts: texts.iter().map(|text| (*text).to_string()).collect(),
                target,
                source,
            });
        Ok(Batch {
            items: texts
                .iter()
                .map(|text| Translated {
                    text: format!("{}{}", self.prefix, text),
                    detected_source: self.detected,
                })
                .collect(),
            source_assumed: self.assumed,
        })
    }

    fn detect_source(&self, _texts: &[&str]) -> Result<Option<Detected>, TranslateError> {
        Ok(self.detection)
    }
}

/// A translator that behaves like an unconfigured engine.
pub struct UnconfiguredTranslator;

impl Translator for UnconfiguredTranslator {
    fn provider(&self) -> Provider {
        Provider::DeepL
    }

    fn translate(
        &self,
        _texts: &[&str],
        _target: Lang,
        _source: Option<Lang>,
    ) -> Result<Batch, TranslateError> {
        Err(TranslateError::NotConfigured {
            variable: crate::deepl::KEY_VARIABLE,
        })
    }
}
