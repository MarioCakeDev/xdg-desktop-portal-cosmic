//! DeepL REST (`/v2/translate`) client.
//!
//! Deliberately thin: build the request body, send it, parse the response.
//! Everything around that (batching, language codes, errors) is testable
//! without the network, and the whole type sits behind [`Translator`].

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Batch, Lang, Provider, TranslateError, Translated, Translator, plan_chunks};

/// API Free plan endpoint (500k characters/month). Free-plan keys end in `:fx`
/// and only work against this host.
pub const FREE_HOST: &str = "https://api-free.deepl.com";
/// Paid (DeepL API Pro) endpoint — the default for non-`:fx` keys.
pub const PAID_HOST: &str = "https://api.deepl.com";

pub const KEY_VARIABLE: &str = "DEEPL_API_KEY";
pub const HOST_VARIABLE: &str = "DEEPL_HOST";
const TIMEOUT_VARIABLE: &str = "DEEPL_TIMEOUT_SECS";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Which DeepL endpoint to talk to. Getting this wrong is a 403, so it is
/// explicit configuration, never inferred from the key at request time. Without
/// `DEEPL_HOST`, a key ending in `:fx` is a free-plan key and selects the free
/// endpoint; every other key gets the paid endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeepLHost {
    /// API Free plan (`api-free.deepl.com`); `:fx` keys.
    Free,
    #[default]
    Paid,
    Custom(String),
}

impl DeepLHost {
    pub fn base_url(&self) -> &str {
        match self {
            DeepLHost::Free => FREE_HOST,
            DeepLHost::Paid => PAID_HOST,
            DeepLHost::Custom(url) => url,
        }
    }

    /// `DEEPL_HOST` accepts `paid` (default), `free` (API Free, `:fx` keys), a bare
    /// hostname, or a full base URL (the latter matters for proxies and tests).
    pub fn parse(value: &str) -> Result<Self, TranslateError> {
        let trimmed = value.trim().trim_end_matches('/');
        match trimmed.to_ascii_lowercase().as_str() {
            "" | "paid" | "pro" | "api" | "api.deepl.com" | "https://api.deepl.com" => {
                Ok(DeepLHost::Paid)
            }
            "free" | "api-free" | "api-free.deepl.com" | "https://api-free.deepl.com" => {
                Ok(DeepLHost::Free)
            }
            _ if trimmed.starts_with("http://") || trimmed.starts_with("https://") => {
                Ok(DeepLHost::Custom(trimmed.to_string()))
            }
            _ => Err(TranslateError::BadConfig {
                variable: HOST_VARIABLE,
                value: value.to_string(),
                hint: "use 'paid' (default), 'free' (API Free, :fx keys), or a full base URL such as https://api.deepl.com",
            }),
        }
    }

    /// The host implied by the key when `DEEPL_HOST` is not set. DeepL's own
    /// convention: free-plan keys end in `:fx`.
    pub fn for_key(key: &str) -> Self {
        if key.trim().ends_with(":fx") {
            DeepLHost::Free
        } else {
            DeepLHost::Paid
        }
    }

    /// `DEEPL_HOST` wins; otherwise derive the host from the key.
    pub fn from_env_for_key(key: &str) -> Result<Self, TranslateError> {
        match std::env::var(HOST_VARIABLE) {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            _ => Ok(Self::for_key(key)),
        }
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    text: &'a [&'a str],
    target_lang: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_lang: Option<&'a str>,
}

#[derive(Deserialize)]
struct ResponseBody {
    translations: Vec<ResponseItem>,
}

#[derive(Deserialize)]
struct ResponseItem {
    detected_source_language: String,
    text: String,
}

/// The JSON body sent to `/v2/translate`, exposed for fixture tests.
pub fn request_body(texts: &[&str], target: Lang, source: Option<Lang>) -> serde_json::Value {
    let body = RequestBody {
        text: texts,
        target_lang: target.deepl_code(),
        source_lang: source.map(Lang::deepl_code),
    };
    serde_json::to_value(body).expect("request body is serialisable")
}

