use dext_provider::catalog::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;

fn context() -> HeaderContext<'static> {
    HeaderContext {
        originator: "embedded-test",
        user_agent: "embedded/1",
        request_id: Some("11111111-2222-4333-8444-555555555555"),
        kimi: Some(KimiHeaderContext {
            device_name: "fixture",
            device_model: "test wasm32",
            os_version: "test",
            device_id: "11111111-2222-4333-8444-555555555555",
        }),
    }
}

#[test]
fn pure_catalog_normalization_and_wire_encoding_need_no_native_host() {
    let catalog = default_provider_catalog();
    let roundtrip = normalize_provider_catalog(
        serde_json::from_value(serde_json::to_value(&catalog).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(roundtrip.providers.len(), catalog.providers.len());
    let deepseek = find_provider_profile(&catalog, "deepseek").unwrap();
    assert_eq!(
        normalize_provider_model_value(&deepseek, "deepseek-chat"),
        "deepseek-flash"
    );
    let profile = find_provider_profile(&catalog, "openai").unwrap();
    assert_eq!(
        effective_request_contract(&profile, "https://api.openai.com/v1", "gpt-6.1-sol"),
        RequestContract::OpenAiResponses
    );
    assert_eq!(
        effective_request_contract(&profile, "https://elsewhere.invalid/v1", "gpt-6.1-sol"),
        RequestContract::OpenAiChatCompletions
    );
    let body = build_chatgpt_request(
        "gpt61sol",
        Some("xhigh"),
        "system",
        "session",
        vec![],
        vec![],
    );
    assert!(body.get("max_output_tokens").is_none());
    let encoded = ProviderRequestBody::encode(
        RequestContract::ChatGptResponses,
        body.to_string().into_bytes(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!encoded.gzip);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&encoded.bytes).unwrap(),
        body
    );
}

#[test]
fn catalog_versions_and_custom_provider_order_are_deterministic_and_keep_file_order() {
    let catalog = default_provider_catalog();
    for version in [0, default_provider_catalog_version() + 1] {
        let mut invalid = catalog.clone();
        invalid.version = version;
        assert!(normalize_provider_catalog(invalid).is_err());
    }
    let profile = find_provider_profile(&catalog, "openai").unwrap();
    let mut custom_catalog = catalog.clone();
    for id in ["z-custom", "a-custom"] {
        let mut custom = profile.clone();
        custom.id = id.into();
        custom.builtin = None;
        custom_catalog.providers.push(custom);
    }
    let expected =
        serde_json::to_value(normalize_provider_catalog(custom_catalog.clone()).unwrap()).unwrap();
    for _ in 0..20 {
        let actual = normalize_provider_catalog(custom_catalog.clone()).unwrap();
        let custom = actual
            .providers
            .iter()
            .filter(|p| p.id.ends_with("-custom"))
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(custom, ["z-custom", "a-custom"]);
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
}

#[test]
fn headers_use_supplied_facts_and_keep_credentials_sensitive() {
    let ctx = context();
    let openai = provider_headers_with_context(
        RequestContract::OpenAiResponses,
        "fixture",
        false,
        false,
        false,
        Some("s"),
        &ctx,
    )
    .unwrap();
    assert_eq!(openai["authorization"], "Bearer fixture");
    assert!(openai["authorization"].is_sensitive());
    let key = provider_headers_with_context(
        RequestContract::AnthropicMessages,
        "fixture",
        false,
        false,
        true,
        None,
        &ctx,
    )
    .unwrap();
    assert_eq!(key["x-api-key"], "fixture");
    assert!(key["x-api-key"].is_sensitive());
    assert_eq!(key["anthropic-version"], ANTHROPIC_API_VERSION);
    let subscription = provider_headers_with_context(
        RequestContract::AnthropicMessages,
        "fixture",
        false,
        true,
        false,
        Some("s"),
        &ctx,
    )
    .unwrap();
    assert_eq!(subscription["x-client-request-id"], ctx.request_id.unwrap());
    assert!(subscription["authorization"].is_sensitive());
    assert!(!subscription.contains_key("x-api-key"));
    let kimi = provider_headers_with_context(
        RequestContract::AnthropicMessages,
        "fixture",
        true,
        false,
        false,
        None,
        &ctx,
    )
    .unwrap();
    assert_eq!(kimi["x-msh-device-name"], "fixture");
    assert_eq!(kimi["x-msh-device-model"], "test wasm32");
    assert_eq!(kimi["x-msh-os-version"], "test");
    let payload = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}).to_string(),
    );
    let token = format!("e30.{payload}.fixture");
    let chatgpt = provider_headers_with_context(
        RequestContract::ChatGptResponses,
        &token,
        false,
        false,
        false,
        Some("s"),
        &ctx,
    )
    .unwrap();
    assert_eq!(chatgpt["originator"], "embedded-test");
    assert_eq!(chatgpt["user-agent"], "embedded/1");
    assert_eq!(chatgpt["chatgpt-account-id"], "fixture-account");
    assert_eq!(chatgpt["session_id"], "s");
    assert!(chatgpt["authorization"].is_sensitive());
}

#[test]
fn missing_header_facts_and_header_injection_fail_closed() {
    let mut ctx = context();
    ctx.request_id = None;
    assert!(
        provider_headers_with_context(
            RequestContract::AnthropicMessages,
            "fixture",
            false,
            true,
            false,
            None,
            &ctx
        )
        .is_err()
    );
    ctx.kimi = None;
    assert!(
        provider_headers_with_context(
            RequestContract::AnthropicMessages,
            "fixture",
            true,
            false,
            false,
            None,
            &ctx
        )
        .is_err()
    );
    assert!(
        provider_headers_with_context(
            RequestContract::OpenAiResponses,
            "bad\r\nheader",
            false,
            false,
            false,
            None,
            &ctx
        )
        .is_err()
    );
}

#[test]
fn portable_sources_have_no_ambient_native_dependencies() {
    for text in [
        include_str!("../src/catalog.rs"),
        include_str!("../src/wire.rs"),
    ] {
        for forbidden in [
            "std::fs",
            "std::env",
            "std::process",
            "reqwest",
            "tokio",
            "crate::host",
        ] {
            assert!(
                !text.contains(forbidden),
                "portable source contains {forbidden}"
            );
        }
    }
}
