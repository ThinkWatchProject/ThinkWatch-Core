//! Codex's native model catalog, projected from the live gateway directory.
//!
//! Only upstream metadata is cached. Names, aliases, permissions and manual
//! capabilities are resolved again on every read, including conditional reads.
//! A version/credential-scoped upstream catalog preserves native instructions
//! and tools; ordinary API model lists use a conservative gateway profile.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tw_config::{Provider, SpecSource};
use tw_types::msg;

use crate::error::GatewayError;
use crate::state::{AppState, Runtime};

mod reasoning;

const MAX_BYTES: usize = 1024 * 1024;
const MAX_CACHED: usize = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const CACHE_TTL: Duration = Duration::from_secs(300);
type Models = Arc<BTreeMap<String, Value>>;
type Cell = Arc<tokio::sync::Mutex<Option<Cached>>>;

struct Cached {
    expires: Instant,
    models: Models,
}

pub(crate) struct Cache {
    entries: Mutex<HashMap<String, (Instant, Cell)>>,
    fetches: tokio::sync::Semaphore,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            fetches: tokio::sync::Semaphore::new(4),
        }
    }
}

impl Cache {
    fn cell(&self, key: String) -> Cell {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if !entries.contains_key(&key) && entries.len() >= MAX_CACHED {
            let oldest = entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        let (used, cell) = entries
            .entry(key)
            .or_insert_with(|| (Instant::now(), Default::default()));
        *used = Instant::now();
        cell.clone()
    }

    async fn get(
        &self,
        state: &AppState,
        rt: &Runtime,
        provider: &Provider,
        version: &str,
    ) -> Models {
        let empty = || Arc::new(BTreeMap::new());
        let http = rt.clients.get(&provider.name).unwrap_or(&state.http);
        let Ok(headers) = state.headers_for(provider, http).await else {
            return empty();
        };
        let url = upstream_url(provider, version);
        // Resolved credentials, not environment-variable spellings, partition
        // access-dependent catalogs. No raw credential is retained in the key.
        let identity = serde_json::to_vec(&(
            &provider.name,
            &url,
            &headers,
            crate::outbound::proxy_shape(&rt.config, provider),
        ))
        .expect("catalog identity is JSON");
        let cell = self.cell(blake3::hash(&identity).to_hex().to_string());
        let mut cached = cell.lock().await;
        if let Some(entry) = cached.as_ref().filter(|c| c.expires > Instant::now()) {
            return entry.models.clone();
        }
        let Ok(_permit) = self.fetches.acquire().await else {
            return empty();
        };
        let (models, ttl) = fetch(http, &url, &headers)
            .await
            .unwrap_or_else(|| (empty(), Duration::from_secs(30)));
        *cached = (!ttl.is_zero()).then(|| Cached {
            expires: Instant::now() + ttl,
            models: models.clone(),
        });
        models
    }
}

fn upstream_url(provider: &Provider, version: &str) -> String {
    let query = format!("client_version={version}");
    if provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt) {
        return crate::forward::upstream_url(&provider.base_url, "/models", Some(&query));
    }
    // OpenAI documents its native Codex catalog separately from its public
    // models API. Only the actual official origin uses this credential target.
    let official = reqwest::Url::parse(&provider.base_url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api.openai.com")
            && u.port_or_known_default() == Some(443)
            && matches!(u.path().trim_end_matches('/'), "" | "/v1")
    });
    if official {
        return crate::forward::upstream_url(crate::chatgpt::BASE_URL, "/models", Some(&query));
    }
    crate::forward::upstream_url(&provider.base_url, "/v1/models", Some(&query))
}

async fn fetch(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Option<(Models, Duration)> {
    let mut response = crate::forward::apply_headers(http.get(url).timeout(FETCH_TIMEOUT), headers)
        .send()
        .await
        .ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return None;
    }
    let ttl = cache_ttl(response.headers());
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).ok()?;
    let mut models = BTreeMap::new();
    for model in value.get("models")?.as_array()? {
        // A list of IDs is not a native Codex definition. Unknown fields in a
        // valid definition survive unchanged, including version-specific tools.
        if native(model) {
            models
                .entry(model["slug"].as_str()?.to_string())
                .or_insert_with(|| model.clone());
        }
    }
    Some((Arc::new(models), ttl))
}