/// Parse a `/v2/translate` response into a [`Batch`].
pub fn parse_response(body: &str, requested: usize) -> Result<Batch, TranslateError> {
    let parsed: ResponseBody =
        serde_json::from_str(body).map_err(|error| TranslateError::Decode(error.to_string()))?;
    if parsed.translations.len() != requested {
        return Err(TranslateError::CountMismatch {
            requested,
            received: parsed.translations.len(),
        });
    }
    let items = parsed
        .translations
        .into_iter()
        .map(|item| Translated {
            text: item.text,
            detected_source: normalise_language(&item.detected_source_language),
        })
        .collect();
    Ok(Batch {
        items,
        source_assumed: false,
    })
}

/// DeepL answers with codes such as `EN`, `PT-PT`, `ZH`; map them onto the
/// canonical vocabulary without failing on languages we do not target.
fn normalise_language(code: &str) -> Option<Lang> {
    Lang::parse(code)
}

/// Turn a non-success HTTP status into a message the UI can print on one line.
pub fn map_status(status: u16, body: &str) -> TranslateError {
    let detail = engine_message(body);
    match status {
        401 | 403 => TranslateError::Auth {
            status,
            detail: if detail.is_empty() {
                "DeepL refused the key; check DEEPL_API_KEY and DEEPL_HOST".to_string()
            } else {
                format!("{detail} (check DEEPL_API_KEY and DEEPL_HOST; a free key ends in :fx)")
            },
        },
        456 => TranslateError::Quota {
            detail: if detail.is_empty() {
                "the monthly character limit of the DeepL plan has been used up".to_string()
            } else {
                detail
            },
        },
        429 => TranslateError::RateLimited,
        _ => TranslateError::Rejected { status, detail },
    }
}

