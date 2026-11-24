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
fn a_saved_glm_profile_lists_each_context_hinted_model_once() {
    // A catalog saved before the `[1m]` names became aliases (Z.ai refuses them): its
    // hinted default and models are their models, listed once; its own models stay.
    let mut saved = default_provider_catalog();
    let glm = saved
        .providers
        .iter_mut()
        .find(|profile| profile.id == "glm")
        .unwrap();
    glm.default_model = "glm-5.2[1m]".into();
    glm.models = ["glm-5.3-flash[1m]", "glm-5.4[1m]", "glm-4.7"]
        .map(String::from)
        .to_vec();
    glm.model_aliases.clear();
    // Written by a save when GLM-5.2 had only GLM's generic price.
    glm.model_specs.entry("glm-5.2".into()).or_default().pricing = Some(ModelPricing {
        input_usd_per_mtok: 1.0,
        output_usd_per_mtok: 5.0,
        cache_read_usd_per_mtok: 0.1,
        cache_create_usd_per_mtok: 1.25,
    });
    let merged = normalize_provider_catalog(saved).unwrap();
    let glm = find_provider_profile(&merged, "glm").unwrap();
    assert_eq!(glm.default_model, "glm-5.2");
    assert!(
        glm.models.iter().all(|model| !model.contains('[')),
        "{:?}",
        glm.models
    );
    assert!(glm.models.iter().any(|model| model == "glm-4.7"));
    for model in ["glm-5.3-flash", "glm-5.4", "glm-5.2"] {
        assert_eq!(
            glm.models.iter().filter(|listed| *listed == model).count(),
            1,
            "{model}"
        );
    }
    assert_eq!(
        normalize_provider_model_value(&glm, "glm-5.3[1m]"),
        "glm-5.3"
    );
    // Z.ai's published prices, the stale generic one given way.
    for (model, input, output) in [
        ("glm-5.2", 1.4, 4.4),
        ("glm-5.1", 1.4, 4.4),
        ("glm-5", 1.0, 3.2),
        ("glm-4.6", 0.6, 2.2),
    ] {
        let price = resolve_model_spec(&glm, model).pricing.expect(model);
        assert_eq!(
            (price.input_usd_per_mtok, price.output_usd_per_mtok),
            (input, output),
            "{model}"
        );
    }
    assert!(resolve_model_spec(&glm, "glm-5.4").pricing.is_none());
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
    // A host that does not know the device (the browser) sends the key alone, as Kimi
    // documents for third-party coding agents.
    let browser = provider_headers_with_context(
        RequestContract::AnthropicMessages,
        "fixture",
        true,
        false,
        false,
        None,
        &HeaderContext {
            kimi: None,
            ..context()
        },
    )
    .unwrap();
    assert_eq!(browser["x-api-key"], "fixture");
    assert!(
        browser
            .keys()
            .all(|name| !name.as_str().starts_with("x-msh-")),
        "{browser:?}"
    );
    assert!(!browser.contains_key("user-agent"));
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
    // Kimi's device facts are not among them: without them the key goes alone (above).
    ctx.kimi = None;
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

#[test]
fn openai_api_models_are_asked_no_more_than_they_answer_and_retired_ones_are_gone() {
    let profiles = built_in_provider_profiles();
    let openai = profiles
        .iter()
        .find(|profile| profile.id == "openai")
        .unwrap();
    // OpenAI answers model_not_found for these (deprecated).
    for retired in ["o3-mini", "o4-mini"] {
        assert!(
            !openai.models.iter().any(|model| model == retired),
            "{retired}"
        );
    }
    // GPT-4o refuses an output ask above 16,384 tokens.
    for model in ["gpt-4o", "gpt-4o-mini"] {
        assert_eq!(
            openai.model_specs[model].max_output_tokens,
            Some(16_384),
            "{model}"
        );
    }
}