fn cache_ttl(headers: &HeaderMap) -> Duration {
    let mut ttl = CACHE_TTL;
    for value in headers.get_all(header::CACHE_CONTROL) {
        for directive in value.to_str().unwrap_or_default().split(',').map(str::trim) {
            if directive.eq_ignore_ascii_case("no-store")
                || directive.eq_ignore_ascii_case("no-cache")
            {
                return Duration::ZERO;
            }
            if let Some((key, value)) = directive.split_once('=')
                && key.eq_ignore_ascii_case("max-age")
                && let Ok(seconds) = value.trim().trim_matches('"').parse::<u64>()
            {
                ttl = ttl.min(Duration::from_secs(seconds));
            }
        }
    }
    let age = headers
        .get(header::AGE)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .unwrap_or_default();
    ttl.saturating_sub(Duration::from_secs(age))
}

fn native(model: &Value) -> bool {
    model["slug"].as_str().is_some_and(|s| !s.trim().is_empty())
        && model["display_name"].is_string()
        && model["supported_reasoning_levels"].is_array()
        && model["shell_type"].is_string()
        && model["truncation_policy"].is_object()
        && (model["base_instructions"].is_string() || model["model_messages"].is_object())
}

fn version(query: Option<&str>) -> Result<String, GatewayError> {
    let values: Vec<_> =
        reqwest::Url::parse(&format!("http://catalog/?{}", query.unwrap_or_default()))
            .expect("a query appended to a fixed URL is valid")
            .query_pairs()
            .filter(|(key, _)| key == "client_version")
            .map(|(_, v)| v.into_owned())
            .collect();
    if let [value] = values.as_slice()
        && value.len() <= 32
        && value.split('.').count() == 3
        && value
            .split('.')
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Ok(value.clone());
    }
    Err(GatewayError::request(
        msg!("gw.codex_catalog.client_version" => "The Codex catalog requires one client_version in major.minor.patch form."),
    ))
}

fn authorize(
    state: &AppState,
    peer: std::net::SocketAddr,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<(Arc<Runtime>, Option<Vec<String>>), GatewayError> {
    loop {
        let rt = state.runtime();
        let identity = state.identify(headers, query);
        // identify reads the runtime itself. Retry if a reload separated that
        // authentication from the source/permission snapshot used here.
        if !Arc::ptr_eq(&rt, &state.runtime()) {
            continue;
        }
        if !rt.allow.allows(peer.ip()) {
            return Err(GatewayError::auth(
                msg!("gw.auth.source_not_allowed", peer = peer.ip() => "{peer} is not among the allowed source addresses."),
            ));
        }
        let (client, _) = identity?;
        let allow = crate::models::key_allow(&rt.config, &client).map(<[_]>::to_vec);
        return Ok((rt, allow));
    }
}

fn candidates<'a>(
    rt: &'a Runtime,
    catalog: &'a tw_engine::Catalog,
    name: &'a str,
) -> Vec<(&'a Provider, &'a str)> {
    let available: Vec<_> = if catalog.alias(name).is_some() {
        catalog
            .served(name)
            .iter()
            .map(|(p, model)| (p.as_str(), model.as_str()))
            .collect()
    } else {
        catalog
            .providers_for(name)
            .iter()
            .map(|p| (p.as_str(), name))
            .collect()
    };
    available
        .into_iter()
        .filter_map(|(p, model)| {
            rt.config
                .providers
                .iter()
                .find(|v| v.name == p && !v.disabled)
                .map(|p| (p, model))
        })
        .collect()
}

fn names(
    cfg: &tw_config::Config,
    catalog: &tw_engine::Catalog,
    allow: Option<&[String]>,
) -> Vec<String> {
    crate::discovery::models(
        cfg,
        catalog,
        Some(&crate::client_api::slugs(
            crate::client_api::ClientApi::OpenaiResponses.servable_by(true),
        )),
        allow,
    )
}