/// DeepL errors look like `{"message": "..."}`; keep it to one short line and
/// never echo anything that could contain credentials.
fn engine_message(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(|m| m.as_str())
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

/// DeepL behind [`Translator`].
pub struct DeepLTranslator {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
}

impl DeepLTranslator {
    pub fn new(host: DeepLHost, api_key: impl Into<String>) -> Self {
        let timeout_secs = std::env::var(TIMEOUT_VARIABLE)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .build();
        Self {
            agent,
            base_url: host.base_url().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Build from the environment. A missing or blank key is a clean
    /// [`TranslateError::NotConfigured`], not a panic.
    pub fn from_env() -> Result<Self, TranslateError> {
        let api_key = std::env::var(KEY_VARIABLE)
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .ok_or(TranslateError::NotConfigured {
                variable: KEY_VARIABLE,
            })?;
        Ok(Self::new(DeepLHost::from_env_for_key(&api_key)?, api_key))
    }

    pub fn endpoint(&self) -> String {
        format!("{}/v2/translate", self.base_url)
    }

    fn send_chunk(
        &self,
        texts: &[&str],
        target: Lang,
        source: Option<Lang>,
    ) -> Result<Batch, TranslateError> {
        let body = request_body(texts, target, source);
        let response = self
            .agent
            .post(&self.endpoint())
            .set("Authorization", &format!("DeepL-Auth-Key {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(body);

        match response {
            Ok(response) => {
                let body = response
                    .into_string()
                    .map_err(|error| TranslateError::Network(error.to_string()))?;
                parse_response(&body, texts.len())
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(map_status(status, &body))
            }
            Err(ureq::Error::Transport(error)) => Err(TranslateError::Network(error.to_string())),
        }
    }
}

impl Translator for DeepLTranslator {
    fn provider(&self) -> Provider {
        Provider::DeepL
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
        let mut items = Vec::with_capacity(texts.len());
        for (start, end) in plan_chunks(texts) {
            let chunk = self.send_chunk(&texts[start..end], target, source)?;
            items.extend(chunk.items);
        }
        // DeepL detects the source itself and always answers with it.
        Ok(Batch {
            items,
            source_assumed: false,
        })
    }
}

/// Never print the key, not even in a panic message.
impl fmt::Debug for DeepLTranslator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepLTranslator")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paid_host_is_the_default() {
        assert_eq!(DeepLHost::default(), DeepLHost::Paid);
        assert_eq!(DeepLHost::default().base_url(), "https://api.deepl.com");
        assert!(DeepLHost::Free.base_url().starts_with("https://api-free"));
    }

    #[test]
    fn a_free_key_selects_the_free_endpoint() {
        assert_eq!(DeepLHost::for_key("0000:fx"), DeepLHost::Free);
        assert_eq!(DeepLHost::for_key(" 0000:fx "), DeepLHost::Free);
        assert_eq!(DeepLHost::for_key("0000"), DeepLHost::Paid);
        // The paid endpoint stays the default when there is no key hint.
        assert_eq!(DeepLHost::default(), DeepLHost::Paid);
    }

    #[test]
    fn host_parsing_accepts_named_hosts_and_urls() {
        assert_eq!(DeepLHost::parse("paid").unwrap(), DeepLHost::Paid);
        assert_eq!(DeepLHost::parse("").unwrap(), DeepLHost::Paid);
        assert_eq!(
            DeepLHost::parse("https://api.deepl.com/").unwrap(),
            DeepLHost::Paid
        );
        assert_eq!(
            DeepLHost::parse("https://api-free.deepl.com").unwrap(),
            DeepLHost::Free
        );
        assert_eq!(
            DeepLHost::parse("http://127.0.0.1:9").unwrap(),
            DeepLHost::Custom("http://127.0.0.1:9".to_string())
        );
        assert!(DeepLHost::parse("deepl.example:4439").is_err());
    }

    #[test]
    fn request_body_carries_texts_and_codes() {
        let body = request_body(&["Hallo", "Welt"], Lang::parse("zh").unwrap(), None);
        assert_eq!(body["text"], serde_json::json!(["Hallo", "Welt"]));
        assert_eq!(body["target_lang"], "ZH");
        assert!(body.get("source_lang").is_none());

        let body = request_body(
            &["Hallo"],
            Lang::parse("de").unwrap(),
            Some(Lang::parse("en").unwrap()),
        );
        assert_eq!(body["source_lang"], "EN-US");
    }

    #[test]
    fn parses_a_translation_response() {
        let body = r#"{"translations":[
            {"detected_source_language":"EN","text":"Hallo"},
            {"detected_source_language":"EN","text":"Welt"}
        ]}"#;
        let batch = parse_response(body, 2).unwrap();
        assert_eq!(batch.items[0].text, "Hallo");
        assert_eq!(batch.items[1].text, "Welt");
        assert_eq!(batch.detected_source().map(Lang::code), Some("en"));
    }

    #[test]
    fn rejects_a_short_response() {
        let body = r#"{"translations":[{"detected_source_language":"EN","text":"Hallo"}]}"#;
        let error = parse_response(body, 3).unwrap_err();
        assert!(
            error.to_string().contains("1 translations for 3"),
            "{error}"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_response("not json", 1),
            Err(TranslateError::Decode(_))
        ));
    }

    #[test]
    fn a_403_explains_the_host_mismatch() {
        let message = map_status(
            403,
            r#"{"message":"Wrong endpoint. Use api-free.deepl.com"}"#,
        )
        .to_string();
        assert!(message.contains("DEEPL_HOST"), "{message}");
        assert!(!message.contains('\n'));
    }

    #[test]
    fn a_456_is_a_quota_error() {
        let error = map_status(456, "");
        assert!(matches!(error, TranslateError::Quota { .. }));
        assert!(error.to_string().contains("quota"));
    }

    #[test]
    fn a_429_is_rate_limited() {
        assert!(matches!(map_status(429, ""), TranslateError::RateLimited));
    }

    #[test]
    fn engine_messages_are_flattened() {
        let message = map_status(400, "{\"message\":\"line one\\nline two\"}").to_string();
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn missing_key_is_not_configured_and_does_not_panic() {
        // Only safe because no other test reads DEEPL_API_KEY concurrently.
        unsafe { std::env::remove_var(KEY_VARIABLE) };
        let error = DeepLTranslator::from_env().unwrap_err();
        assert_eq!(
            error.to_string(),
            "translation engine not configured: set DEEPL_API_KEY"
        );
    }

    #[test]
    fn debug_never_leaks_the_key() {
        let translator = DeepLTranslator::new(DeepLHost::Free, "secret-key-material");
        let debug = format!("{translator:?}");
        assert!(!debug.contains("secret-key-material"), "{debug}");
        assert!(debug.contains("<redacted>"));
    }
}
