//! The model catalog a picker or the `settings` tool offers: every provider in Dext's
//! catalog with its sign-in state and models, each with the reasoning efforts this
//! build can send it, image input, context window and catalog price. Built from the
//! catalog and the stored logins only: no network, no credential commands beyond the
//! ones `logins()` already runs.
use super::{Effort, Result, Route, install_host, provider, reasoning, route_for};
use dext_provider_catalog::provider::{ProviderProfile, RequestContract, StoredCredential};
use serde_json::{Value, json};

/// The efforts `settings` accepts for a route, `default` first.
fn efforts(route: &Route) -> Vec<&'static str> {
    Effort::ALL
        .into_iter()
        .filter(|effort| reasoning(route, *effort).is_ok())
        .map(Effort::as_str)
        .collect()
}

/// The valid `effort` values for a provider and model (empty when the route is unknown).
pub fn valid_efforts(provider_id: &str, model: &str) -> Vec<&'static str> {
    super::route(provider_id, Some(model))
        .map(|route| efforts(&route))
        .unwrap_or_default()
}

/// The models a provider offers, its default first (`curated_provider_models`).
pub(super) fn models_of(profile: &ProviderProfile) -> Vec<String> {
    let mut models = provider::curated_provider_models(profile);
    let default = provider::resolve_provider_model(profile);
    if !default.is_empty() && !models.iter().any(|m| m.eq_ignore_ascii_case(&default)) {
        models.insert(0, default);
    }
    models
}

fn model_json(profile: &ProviderProfile, model: &str) -> Value {
    let spec = provider::resolve_model_spec(profile, model);
    let efforts = route_for(profile.clone(), Some(model))
        .map(|route| efforts(&route))
        .unwrap_or_default();
    let price = spec.pricing.as_ref().map(|pricing| {
        let (pricing, _) = super::cost::priced_by_env(pricing);
        json!({
            "input": pricing.input_usd_per_mtok,
            "output": pricing.output_usd_per_mtok,
            "cached": pricing.cache_read_usd_per_mtok,
        })
    });
    json!({
        "id": model,
        "label": model,
        "efforts": efforts,
        "default_effort": Effort::Default.as_str(),
        "image": spec.image_input,
        "context": spec.context_window,
        "price": price,
    })
}

/// `{"providers": [{id, name, signed_in, subscription, models: [{id, label, efforts,
/// default_effort, image, context, price: {input, output, cached} per million tokens |
/// null}], default_model}]}`. `subscription` is whether requests go through a
/// subscription login (no per-call price): the ChatGPT route, or an Anthropic route
/// signed in with OAuth.
pub fn model_catalog() -> Result<Value> {
    install_host();
    let catalog = provider::load_provider_catalog().map_err(|error| error.to_string())?;
    let store = provider::load_auth_store().map_err(|error| error.to_string())?;
    let providers = catalog
        .providers
        .iter()
        .map(|profile| {
            let models = models_of(profile);
            let default_model = provider::resolve_provider_model(profile);
            let contract = route_for(profile.clone(), Some(&default_model))
                .map(|route| route.contract)
                .ok();
            let canonical = provider::canonical_provider_id(&profile.id);
            let oauth = matches!(
                store
                    .providers
                    .get(&profile.id)
                    .or_else(|| store.providers.get(&canonical)),
                Some(StoredCredential::OAuth { .. })
            );
            let subscription = contract == Some(RequestContract::ChatGptResponses)
                || (contract == Some(RequestContract::AnthropicMessages) && oauth);
            json!({
                "id": profile.id,
                "name": if profile.display_name.is_empty() { &profile.id } else { &profile.display_name },
                "signed_in": provider::provider_has_available_credentials(profile, &store),
                "subscription": subscription,
                "models": models.iter().map(|model| model_json(profile, model)).collect::<Vec<_>>(),
                "default_model": default_model,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "providers": providers }))
}

/// The catalog in a few lines for a model: per provider, sign-in state and models
/// (the default marked `*`), and the efforts of `current` (provider, model).
pub(super) fn compact(current: (&str, &str)) -> Result<String> {
    let catalog = model_catalog()?;
    let mut text = String::new();
    for provider in catalog["providers"].as_array().into_iter().flatten() {
        let default = provider["default_model"].as_str().unwrap_or_default();
        let models: Vec<String> = provider["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| model["id"].as_str())
            .map(|id| {
                if id == default {
                    format!("{id}*")
                } else {
                    id.to_string()
                }
            })
            .collect();
        text.push_str(&format!(
            "\n- {} ({}{}): {}",
            provider["id"].as_str().unwrap_or_default(),
            if provider["signed_in"] == true {
                "signed in"
            } else {
                "not signed in"
            },
            if provider["subscription"] == true {
                ", subscription"
            } else {
                ""
            },
            models.join(", ")
        ));
    }
    let efforts = valid_efforts(current.0, current.1);
    text.push_str(&format!(
        "\nEfforts for {}/{}: {}.",
        current.0,
        current.1,
        efforts.join(", ")
    ));
    Ok(text)
}

/// A model's context window in tokens, resolved through the provider catalog once per
/// (provider, model) for the life of the process: the catalog is read rarely, and a
/// snapshot is read often.
pub fn context_window(provider_id: &str, model: &str) -> Option<u64> {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    /// (provider, model) → window, None where the catalog does not say.
    type Windows = BTreeMap<(String, String), Option<u64>>;
    static CACHE: OnceLock<Mutex<Windows>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let key = (provider_id.to_string(), model.to_string());
    if let Some(window) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return *window;
    }
    install_host();
    // A spec without a window of its own falls back to the provider's; None means
    // the catalog does not say, and the UI hides the gauge rather than guess.
    let catalog = provider::load_provider_catalog().ok()?;
    let window = provider::find_provider_profile(&catalog, provider_id)
        .and_then(|profile| provider::resolve_model_spec(&profile, model).context_window);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, window);
    window
}