pub(crate) async fn list(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let (mut rt, mut allow) = authorize(&state, peer, &headers, query.as_deref())?;
    let version = version(query.as_deref())?;
    let catalog = state.catalog.load_full();
    let listed = names(&rt.config, &catalog, allow.as_deref());
    let mut providers = BTreeMap::new();
    for name in &listed {
        for (provider, _) in candidates(&rt, &catalog, name) {
            if matches!(
                provider.effective_protocol(),
                Some(
                    tw_config::Protocol::OpenaiChat
                        | tw_config::Protocol::OpenaiResponses
                        | tw_config::Protocol::Chatgpt
                )
            ) {
                providers.insert(provider.name.clone(), provider.clone());
            }
        }
    }
    let mut upstreams = HashMap::new();
    {
        let requests: Vec<_> = providers
            .into_values()
            .map(|p| {
                let state = state.clone();
                let rt = rt.clone();
                let version = version.clone();
                async move {
                    (
                        p.name.clone(),
                        state.codex_catalogs.get(&state, &rt, &p, &version).await,
                    )
                }
            })
            .collect();
        let mut requests = stream::iter(requests).buffer_unordered(4);
        let deadline = tokio::time::Instant::now() + FETCH_TIMEOUT;
        while let Ok(Some((provider, models))) =
            tokio::time::timeout_at(deadline, requests.next()).await
        {
            upstreams.insert(provider, models);
        }
    }
    // A permission/credential change during a slow metadata fetch must not
    // return the old account's catalog, even as a 304. The next read can fetch
    // fresh native metadata; this read uses the new runtime's generic profile.
    let (current, current_allow) = authorize(&state, peer, &headers, query.as_deref())?;
    if !Arc::ptr_eq(&rt, &current) {
        upstreams.clear();
    }
    rt = current;
    allow = current_allow;
    let catalog = state.catalog.load();
    let book = state.pricing.load();
    let models: Vec<_> = names(&rt.config, &catalog, allow.as_deref())
        .iter()
        .map(|name| {
            project(
                name,
                &candidates(&rt, &catalog, name),
                &book,
                &upstreams,
                &version,
            )
        })
        .collect();
    let body = serde_json::to_vec(&json!({"models":models})).expect("catalog is JSON");
    if body.len() > MAX_BYTES {
        return Err(GatewayError::too_large(
            msg!("gw.codex_catalog.too_large" => "The Codex catalog exceeds 1 MiB. Reduce the models available to this gateway key."),
        ));
    }
    let etag = format!("\"{}\"", blake3::hash(&body).to_hex());
    let unchanged = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|v| v == "*" || v.strip_prefix("W/").unwrap_or(v) == etag);
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("hash is a header value"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=30, must-revalidate"),
    );
    response.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("Authorization, X-API-Key, X-Goog-API-Key"),
    );
    Ok(response)
}

fn generic(name: &str) -> Value {
    json!({
        "slug":name,"display_name":name,"description":"Available through ThinkWatch",
        "base_instructions":"You are a coding assistant. Follow the user's instructions, inspect relevant files before editing, use the provided tools when needed, and report changes and verification clearly.",
        "supported_reasoning_levels":[],"shell_type":"shell_command","visibility":"list",
        "supported_in_api":true,"priority":0,"support_verbosity":false,
        "truncation_policy":{"mode":"tokens","limit":10000},
        "experimental_supported_tools":[],"upgrade":null,"input_modalities":["text"],
        "supports_search_tool":false,"node_repl_disabled":true,
        "service_tiers":[],"additional_speed_tiers":[],"default_service_tier":null
    })
}

fn pick<T: Copy>(spec: Option<tw_config::Sourced<T>>, native: Option<T>) -> Option<T> {
    match spec {
        Some(s) if s.source == SpecSource::Manual => Some(s.value),
        _ => native.or(spec.map(|s| s.value)),
    }
}

