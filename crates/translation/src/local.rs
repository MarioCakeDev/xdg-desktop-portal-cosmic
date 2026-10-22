//! Client for the local NLLB-200 sidecar (`sidecar/`).
//!
//! The sidecar owns the model, the tokenizer and language detection; this type
//! is deliberately only an HTTP client, so the desktop binary never links
//! CTranslate2 and runs on a machine that only has the Rust toolchain.
//!
//! One request carries every OCR line of one capture. That is not a stylistic
//! choice: CTranslate2 has a fixed ~200 ms per-call overhead, so a request per
//! line would pay it per line (5 lines 274 ms, 25 lines 634 ms).
//!
//! The sidecar takes canonical two-letter codes and maps them to NLLB tokens
//! itself — there is exactly one language table (the sidecar's), and a bad
//! code is rejected by the engine that has to translate it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Batch, Detected, Lang, Provider, TranslateError, Translated, Translator};

/// Base URL of the sidecar; the systemd user unit listens on the default.
pub const URL_VARIABLE: &str = "COSMIC_SCREENSHOT_NLLB_URL";
pub const DEFAULT_URL: &str = "http://127.0.0.1:8765";
const TIMEOUT_VARIABLE: &str = "COSMIC_SCREENSHOT_NLLB_TIMEOUT_SECS";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Serialize)]
struct RequestBody<'a> {
    lines: &'a [&'a str],
    target: &'a str,
    source: &'a str,
}

#[derive(Deserialize)]
struct ResponseBody {
    translations: Vec<String>,
    #[serde(default)]
    source: Option<String>,
    /// The sidecar could not trust detection and used its default source.
    #[serde(default)]
    fallback: bool,
}

/// The JSON body sent to `/translate`, exposed for fixture tests.
pub fn request_body(texts: &[&str], target: Lang, source: Option<Lang>) -> serde_json::Value {
    let body = RequestBody {
        lines: texts,
        target: target.code(),
        source: source.map(Lang::code).unwrap_or("auto"),
    };
    serde_json::to_value(body).expect("request body is serialisable")
}

/// Parse a `/translate` response into a [`Batch`].
pub fn parse_response(body: &str, requested: usize) -> Result<Batch, TranslateError> {
    let parsed: ResponseBody =
        serde_json::from_str(body).map_err(|error| TranslateError::Decode(error.to_string()))?;
    if parsed.translations.len() != requested {
        return Err(TranslateError::CountMismatch {
            requested,
            received: parsed.translations.len(),
        });
    }
    let detected = parsed.source.as_deref().and_then(Lang::parse);
    let items = parsed
        .translations
        .into_iter()
        .map(|text| Translated {
            text,
            detected_source: detected,
        })
        .collect();
    Ok(Batch {
        items,
        source_assumed: parsed.fallback,
    })
}

#[derive(Deserialize)]
struct DetectionBody {
    #[serde(default)]
    source: Option<String>,
    /// The sidecar could not trust detection and used its default source.
    #[serde(default)]
    fallback: bool,
}

/// Parse a `/detect` response into the language the engine named.
///
/// An unknown or missing code is `None`: the pipeline then keeps its one-pass
/// OCR instead of narrowing to a pack it cannot name.
pub fn parse_detection(body: &str) -> Result<Option<Detected>, TranslateError> {
    let parsed: DetectionBody =
        serde_json::from_str(body).map_err(|error| TranslateError::Decode(error.to_string()))?;
    Ok(parsed
        .source
        .as_deref()
        .and_then(Lang::parse)
        .map(|lang| Detected {
            lang,
            assumed: parsed.fallback,
        }))
}

/// Turn a non-success status into the one line the UI prints.
///
/// 503 means the sidecar is there but the engine is not (model missing, engine
/// failure) — the same "not available" class as a refused connection, so the
/// message starts the same way. 400 is a rejected request.
pub fn map_status(status: u16, body: &str) -> TranslateError {
    let detail = engine_error(body);
    if status == 503 {
        return TranslateError::Unavailable {
            detail: if detail.is_empty() {
                "the local NLLB sidecar cannot serve translations".to_string()
            } else {
                detail
            },
        };
    }
    TranslateError::Rejected { status, detail }
}

