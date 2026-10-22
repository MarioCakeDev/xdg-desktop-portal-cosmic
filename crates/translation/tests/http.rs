//! The real HTTP client against a throwaway local server: proves the request
//! line, headers, body and error mapping without touching the DeepL API.

mod support;

use cosmic_screenshot_translation::{
    DeepLHost, DeepLTranslator, Lang, Provider, TranslateError, Translator,
};
use support::{http_response, serve};

#[test]
fn posts_one_batched_request_with_the_auth_header() {
    let body = r#"{"translations":[
        {"detected_source_language":"DE","text":"Hello"},
        {"detected_source_language":"DE","text":"World"}
    ]}"#;
    let (url, requests) = serve(vec![http_response("200 OK", body)]);
    let translator = DeepLTranslator::new(DeepLHost::Custom(url), "test-key");

    let batch = translator
        .translate(&["Hallo", "Welt"], Lang::parse("en").unwrap(), None)
        .expect("translation succeeds");

    assert_eq!(batch.items[0].text, "Hello");
    assert_eq!(batch.detected_source().map(Lang::code), Some("de"));
    assert_eq!(translator.provider(), Provider::DeepL);

    let request = requests.recv().expect("one request");
    assert!(
        request.starts_with("POST /v2/translate HTTP/1.1"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: deepl-auth-key test-key"),
        "{request}"
    );
    assert!(request.contains(r#""text":["Hallo","Welt"]"#), "{request}");
    assert!(request.contains(r#""target_lang":"EN-US""#), "{request}");
}

#[test]
fn fifty_one_blocks_become_two_requests() {
    let translations = |range: std::ops::Range<usize>| {
        let items: Vec<serde_json::Value> = range
            .map(|i| {
                serde_json::json!({
                    "detected_source_language": "EN",
                    "text": format!("t{i}"),
                })
            })
            .collect();
        serde_json::json!({ "translations": items }).to_string()
    };
    let (url, requests) = serve(vec![
        http_response("200 OK", &translations(0..50)),
        http_response("200 OK", &translations(50..51)),
    ]);
    let translator = DeepLTranslator::new(DeepLHost::Custom(url), "test-key");
    let texts: Vec<String> = (0..51).map(|i| format!("line {i}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();

    let batch = translator
        .translate(&refs, Lang::parse("de").unwrap(), None)
        .expect("translation succeeds");
    assert_eq!(batch.items.len(), 51);
    assert_eq!(batch.items[50].text, "t50");

    let first_request = requests.recv().expect("first request");
    let second_request = requests.recv().expect("second request");
    assert_eq!(
        first_request.matches(r#""line "#).count(),
        50,
        "first request must carry 50 items"
    );
    assert!(
        second_request.contains(r#""line 50""#),
        "second request carries the remainder"
    );
}

#[test]
fn a_403_becomes_a_host_hint() {
    let (url, _requests) = serve(vec![http_response(
        "403 Forbidden",
        r#"{"message":"Wrong endpoint. Use api-free.deepl.com"}"#,
    )]);
    let translator = DeepLTranslator::new(DeepLHost::Custom(url), "free-key");
    let error = translator
        .translate(&["Hallo"], Lang::parse("en").unwrap(), None)
        .unwrap_err();
    assert!(matches!(error, TranslateError::Auth { status: 403, .. }));
    assert!(error.to_string().contains("DEEPL_HOST"));
}

#[test]
fn an_unreachable_host_is_a_network_error() {
    // Port 9 on loopback is closed; the connection is refused immediately.
    let translator = DeepLTranslator::new(DeepLHost::Custom("http://127.0.0.1:9".into()), "key");
    let error = translator
        .translate(&["Hallo"], Lang::parse("en").unwrap(), None)
        .unwrap_err();
    assert!(matches!(error, TranslateError::Network(_)), "{error}");
}