fn project(
    name: &str,
    candidates: &[(&Provider, &str)],
    book: &tw_pricing::PriceBook,
    upstreams: &HashMap<String, Models>,
    version: &str,
) -> Value {
    let definitions: Vec<_> = candidates
        .iter()
        .map(|(p, model)| upstreams.get(&p.name).and_then(|models| models.get(*model)))
        .collect();
    let native = definitions
        .first()
        .copied()
        .flatten()
        .filter(|first| definitions.iter().all(|v| v.is_some_and(|v| v == *first)));
    let mut out = native.cloned().unwrap_or_else(|| generic(name));
    let mut contexts = Vec::new();
    let mut max_contexts = Vec::new();
    let mut reasoning = Vec::new();
    let mut images = Vec::new();
    for ((p, model), definition) in candidates.iter().zip(&definitions) {
        let spec = p.model_spec(book, model);
        contexts.push(pick(
            spec.context_window,
            definition.and_then(|d| d["context_window"].as_u64()),
        ));
        max_contexts.push(pick(
            spec.context_window,
            definition.and_then(|d| {
                d["max_context_window"]
                    .as_u64()
                    .or_else(|| d["context_window"].as_u64())
            }),
        ));
        // Only effort choices use the bundled profiles. Instructions, tools
        // and other native fields still come exclusively from the upstream.
        let profile = definition.or_else(|| reasoning::profile(model, version));
        let enabled = pick(
            spec.reasoning,
            profile.map(|d| {
                d["supported_reasoning_levels"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
            }),
        );
        reasoning.push(if enabled == Some(false) {
            None
        } else {
            profile
        });
        images.push(pick(
            spec.image_input,
            definition
                .and_then(|d| d["input_modalities"].as_array())
                .map(|v| v.iter().any(|v| v == "image")),
        ));
    }
    let context = (!contexts.is_empty() && contexts.iter().all(Option::is_some))
        .then(|| contexts.iter().flatten().copied().min())
        .flatten();
    if let Some(context) = context {
        out["context_window"] = json!(context);
        out["max_context_window"] = json!(
            max_contexts
                .iter()
                .flatten()
                .copied()
                .min()
                .unwrap_or(context)
        );
    } else if let Some(object) = out.as_object_mut() {
        object.remove("context_window");
        object.remove("max_context_window");
    }
    let requires_translation = candidates.iter().any(|(provider, _)| {
        !matches!(
            provider.effective_protocol(),
            Some(tw_config::Protocol::OpenaiResponses | tw_config::Protocol::Chatgpt)
        )
    });
    reasoning::apply(&mut out, &reasoning, requires_translation);
    if !images.is_empty() && images.iter().all(|v| *v == Some(true)) {
        out["input_modalities"] = json!(["text", "image"]);
    } else {
        out["input_modalities"] = json!(["text"]);
        out["supports_image_detail_original"] = json!(false);
    }
    if out["slug"] != name {
        out["display_name"] = json!(name);
    }
    out["slug"] = json!(name);
    out["supported_in_api"] = json!(true);
    out["visibility"] = json!("list");
    out["upgrade"] = Value::Null;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_cache_honors_upstream_controls_and_validates_the_client_version() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=17"),
        );
        assert_eq!(cache_ttl(&headers), Duration::from_secs(17));
        headers.insert(header::AGE, HeaderValue::from_static("10"));
        assert_eq!(cache_ttl(&headers), Duration::from_secs(7));
        headers.append(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        assert_eq!(cache_ttl(&headers), Duration::ZERO);
        assert_eq!(version(Some("client_version=0.166.0")).unwrap(), "0.166.0");
        for query in [
            "",
            "client_version=invalid",
            "client_version=1.2.3&client_version=1.2.4",
        ] {
            assert!(version(Some(query)).is_err());
        }
    }

    #[test]
    fn upstream_urls_select_native_openai_defaults_only_for_the_official_origin() {
        let mut provider = Provider {
            name: "test".into(),
            base_url: "https://api.openai.com/v1/".into(),
            protocol: Some(tw_config::Protocol::OpenaiResponses),
            ..Default::default()
        };
        assert_eq!(
            upstream_url(&provider, "0.166.0"),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.166.0"
        );
        provider.base_url = "https://relay.example/api.openai.com/v1".into();
        assert!(upstream_url(&provider, "0.166.0").starts_with("https://relay.example/"));
        provider.base_url = "http://127.0.0.1:1234/codex".into();
        provider.protocol = Some(tw_config::Protocol::Chatgpt);
        assert_eq!(
            upstream_url(&provider, "0.166.0"),
            "http://127.0.0.1:1234/codex/models?client_version=0.166.0"
        );
    }
}