/// The sidecar answers errors as `{"error": "..."}`; flatten to one short line.
fn engine_error(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string());
    let single_line: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() > 240 {
        let mut truncated: String = single_line.chars().take(240).collect();
        truncated.push('…');
        truncated
    } else {
        single_line
    }
}

/// NLLB-200 sidecar behind [`Translator`].
pub struct LocalTranslator {
    agent: ureq::Agent,
    base_url: String,
}

impl LocalTranslator {
    pub fn new(base_url: impl Into<String>) -> Self {
        let timeout_secs = std::env::var(TIMEOUT_VARIABLE)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .build();
        Self {
            agent,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Build from `COSMIC_SCREENSHOT_NLLB_URL`, defaulting to the systemd unit.
    /// A malformed URL is a config error before any capture is taken.
    pub fn from_env() -> Result<Self, TranslateError> {
        let base_url = std::env::var(URL_VARIABLE)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(TranslateError::BadConfig {
                variable: URL_VARIABLE,
                value: base_url,
                hint: "use a base URL such as http://127.0.0.1:8765",
            });
        }
        let translator = Self::new(base_url);
        // Fail before the portal opens, with the sidecar's own one-line reason
        // (model missing, not started), not after the user selected a region.
        translator.check()?;
        Ok(translator)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn endpoint(&self) -> String {
        format!("{}/translate", self.base_url)
    }

    /// Is the sidecar up and serving? Checked once in [`from_env`], before a
    /// capture is taken, so a stopped service never costs the user a screenshot.
    pub fn check(&self) -> Result<(), TranslateError> {
        let url = format!("{}/health", self.base_url);
        match self.agent.get(&url).call() {
            Ok(response) => {
                let body = response
                    .into_string()
                    .map_err(|error| TranslateError::Decode(error.to_string()))?;
                let ready = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| value.get("ok").and_then(|ok| ok.as_bool()))
                    == Some(true);
                if ready {
                    Ok(())
                } else {
                    Err(TranslateError::Unavailable {
                        detail: format!(
                            "the local NLLB sidecar at {} is running but not ready",
                            self.base_url
                        ),
                    })
                }
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(map_status(status, &body))
            }
            Err(ureq::Error::Transport(_)) => Err(self.unreachable()),
        }
    }

    fn unreachable(&self) -> TranslateError {
        TranslateError::Unavailable {
            detail: format!(
                "cannot reach the local NLLB sidecar at {} — start it with \
                 'systemctl --user start cosmic-screenshot-nllb'",
                self.base_url
            ),
        }
    }
}

impl Translator for LocalTranslator {
    fn provider(&self) -> Provider {
        Provider::LocalNmt
    }

    fn translate(
        &self,
        texts: &[&str],
        target: Lang,
        source: Option<Lang>,
    ) -> Result<Batch, TranslateError> {
        if texts.is_empty() {
            return Ok(Batch::default());
        }
        let body = request_body(texts, target, source);
        let response = self
            .agent
            .post(&self.endpoint())
            .set("Content-Type", "application/json")
            .send_json(body);

        match response {
            Ok(response) => {
                let body = response
                    .into_string()
                    .map_err(|error| TranslateError::Decode(error.to_string()))?;
                parse_response(&body, texts.len())
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(map_status(status, &body))
            }
            Err(ureq::Error::Transport(_)) => Err(self.unreachable()),
        }
    }

    fn detect_source(&self, texts: &[&str]) -> Result<Option<Detected>, TranslateError> {
        if texts.is_empty() {
            return Ok(None);
        }
        let url = format!("{}/detect", self.base_url);
        let response = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(serde_json::json!({ "lines": texts }));

        match response {
            Ok(response) => {
                let body = response
                    .into_string()
                    .map_err(|error| TranslateError::Decode(error.to_string()))?;
                parse_detection(&body)
            }
            // A sidecar installed before two-pass OCR has no `/detect`. One OCR
            // pass is the old, working behaviour: degrade, do not fail.
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(map_status(status, &body))
            }
            Err(ureq::Error::Transport(_)) => Err(self.unreachable()),
        }
    }
}

impl std::fmt::Debug for LocalTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalTranslator")
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_carries_every_line_and_canonical_codes() {
        let body = request_body(&["Hallo", "Welt"], Lang::parse("en").unwrap(), None);
        assert_eq!(body["lines"], serde_json::json!(["Hallo", "Welt"]));
        assert_eq!(body["target"], "en");
        assert_eq!(body["source"], "auto");
    }

    #[test]
    fn an_explicit_source_is_sent_as_a_code() {
        let body = request_body(
            &["Hallo"],
            Lang::parse("en").unwrap(),
            Some(Lang::parse("de").unwrap()),
        );
        assert_eq!(body["source"], "de");
    }

    #[test]
    fn a_response_keeps_order_and_the_detected_source() {
        let body = r#"{"translations":["Hello","World"],"source":"de",
            "source_confidence":0.995,"detected":true,"fallback":false,"unchanged":false}"#;
        let batch = parse_response(body, 2).unwrap();
        assert_eq!(batch.items[0].text, "Hello");
        assert_eq!(batch.items[1].text, "World");
        assert_eq!(batch.detected_source().map(Lang::code), Some("de"));
        assert!(!batch.source_assumed);
    }

    #[test]
    fn a_low_confidence_fallback_is_not_reported_as_a_detection() {
        let body = r#"{"translations":["Hello"],"source":"en",
            "source_confidence":0.11,"detected":false,"fallback":true}"#;
        let batch = parse_response(body, 1).unwrap();
        assert!(batch.source_assumed, "the UI must be able to say 'assumed'");
        assert_eq!(batch.detected_source().map(Lang::code), Some("en"));
    }

    #[test]
    fn a_short_response_is_a_count_mismatch() {
        let error = parse_response(r#"{"translations":["only"]}"#, 3).unwrap_err();
        assert!(
            error.to_string().contains("1 translations for 3"),
            "{error}"
        );
    }

    #[test]
    fn a_detection_names_the_source_and_whether_it_was_trusted() {
        let detected = parse_detection(
            r#"{"source":"de","source_confidence":0.991,"detected":true,"fallback":false}"#,
        )
        .unwrap()
        .expect("a detection");
        assert_eq!(detected.lang.code(), "de");
        assert!(!detected.assumed);
    }

    #[test]
    fn a_fallback_detection_is_marked_assumed() {
        let detected = parse_detection(
            r#"{"source":"en","source_confidence":0.12,"detected":false,"fallback":true}"#,
        )
        .unwrap()
        .expect("a detection");
        assert_eq!(detected.lang.code(), "en");
        assert!(
            detected.assumed,
            "an assumed language must not narrow the OCR packs"
        );
    }

    #[test]
    fn an_unknown_detection_language_is_no_detection() {
        assert!(parse_detection(r#"{"source":"tlh"}"#).unwrap().is_none());
        assert!(parse_detection(r#"{}"#).unwrap().is_none());
    }

    #[test]
    fn a_malformed_detection_is_a_decode_error() {
        let error = parse_detection("not json").unwrap_err();
        assert!(matches!(error, TranslateError::Decode(_)), "{error}");
    }

    #[test]
    fn a_503_is_not_available_with_the_sidecar_message() {
        let error = map_status(
            503,
            r#"{"error":"NLLB model not found at /models/nllb","code":"engine_failed"}"#,
        );
        assert_eq!(
            error.to_string(),
            "translation engine not available: NLLB model not found at /models/nllb"
        );
    }

    #[test]
    fn a_400_is_rejected_without_a_language_list_wall() {
        let error = map_status(400, r#"{"error":"unknown target language 'tlh'"}"#);
        assert!(matches!(
            error,
            TranslateError::Rejected { status: 400, .. }
        ));
        assert_eq!(
            error.to_string(),
            "translation engine rejected the request (400): unknown target language 'tlh'"
        );
    }

    #[test]
    fn a_non_json_error_body_is_still_one_line() {
        let error = map_status(500, "<html>oops\nbroken</html>");
        assert!(!error.to_string().contains('\n'), "{error}");
    }
}
