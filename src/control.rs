//! Model control: manually load/unload models on connected backends.
//!
//! Backend contracts (verified against upstream sources/docs):
//!
//! * **Ollama** has no dedicated load/unload endpoint. Loading is
//!   `POST /api/generate` with an empty prompt (the model is scheduled into
//!   memory and an empty response is returned immediately). Unloading is the
//!   same endpoint with `keep_alive: 0` (the response carries
//!   `done_reason: "unload"`). Models only stay resident for the request's
//!   `keep_alive` (server default 5m), so explicit loads send a long
//!   `keep_alive` (configurable via `--load-keep-alive`).
//! * **LM Studio** (0.3.6+, beta REST API): `POST /api/v1/models/load` with
//!   `{"model": "<key>"}` (synchronous, blocks until loaded), and
//!   `POST /api/v1/models/unload` with `{"instance_id": "<id>"}` where the
//!   instance id comes from `GET /api/v1/models` -> `loaded_instances[].id`.

use crate::dispatcher::{
    AppState, BackendApiType, BackendStatus, LmModelInfo, smart_model_match_one,
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// How many finished control ops to remember for TUI feedback.
const MAX_HISTORY: usize = 20;

/// How long a finished op stays visible in the TUI.
pub const RESULT_VISIBLE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Load,
    Unload,
}

impl ControlAction {
    pub fn label(self) -> &'static str {
        match self {
            ControlAction::Load => "load",
            ControlAction::Unload => "unload",
        }
    }

    /// Present-participle form for UI text ("loading llama3…").
    pub fn verb(self) -> &'static str {
        match self {
            ControlAction::Load => "loading",
            ControlAction::Unload => "unloading",
        }
    }
}

/// Extra options for a model-control "load" (from a profile or the admin
/// API). All fields optional; `None` falls back to existing defaults.
#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Max context window: Ollama `options.num_ctx`, LM Studio
    /// `context_length`.
    pub num_ctx: Option<u64>,
    /// `keep_alive` override (seconds) for Ollama loads. `-1` keeps the
    /// model loaded indefinitely (Ollama semantics).
    pub keep_alive: Option<i64>,
    /// Free-form identifier/label shown with the op.
    pub identifier: Option<String>,
}

/// A model control operation currently running on a backend.
/// At most one operation per backend at a time.
#[derive(Clone, Debug)]
pub struct ControlOp {
    pub backend_idx: usize,
    pub action: ControlAction,
    /// Model name as typed by the user.
    pub requested: String,
    /// Resolved backend model name / key.
    pub canonical: String,
    /// Optional identifier/label attached to the op (e.g. from a profile).
    pub identifier: Option<String>,
    pub started: Instant,
}

/// Terminal record of a finished control op (kept for TUI feedback).
#[derive(Clone, Debug)]
pub struct ControlResult {
    pub backend_idx: usize,
    pub action: ControlAction,
    pub model: String,
    pub identifier: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
    pub finished_at: Instant,
}

/// Outcome of probing a single backend. Shared by the periodic health loop
/// and the immediate re-probe that runs after a control op finishes.
pub struct BackendProbe {
    pub is_online: bool,
    pub api_type: BackendApiType,
    pub available_models: HashSet<String>,
    pub loaded_models: HashSet<String>,
    /// Actual context window per resident model (see
    /// `BackendStatus::loaded_ctx`). Empty when the backend doesn't report it.
    pub loaded_ctx: HashMap<String, u64>,
    /// True when the backend speaks the LM Studio native REST API.
    pub lmstudio: bool,
    /// LM Studio native model list (keys, display names, loaded instance ids).
    pub native_models: Vec<LmModelInfo>,
    /// Endpoints probed this round that the backend rejected (HTTP error, or
    /// 2xx with an error body instead of the expected payload). The caller
    /// should remember these and skip them on subsequent probes.
    pub bad_endpoints: HashSet<String>,
    /// Endpoints probed this round that answered successfully — any previous
    /// "bad" memory for these is stale and can be dropped.
    pub good_endpoints: HashSet<String>,
}

/// Probe one backend: online status, API type, available models, loaded
/// models, and (for LM Studio) the native model list.
pub async fn probe_backend(
    client: &reqwest::Client,
    url: &str,
    skip: &HashSet<String>,
) -> BackendProbe {
    let mut is_online = false;
    let mut detected_type = BackendApiType::Unknown;
    let mut models = HashSet::new();
    let mut loaded = HashSet::new();
    let mut loaded_ctx: HashMap<String, u64> = HashMap::new();
    let mut lmstudio = false;
    let mut native_models: Vec<LmModelInfo> = Vec::new();
    let mut bad_endpoints: HashSet<String> = HashSet::new();
    let mut good_endpoints: HashSet<String> = HashSet::new();

    // Probe Ollama API: /api/tags → expects {"models": [...]}
    if !skip.contains("/api/tags") {
        let check_url = format!("{}/api/tags", url);
        match client.get(&check_url).send().await {
            Ok(res) if res.status().is_success() => {
                is_online = true;
                let body = res.text().await.unwrap_or_default();
                match serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|j| j.get("models").and_then(|m| m.as_array()).cloned())
                {
                    Some(models_json) => {
                        detected_type = detected_type.merge(BackendApiType::Ollama);
                        good_endpoints.insert("/api/tags".to_string());
                        debug!("Backend {} confirmed Ollama API via /api/tags", url);
                        for m in models_json {
                            if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                                models.insert(name.to_string());
                            }
                        }
                    }
                    // 2xx but no models array: e.g. LM Studio answers unknown
                    // endpoints with a 200 + {"error": ...} body. Remember the
                    // endpoint instead of warning on every cycle.
                    None => {
                        bad_endpoints.insert("/api/tags".to_string());
                        debug!(
                            "Backend {} /api/tags has no 'models' array (not an Ollama endpoint). Body: {}",
                            url,
                            body.chars().take(200).collect::<String>()
                        );
                    }
                }
            }
            Ok(res) => {
                bad_endpoints.insert("/api/tags".to_string());
                debug!(
                    "Backend {} /api/tags returned status: {}",
                    url,
                    res.status()
                );
            }
            Err(e) => {
                // Connection-level failure: backend may be down; don't remember.
                debug!("Backend {} /api/tags error: {}", url, e);
            }
        }

        // Also check for loaded models via /api/ps if it was an Ollama-like response
        if is_online && !skip.contains("/api/ps") {
            let ps_url = format!("{}/api/ps", url);
            match client.get(&ps_url).send().await {
                Ok(res) if res.status().is_success() => {
                    let body = res.text().await.unwrap_or_default();
                    match serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|j| j.get("models").and_then(|m| m.as_array()).cloned())
                    {
                        Some(models_json) => {
                            good_endpoints.insert("/api/ps".to_string());
                            for m in models_json {
                                if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                                    loaded.insert(name.to_string());
                                }
                                // Newer Ollama versions report the runner's
                                // actual context window here.
                                if let (Some(name), Some(ctx)) = (
                                    m.get("name").and_then(|n| n.as_str()),
                                    m.get("context_length").and_then(|c| c.as_u64()),
                                ) {
                                    loaded_ctx.insert(name.to_string(), ctx);
                                }
                            }
                        }
                        None => {
                            bad_endpoints.insert("/api/ps".to_string());
                            debug!("Backend {} /api/ps has no 'models' array", url);
                        }
                    }
                }
                Ok(res) => {
                    bad_endpoints.insert("/api/ps".to_string());
                    debug!(
                        "Backend {} /api/ps returned status: {}",
                        url,
                        res.status()
                    );
                }
                Err(e) => {
                    debug!("Backend {} /api/ps error: {}", url, e);
                }
            }
        }
    }

// Probe OpenAI API: /v1/models → expects {"data": [...]}
    if !skip.contains("/v1/models") {
        let check_url = format!("{}/v1/models", url);
        match client.get(&check_url).send().await {
            Ok(res) if res.status().is_success() => {
                is_online = true;
                let body = res.text().await.unwrap_or_default();
                match serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|j| j.get("data").and_then(|d| d.as_array()).cloned())
                {
                    Some(data_json) => {
                        detected_type = detected_type.merge(BackendApiType::OpenAi);
                        good_endpoints.insert("/v1/models".to_string());
                        debug!("Backend {} confirmed OpenAI API via /v1/models", url);
                        for m in data_json {
                            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                                models.insert(id.to_string());
                            }
                        }
                    }
                    None => {
                        bad_endpoints.insert("/v1/models".to_string());
                        debug!(
                            "Backend {} /v1/models has no 'data' array (not an OpenAI endpoint). Body: {}",
                            url,
                            body.chars().take(200).collect::<String>()
                        );
                    }
                }
            }
            Ok(res) => {
                bad_endpoints.insert("/v1/models".to_string());
                debug!(
                    "Backend {} /v1/models returned status: {}",
                    url,
                    res.status()
                );
            }
            Err(e) => {
                debug!("Backend {} /v1/models error: {}", url, e);
            }
        }
    }

    // Probe LM Studio native API for loaded models + model metadata
    // (loaded_instances non-empty). LM-Studio-specific endpoint; Ollama and
    // generic OpenAI servers 404 it. The outcome is remembered so we stop
    // re-probing backends that don't have the endpoint.
        if !skip.contains("/api/v1/models") {
            match probe_lmstudio_native(client, url).await {
                NativeProbeOutcome::Found(ls_loaded, ls_native, ls_ctx) => {
                    lmstudio = true;
                    native_models = ls_native;
                    loaded.extend(ls_loaded);
                    for (k, v) in ls_ctx {
                        loaded_ctx.insert(k, v);
                    }
                    good_endpoints.insert("/api/v1/models".to_string());
                }
            NativeProbeOutcome::Rejected => {
                bad_endpoints.insert("/api/v1/models".to_string());
            }
            NativeProbeOutcome::Unknown => {}
        }
    }

    // Fallback: just check root if both specific probes failed
    if !is_online && !skip.contains("/") {
        let check_url = format!("{}/", url);
        match client.get(&check_url).send().await {
            Ok(res) if res.status().is_success() => {
                is_online = true;
                good_endpoints.insert("/".to_string());
            }
            Ok(res) => {
                bad_endpoints.insert("/".to_string());
                debug!("Backend {} / returned status: {}", url, res.status());
            }
            Err(e) => {
                debug!("Backend {} / error: {}", url, e);
            }
        }
    }

    BackendProbe {
        is_online,
        api_type: detected_type,
        available_models: models,
        loaded_models: loaded,
        loaded_ctx,
        lmstudio,
        native_models,
        bad_endpoints,
        good_endpoints,
    }
}

/// Merge a fresh probe result into backend state, including endpoint memory:
/// endpoints that answered successfully clear their "bad" flag; rejected ones
/// are remembered so future probes skip them. Shared by the periodic health
/// loop and the one-shot re-probes after control ops / config apply.
pub fn apply_probe(b: &mut BackendStatus, probe: BackendProbe) {
    b.is_online = probe.is_online;
    b.api_type = probe.api_type;
    b.available_models = probe.available_models;
    b.loaded_models = probe.loaded_models;
    b.loaded_ctx = probe.loaded_ctx;
    b.lmstudio = probe.lmstudio;
    b.native_models = probe.native_models;
    for e in &probe.good_endpoints {
        b.known_bad_endpoints.remove(e);
    }
    b.known_bad_endpoints.extend(probe.bad_endpoints.iter().cloned());
}

/// Outcome of probing the LM Studio native model list. Distinguishes a real
/// "endpoint exists and answered" from "backend rejected it" (remember as bad)
/// from "couldn't tell" (connection error / unparseable — remember nothing).
enum NativeProbeOutcome {
    /// Endpoint exists; `(loaded_names, native_model_list, loaded_ctx)`.
    Found(
        HashSet<String>,
        Vec<LmModelInfo>,
        HashMap<String, u64>,
    ),
    /// Backend answered with an HTTP error — the endpoint doesn't exist here.
    Rejected,
    /// Connection-level failure or unparseable body — say nothing.
    Unknown,
}

/// Probe LM Studio's native `/api/v1/models` endpoint.
async fn probe_lmstudio_native(
    client: &reqwest::Client,
    url: &str,
) -> NativeProbeOutcome {
    let res = match client.get(format!("{}/api/v1/models", url)).send().await {
        Ok(r) => r,
        Err(_) => return NativeProbeOutcome::Unknown,
    };
    if !res.status().is_success() {
        return NativeProbeOutcome::Rejected;
    }
    let body = match res.text().await.ok() {
        Some(b) => b,
        None => return NativeProbeOutcome::Unknown,
    };
    let json = match serde_json::from_str::<serde_json::Value>(&body).ok() {
        Some(j) => j,
        None => return NativeProbeOutcome::Unknown,
    };
    let models_json = match json.get("models").and_then(|m| m.as_array()) {
        Some(a) => a.clone(),
        None => return NativeProbeOutcome::Unknown,
    };

    let mut loaded = HashSet::new();
    let mut loaded_ctx: HashMap<String, u64> = HashMap::new();
    let mut native = Vec::new();
    for m in models_json {
        let key = m
            .get("key")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string();
        let display_name = m
            .get("display_name")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string());
        let instances = m
            .get("loaded_instances")
            .and_then(|li| li.as_array())
            .cloned()
            .unwrap_or_default();
        let instance_ids: Vec<String> = instances
            .iter()
            .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();

        if !instance_ids.is_empty() {
            // Insert both "key" and instance "id" to maximize match coverage
            // with available_models.
            if !key.is_empty() {
                loaded.insert(key.clone());
            }
            for id in &instance_ids {
                loaded.insert(id.to_string());
            }
            // First loaded instance's actual context window (all instances of
            // a key share the load config in practice).
            let ctx = instances
                .iter()
                .find_map(|i| i.get("config").and_then(|c| c.get("context_length")).and_then(|v| v.as_u64()));
            if let Some(ctx) = ctx {
                let ctx_key = if key.is_empty() {
                    instance_ids[0].clone()
                } else {
                    key.clone()
                };
                loaded_ctx.insert(ctx_key, ctx);
            }
        }
        native.push(LmModelInfo {
            key: key.clone(),
            display_name,
            loaded_instance_ids: instance_ids,
        });
    }
    NativeProbeOutcome::Found(loaded, native, loaded_ctx)
}

pub fn resolve_model_name(b: &BackendStatus, requested: &str) -> Option<String> {
    let req = requested.trim();
    if req.is_empty() {
        return None;
    }

    // 1. Exact match
    if b.available_models.contains(req) {
        return Some(req.to_string());
    }

    // 2. Smart match (handles :latest tags and case), deterministic order
    let mut available: Vec<&String> = b.available_models.iter().collect();
    available.sort();
    if let Some(m) = available.iter().find(|m| smart_model_match_one(req, m)) {
        return Some((*m).clone());
    }

    // 3. Case-insensitive substring (user typed part of the backend name);
    //    only accept when exactly one model matches to avoid guessing.
    let req_low = req.to_lowercase();
    let subs: Vec<&String> = available
        .iter()
        .copied()
        .filter(|m| m.to_lowercase().contains(&req_low))
        .collect();
    if subs.len() == 1 {
        return Some(subs[0].clone());
    }

    // 4. LM Studio native list: key or display name (exact, then unique substring)
    if b.lmstudio {
        let mut native: Vec<&LmModelInfo> = b.native_models.iter().collect();
        native.sort_by(|a, c| a.key.cmp(&c.key));
        if let Some(m) = native.iter().find(|m| {
            m.key.eq_ignore_ascii_case(req)
                || m.display_name
                    .as_deref()
                    .map(|d| d.eq_ignore_ascii_case(req))
                    .unwrap_or(false)
        }) {
            return Some(m.key.clone());
        }
        let subs: Vec<&LmModelInfo> = native
            .iter()
            .copied()
            .filter(|m| {
                m.key.to_lowercase().contains(&req_low)
                    || m.display_name
                        .as_deref()
                        .map(|d| d.to_lowercase().contains(&req_low))
                        .unwrap_or(false)
            })
            .collect();
        if subs.len() == 1 {
            return Some(subs[0].key.clone());
        }
    }

    None
}

/// True when `name` resolves to a model that is already resident on backend
/// `b`, per its latest probe (`loaded_models`: Ollama `/api/ps`, LM Studio
/// loaded instances). Used when applying the model config so models still
/// loaded from an earlier ollamaMQ run (long keep_alive) are not loaded
/// twice. Unresolvable names return false — `start_model_control` reports
/// those as errors instead.
fn is_already_loaded(b: &BackendStatus, name: &str) -> bool {
    match resolve_model_name(b, name) {
        Some(canonical) => b.loaded_models.iter().any(|m| smart_model_match_one(&canonical, m)),
        None => false,
    }
}

/// The actual context window of the resident instance of `name` on backend
/// `b`, when the backend reports one (Ollama `/api/ps` `context_length`;
/// LM Studio loaded-instance `config.context_length`). `None` when the model
/// isn't resident or the backend doesn't expose its context size.
fn resident_ctx(b: &BackendStatus, name: &str) -> Option<u64> {
    let canonical = resolve_model_name(b, name)?;
    b.loaded_models
        .iter()
        .find(|m| smart_model_match_one(&canonical, m))
        .and_then(|m| b.loaded_ctx.get(m))
        .or_else(|| b.loaded_ctx.get(&canonical))
        .copied()
}

/// Does this backend expose a model load/unload API?
fn supports_control(b: &BackendStatus) -> bool {
    b.lmstudio || matches!(b.api_type, BackendApiType::Ollama | BackendApiType::Both)
}

/// Resolve which backend indices a `appconf.yaml` entry targets: the listed
/// backend URLs (exact or substring match against configured backends), or
/// every backend when the list is empty ("any suitable").
fn config_targets(state: &AppState, cfg: &crate::config::ModelConfig) -> Vec<usize> {
    let backends = state.backends.lock().unwrap();
    if cfg.backends.is_empty() {
        (0..backends.len()).collect()
    } else {
        cfg.backends
            .iter()
            .filter_map(|u| {
                let u_low = u.trim_end_matches('/').to_lowercase();
                backends
                    .iter()
                    .position(|b| b.url.to_lowercase().contains(&u_low))
            })
            .collect()
    }
}

/// Apply the current `appconf.yaml` contents: start a load for every model
/// entry on its target backends, except models that are already resident
/// there — each endpoint is checked live first, so models still loaded from
/// an earlier ollamaMQ run (long keep_alive) are skipped instead of being
/// loaded twice. A resident model whose context window differs from its
/// configured `max_ctx` is unloaded and reloaded with the configured value.
/// Loads for the same backend run sequentially (the backend
/// rejects parallel control ops); different backends proceed in parallel.
/// Returns the number of entries applied.
pub fn apply_model_config(state: &Arc<AppState>) -> usize {
    let configs = state.model_config.lock().unwrap().clone();
    if configs.is_empty() {
        return 0;
    }

    // Group model entries by backend so each backend loads one model at a time.
    let mut by_backend: std::collections::HashMap<usize, Vec<crate::config::ModelConfig>> =
        std::collections::HashMap::new();
    for cfg in configs {
        for idx in config_targets(state, &cfg) {
            by_backend.entry(idx).or_default().push(cfg.clone());
        }
    }

    let mut started = 0;
    for (backend_idx, cfgs) in by_backend {
        let st = state.clone();
        started += cfgs.len();
        tokio::spawn(async move {
            for cfg in cfgs {
                // Wait for any earlier control op on this backend to finish.
                while st.control_ops.lock().unwrap().contains_key(&backend_idx) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }

                // Check the endpoint live (the health loop only ticks every 10 s,
                // and a just-finished op's re-probe may still be in flight): if
                // the model is already resident — e.g. loaded by a previous
                // ollamaMQ run with long keep_alive — skip it instead of loading
                // it twice.
                let url = st
                    .backends
                    .lock()
                    .unwrap()
                    .get(backend_idx)
                    .map(|b| b.url.clone())
                    .unwrap_or_default();
                if !url.is_empty() {
                    let probe = probe_backend(&st.client, &url, &HashSet::new()).await;
                    let mut backends = st.backends.lock().unwrap();
                    if let Some(b) = backends.get_mut(backend_idx) {
                        apply_probe(b, probe);
                    }
                }

                let (resident, lmstudio, native_models, control_timeout) = {
                    let backends = st.backends.lock().unwrap();
                    let b = backends.get(backend_idx);
                    let resident = b.is_some_and(|b| b.is_online && is_already_loaded(b, &cfg.name));
                    (
                        resident,
                        b.map(|b| b.lmstudio).unwrap_or(false),
                        b.map(|b| b.native_models.clone()).unwrap_or_default(),
                        Duration::from_secs(st.timeout.max(600)),
                    )
                };

                // A resident model is only "done" when it also runs with the
                // configured max_ctx. Models loaded by an earlier ollamaMQ
                // run, the backend's own UI, a JIT auto-load or a TUI load
                // (which sends no context) may have the wrong context window;
                // in that case unload and reload with the configured value
                // instead of skipping.
                if resident && cfg.max_ctx.is_some() {
                    let want = cfg.max_ctx.unwrap();
                    let actual = st
                        .backends
                        .lock()
                        .unwrap()
                        .get(backend_idx)
                        .and_then(|b| resident_ctx(b, &cfg.name));
                    match actual {
                        Some(a) if a == want => {}
                        Some(a) => {
                            warn!(
                                "load '{}': resident on {} with context {} != configured {}; reloading",
                                cfg.name, url, a, want
                            );
                            st.log_event(crate::dispatcher::LogEvent {
                                at: std::time::SystemTime::now(),
                                dir: "CTL",
                                user: "-".into(),
                                model: Some(cfg.name.clone()),
                                backend: None,
                                info: format!(
                                    "reloading '{}' on {}: context {} != configured {}",
                                    cfg.name, url, a, want
                                ),
                            });
                            let canonical = {
                                let backends = st.backends.lock().unwrap();
                                backends
                                    .get(backend_idx)
                                    .and_then(|b| resolve_model_name(b, &cfg.name))
                            };
                            if let Some(canonical) = canonical {
                                if let Err(e) = execute_unload(
                                    &st.client,
                                    &url,
                                    lmstudio,
                                    &canonical,
                                    control_timeout,
                                    &native_models,
                                )
                                .await
                                {
                                    warn!("unload of '{}' failed: {}", canonical, e);
                                }
                                // Give the backend a moment to release the model.
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                let probe =
                                    probe_backend(&st.client, &url, &HashSet::new()).await;
                                let mut backends = st.backends.lock().unwrap();
                                if let Some(b) = backends.get_mut(backend_idx) {
                                    apply_probe(b, probe);
                                }
                            }
                        }
                        None => {
                            // Backend doesn't report the resident context;
                            // keep the old skip behavior rather than flapping.
                        }
                    }
                }

                let skip = st
                    .backends
                    .lock()
                    .unwrap()
                    .get(backend_idx)
                    .filter(|b| b.is_online)
                    .is_some_and(|b| is_already_loaded(b, &cfg.name));
                if skip {
                    info!("load '{}' skipped: already loaded on {}", cfg.name, url);
                    st.log_event(crate::dispatcher::LogEvent {
                        at: std::time::SystemTime::now(),
                        dir: "CTL",
                        user: "-".into(),
                        model: Some(cfg.name.clone()),
                        backend: None,
                        info: format!(
                            "load '{}' skipped: already loaded on {}",
                            cfg.name, url
                        ),
                    });
                    continue;
                }

                let options = LoadOptions {
                    num_ctx: cfg.max_ctx,
                    keep_alive: cfg.keep_alive,
                    identifier: cfg.identifier.clone(),
                };
                let info = match start_model_control(
                    &st,
                    backend_idx,
                    ControlAction::Load,
                    cfg.name.clone(),
                    options,
                ) {
                    Ok(canonical) => format!("load '{}' started", canonical),
                    Err(e) => format!("load '{}' rejected: {}", cfg.name, e),
                };
                st.log_event(crate::dispatcher::LogEvent {
                    at: std::time::SystemTime::now(),
                    dir: "CTL",
                    user: "-".into(),
                    model: Some(cfg.name.clone()),
                    backend: None,
                    info,
                });
            }
        });
    }
    started
}

/// Re-read the config file, swap in its `models` section, and apply it.
/// Returns the number of model entries in the new config. (Backends and
/// settings from the file are only read at startup.)
pub fn reload_model_config(state: &Arc<AppState>) -> Result<usize, String> {
    let configs = crate::config::load_config(&state.model_config_path)?.models;
    let n = configs.len();
    *state.model_limits.lock().unwrap() = crate::dispatcher::build_model_limits(&configs);
    *state.model_config.lock().unwrap() = configs;
    apply_model_config(state);
    Ok(n)
}

async fn execute_load(
    client: &reqwest::Client,
    url: &str,
    lmstudio: bool,
    canonical: &str,
    control_timeout: Duration,
    load_keep_alive: i64,
    options: &LoadOptions,
) -> Result<(), String> {
    if lmstudio {
        let mut body = json!({ "model": canonical });
        if let Some(num_ctx) = options.num_ctx {
            body["context_length"] = json!(num_ctx);
        }
        let res = client
            .post(format!("{}/api/v1/models/load", url))
            .timeout(control_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if res.status().is_success() {
            let ok_body = res.text().await.unwrap_or_default();
            debug!("LM Studio load '{}' response: {}", canonical, ok_body);
            Ok(())
        } else if res.status() == StatusCode::NOT_FOUND {
            Err(format!(
                "backend {} has no /api/v1/models/load (LM Studio < 0.3.6?); load the model via the LM Studio app or `lms load`",
                url
            ))
        } else {
            Err(err_from_response(res).await)
        }
    } else {
        // Ollama: empty-prompt generate loads the model into memory.
        // An explicit long keep_alive makes the load "sticky" (Ollama's
        // default is only 5 minutes).
        let mut body = json!({
            "model": canonical,
            "stream": false,
            "keep_alive": options.keep_alive.unwrap_or(load_keep_alive),
        });
        if let Some(num_ctx) = options.num_ctx {
            body["options"] = json!({ "num_ctx": num_ctx });
        }
        let res = client
            .post(format!("{}/api/generate", url))
            .timeout(control_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if res.status().is_success() {
            let _ = res.text().await;
            Ok(())
        } else {
            Err(err_from_response(res).await)
        }
    }
}

async fn fetch_lmstudio_instance_id(
    client: &reqwest::Client,
    url: &str,
    canonical: &str,
    control_timeout: Duration,
) -> Result<String, String> {
    let res = client
        .get(format!("{}/api/v1/models", url))
        .timeout(control_timeout)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;
    let body = res
        .text()
        .await
        .map_err(|e| format!("read failed: {}", e))?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid /api/v1/models response: {}", e))?;
    let models = v
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "invalid /api/v1/models response".to_string())?;
    let entry = models
        .iter()
        .find(|m| {
            let key = m.get("key").and_then(|k| k.as_str()).unwrap_or("");
            let id = m.get("id").and_then(|k| k.as_str()).unwrap_or("");
            key == canonical || id == canonical
        })
        .ok_or_else(|| format!("model '{}' not found in backend model list", canonical))?;
    entry
        .get("loaded_instances")
        .and_then(|li| li.as_array())
        .and_then(|arr| arr.iter().next())
        .and_then(|i| i.get("id").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "model '{}' has no loaded instance on this backend",
                canonical
            )
        })
}

async fn execute_unload(
    client: &reqwest::Client,
    url: &str,
    lmstudio: bool,
    canonical: &str,
    control_timeout: Duration,
    cached_native: &[LmModelInfo],
) -> Result<(), String> {
    if lmstudio {
        // Unload needs the loaded *instance id*. Use the health-check cache when
        // possible, otherwise fetch a fresh native listing.
        let instance_id = match cached_native
            .iter()
            .find(|m| m.key == canonical)
            .and_then(|m| m.loaded_instance_ids.first().cloned())
        {
            Some(id) => id,
            None => fetch_lmstudio_instance_id(client, url, canonical, control_timeout).await?,
        };

        let body = json!({ "instance_id": instance_id });
        let res = client
            .post(format!("{}/api/v1/models/unload", url))
            .timeout(control_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if res.status().is_success() {
            let _ = res.text().await;
            Ok(())
        } else if res.status() == StatusCode::NOT_FOUND {
            Err(format!(
                "backend {} has no /api/v1/models/unload (LM Studio < 0.3.6?); unload the model via the LM Studio app or `lms unload`",
                url
            ))
        } else {
            Err(err_from_response(res).await)
        }
    } else {
        // Ollama: empty-prompt generate with keep_alive 0 expires the runner.
        let body = json!({
            "model": canonical,
            "stream": false,
            "keep_alive": 0,
        });
        let res = client
            .post(format!("{}/api/generate", url))
            .timeout(control_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;
        if res.status().is_success() {
            // Ollama signals success with done_reason: "unload".
            if let Ok(v) = res.json::<Value>().await {
                let done_reason = v.get("done_reason").and_then(|d| d.as_str()).unwrap_or("");
                if done_reason != "unload" {
                    warn!(
                        "Ollama unload of '{}' returned done_reason={:?}; model may still be resident",
                        canonical, done_reason
                    );
                }
            }
            Ok(())
        } else {
            Err(err_from_response(res).await)
        }
    }
}

/// Extract a human-readable error from an HTTP error response body.
async fn err_from_response(res: reqwest::Response) -> String {
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error").and_then(|e| {
                e.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| Some(e.to_string()))
            })
        })
        .unwrap_or(text);
    let msg: String = if msg.chars().count() > 300 {
        let truncated: String = msg.chars().take(300).collect();
        format!("{}…", truncated)
    } else {
        msg
    };
    format!("HTTP {}: {}", status, msg)
}

/// Validate the request, resolve the model name, register the op and spawn
/// the async work. Returns the resolved canonical model name.
///
/// Lock order is `control_ops` before `backends` (mirrored by the completion
/// task) to avoid deadlocks.
pub fn start_model_control(
    state: &Arc<AppState>,
    backend_idx: usize,
    action: ControlAction,
    model: String,
    options: LoadOptions,
) -> Result<String, String> {
    let canonical = {
        let ops = state.control_ops.lock().unwrap();
        if ops.contains_key(&backend_idx) {
            return Err("a model control operation is already in progress on this backend".into());
        }
        let backends = state.backends.lock().unwrap();
        let b = backends
            .get(backend_idx)
            .ok_or_else(|| format!("unknown backend index {}", backend_idx))?;
        if !b.is_online {
            return Err("backend is offline".into());
        }
        if b.active_requests > 0 {
            return Err(
                "backend is busy with active requests; wait for them to finish before changing loaded models".into(),
            );
        }
        if !supports_control(b) {
            return Err(format!(
                "backend {} does not expose a model load/unload API (detected type: {})",
                b.url,
                b.api_type.display()
            ));
        }
        if action == ControlAction::Unload {
            let is_loaded = b
                .loaded_models
                .iter()
                .any(|m| smart_model_match_one(&model, m));
            if !is_loaded {
                return Err(format!(
                    "model '{}' is not currently loaded on this backend",
                    model.trim()
                ));
            }
        }
        let mut available: Vec<&String> = b.available_models.iter().collect();
        available.sort();
        resolve_model_name(b, &model).ok_or_else(|| {
            format!(
                "model '{}' not found on backend {}; available: {:?}",
                model.trim(),
                b.url,
                available
            )
        })?
    };

    let url = state
        .backends
        .lock()
        .unwrap()
        .get(backend_idx)
        .map(|b| b.url.clone())
        .unwrap_or_default();
    info!(
        "Model control: {} '{}' (requested: {}) on {}{}",
        action.label(),
        canonical,
        model.trim(),
        url,
        options
            .identifier
            .as_deref()
            .map(|id| format!(" [id: {}]", id))
            .unwrap_or_default()
    );

    state.control_ops.lock().unwrap().insert(
        backend_idx,
        ControlOp {
            backend_idx,
            action,
            requested: model.trim().to_string(),
            canonical: canonical.clone(),
            identifier: options.identifier.clone(),
            started: Instant::now(),
        },
    );

    let state = state.clone();
    let op_model = canonical.clone();
    let identifier = options.identifier.clone();
    let load_options = LoadOptions {
        num_ctx: options.num_ctx,
        keep_alive: options.keep_alive,
        identifier: options.identifier.clone(),
    };
    tokio::spawn(async move {
        let canonical = op_model;
        let (backend_url, lmstudio, timeout, keep_alive, cached_native) = {
            let backends = state.backends.lock().unwrap();
            let b = &backends[backend_idx];
            (
                b.url.clone(),
                b.lmstudio,
                state.timeout,
                load_options.keep_alive.unwrap_or(state.load_keep_alive),
                b.native_models.clone(),
            )
        };
        // Loading big models can take a long time; never let the proxy request
        // timeout truncate a control operation.
        let control_timeout = Duration::from_secs(timeout.max(600));
        let result = if action == ControlAction::Load {
            execute_load(
                &state.client,
                &backend_url,
                lmstudio,
                &canonical,
                control_timeout,
                keep_alive,
                &load_options,
            )
            .await
        } else {
            execute_unload(
                &state.client,
                &backend_url,
                lmstudio,
                &canonical,
                control_timeout,
                &cached_native,
            )
            .await
        };

        match &result {
            Ok(()) => info!(
                "Model control finished: {} '{}' on {}",
                action.label(),
                canonical,
                backend_url
            ),
            Err(e) => warn!(
                "Model control failed: {} '{}' on {}: {}",
                action.label(),
                canonical,
                backend_url,
                e
            ),
        }

        // Drop the in-flight op, record the outcome, then refresh the
        // backend's model state immediately (instead of waiting for the 10s
        // health tick).
        {
            let mut ops = state.control_ops.lock().unwrap();
            ops.remove(&backend_idx);
            let mut hist = state.control_history.lock().unwrap();
            hist.push_back(ControlResult {
                backend_idx,
                action,
                model: canonical.clone(),
                identifier: identifier.clone(),
                ok: result.is_ok(),
                error: result.clone().err(),
                finished_at: Instant::now(),
            });
            if hist.len() > MAX_HISTORY {
                hist.pop_front();
            }
        }

        let probe = probe_backend(&state.client, &backend_url, &HashSet::new()).await;
        {
            let mut backends = state.backends.lock().unwrap();
            if let Some(b) = backends.get_mut(backend_idx) {
                apply_probe(b, probe);
                if result.is_ok() && action == ControlAction::Load {
                    b.current_model = Some(canonical.clone());
                }
            }
        }

        // Wake the dispatcher: the backend is no longer control-busy and the
        // requested model may now be loaded/available.
        state.notify.notify_one();
    });

    Ok(canonical)
}

// ---------------------------------------------------------------------------
// Admin HTTP API
// ---------------------------------------------------------------------------

/// Request body for `POST /admin/models/load` and `POST /admin/models/unload`.
#[derive(Debug, Deserialize)]
pub struct ControlRequest {
    /// Backend selector: numeric index (0-based), URL (exact or substring),
    /// or the string `"any"` to let the proxy pick a suitable backend.
    pub backend: Value,
    /// Model name as the client knows it (resolved against the backend).
    pub model: String,
    /// Optional max context window (Ollama `options.num_ctx` / LM Studio
    /// `context_length`).
    pub num_ctx: Option<u64>,
    /// Optional `keep_alive` override (seconds) for Ollama loads.
    pub keep_alive: Option<i64>,
    /// Optional identifier/label attached to the operation.
    pub identifier: Option<String>,
}

type AdminError = (StatusCode, Value);

fn parse_backend_selector(
    state: &AppState,
    sel: &Value,
    action: ControlAction,
    model: &str,
) -> Result<usize, AdminError> {
    let backends = state.backends.lock().unwrap();
    let n = backends.len();

    let idx = if let Some(i) = sel.as_u64() {
        i as usize
    } else if let Some(s) = sel.as_str() {
        let s = s.trim();
        if s.eq_ignore_ascii_case("any") {
            (0..n)
                .find(|&i| {
                    let b = &backends[i];
                    if !b.is_online || b.active_requests > 0 || !supports_control(b) {
                        return false;
                    }
                    match action {
                        ControlAction::Load => resolve_model_name(b, model).is_some(),
                        ControlAction::Unload => b.loaded_models.iter().any(|m| smart_model_match_one(model, m)),
                    }
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        json!({ "error": format!("no suitable backend found to {} model '{}'", action.label(), model) }),
                    )
                })?
        } else if let Ok(i) = s.parse::<usize>() {
            i
        } else {
            let s_low = s.to_lowercase();
            (0..n)
                .find(|&i| {
                    let u = backends[i].url.to_lowercase();
                    u == s_low || u.contains(&s_low)
                })
                .ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        json!({ "error": format!("no backend matching '{}'", s) }),
                    )
                })?
        }
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "error": "'backend' must be a number (index) or a URL string" }),
        ));
    };

    if idx >= n {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "error": format!("backend index {} out of range (0..{})", idx, n) }),
        ));
    }
    Ok(idx)
}

async fn handle_control(
    state: Arc<AppState>,
    action: ControlAction,
    req: ControlRequest,
) -> Response {
    let model = req.model.trim().to_string();
    if model.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "'model' is required" })),
        )
            .into_response();
    }

    let idx = match parse_backend_selector(&state, &req.backend, action, &model) {
        Ok(i) => i,
        Err((status, body)) => return (status, Json(body)).into_response(),
    };

    let url = state
        .backends
        .lock()
        .unwrap()
        .get(idx)
        .map(|b| b.url.clone())
        .unwrap_or_default();

    let options = LoadOptions {
        num_ctx: req.num_ctx,
        keep_alive: req.keep_alive,
        identifier: req.identifier,
    };
    let echo_options = options.clone();

    match start_model_control(&state, idx, action, model, options) {
        Ok(canonical) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "action": action.label(),
                "backend": url,
                "model": canonical,
                "num_ctx": echo_options.num_ctx,
                "keep_alive": echo_options.keep_alive,
                "identifier": echo_options.identifier,
            })),
        )
            .into_response(),
        Err(e) => {
            let status = if e.contains("already in progress") || e.contains("busy") {
                StatusCode::CONFLICT
            } else if e.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(json!({ "error": e }))).into_response()
        }
    }
}

/// `POST /admin/models/load`
pub async fn admin_model_load(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ControlRequest>,
) -> impl IntoResponse {
    handle_control(state, ControlAction::Load, req).await
}

/// `POST /admin/models/unload`
pub async fn admin_model_unload(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ControlRequest>,
) -> impl IntoResponse {
    handle_control(state, ControlAction::Unload, req).await
}

/// `GET /admin/models` — per-backend model inventory + in-flight operations.
pub async fn admin_models_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let ops = state.control_ops.lock().unwrap();
    let backends = state.backends.lock().unwrap();
    let arr: Vec<Value> = backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let mut available: Vec<String> = b.available_models.iter().cloned().collect();
            available.sort();
            let mut loaded: Vec<String> = b.loaded_models.iter().cloned().collect();
            loaded.sort();
            json!({
                "index": i,
                "url": b.url,
                "online": b.is_online,
                "api": b.api_type.display(),
                "lmstudio": b.lmstudio,
                "active_requests": b.active_requests,
                "available_models": available,
                "loaded_models": loaded,
                "operation": ops.get(&i).map(|op| json!({
                    "action": op.action.label(),
                    "model": op.canonical,
                    "requested": op.requested,
                    "elapsed_secs": op.started.elapsed().as_secs(),
                })),
            })
        })
        .collect();
    Json(Value::Array(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::BackendStatus;
    use axum::{
        Router,
        routing::{get, post},
    };

    fn test_backend(
        api_type: BackendApiType,
        lmstudio: bool,
        available: &[&str],
        loaded: &[&str],
    ) -> BackendStatus {
        BackendStatus {
            url: "http://test:11434".into(),
            active_requests: 0,
            processed_count: 0,
            is_online: true,
            api_type,
            available_models: available.iter().map(|s| s.to_string()).collect(),
            loaded_models: loaded.iter().map(|s| s.to_string()).collect(),
            loaded_ctx: HashMap::new(),
            current_model: None,
            active_by_model: HashMap::new(),
            lmstudio,
            native_models: Vec::new(),
            known_bad_endpoints: HashSet::new(),
            rejected_families: HashSet::new(),
            family_fail_counts: HashMap::new(),
        }
    }

    #[test]
    fn resolve_exact_match() {
        let b = test_backend(
            BackendApiType::Ollama,
            false,
            &["llama3:latest", "qwen2.5:7b"],
            &[],
        );
        assert_eq!(
            resolve_model_name(&b, "qwen2.5:7b").as_deref(),
            Some("qwen2.5:7b")
        );
    }

    #[test]
    fn resolve_smart_match_strips_tag_and_case() {
        let b = test_backend(BackendApiType::Ollama, false, &["llama3:latest"], &[]);
        assert_eq!(
            resolve_model_name(&b, "llama3").as_deref(),
            Some("llama3:latest")
        );
        assert_eq!(
            resolve_model_name(&b, "LLaMA3").as_deref(),
            Some("llama3:latest")
        );
    }

    #[test]
    fn resolve_substring_unique_only() {
        // "qwen" is not a full (tag-stripped) name of either model, but a
        // substring of both -> ambiguous, no guess.
        let b = test_backend(
            BackendApiType::Ollama,
            false,
            &["qwen2.5:7b", "qwen3:8b"],
            &[],
        );
        assert_eq!(resolve_model_name(&b, "qwen"), None);
        // A unique substring resolves fine
        let b2 = test_backend(
            BackendApiType::Ollama,
            false,
            &["qwen2.5:7b", "llama3:latest"],
            &[],
        );
        assert_eq!(
            resolve_model_name(&b2, "qwen").as_deref(),
            Some("qwen2.5:7b")
        );
    }

    #[test]
    fn resolve_lmstudio_display_name() {
        let mut b = test_backend(BackendApiType::OpenAi, true, &["mock/qwen2-7b"], &[]);
        b.native_models.push(LmModelInfo {
            key: "mock/qwen2-7b".into(),
            display_name: Some("Mock Qwen2 7B".into()),
            loaded_instance_ids: Vec::new(),
        });
        assert_eq!(
            resolve_model_name(&b, "Mock Qwen2 7B").as_deref(),
            Some("mock/qwen2-7b")
        );
        assert_eq!(
            resolve_model_name(&b, "qwen2").as_deref(),
            Some("mock/qwen2-7b")
        );
    }

    #[test]
    fn resolve_unknown_model() {
        let b = test_backend(BackendApiType::Ollama, false, &["llama3:latest"], &[]);
        assert_eq!(resolve_model_name(&b, "does-not-exist"), None);
        assert_eq!(resolve_model_name(&b, "   "), None);
    }

    /// Spin up an in-process backend mock speaking both Ollama and
    /// LM Studio endpoints; records received control requests. The mock
    /// tracks resident Ollama models (name -> context_length): an unload
    /// generate (`keep_alive: 0`) removes the model, any other generate
    /// re-adds it with the requested `num_ctx`. `/api/ps` reports both.
    async fn start_mock_backend() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    ) {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let resident: std::sync::Arc<
            std::sync::Mutex<std::collections::HashMap<String, u64>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            [("qwen2.5:7b".to_string(), 2048u64)].into_iter().collect(),
        ));

        let c = calls.clone();
        let resident_gen = resident.clone();
        let generate = move |Json(body): Json<Value>| {
            let c = c.clone();
            let resident = resident_gen.clone();
            async move {
                c.lock()
                    .unwrap()
                    .push(("/api/generate".into(), body.clone()));
                if body.get("keep_alive").and_then(|k| k.as_i64()) == Some(0) {
                    if let Some(name) = body.get("model").and_then(|m| m.as_str()) {
                        resident.lock().unwrap().remove(name);
                    }
                    Json(json!({"model": body["model"], "done": true, "done_reason": "unload"}))
                } else {
                    if let Some(name) = body.get("model").and_then(|m| m.as_str()) {
                        let ctx = body
                            .get("options")
                            .and_then(|o| o.get("num_ctx"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(2048);
                        resident.lock().unwrap().insert(name.to_string(), ctx);
                    }
                    Json(json!({"model": body["model"], "done": true}))
                }
            }
        };

        let c = calls.clone();
        let ls_load = move |Json(body): Json<Value>| {
            let c = c.clone();
            async move {
                c.lock()
                    .unwrap()
                    .push(("/api/v1/models/load".into(), body.clone()));
                Json(
                    json!({"type": "llm", "instance_id": body["model"], "status": "loaded", "load_time_seconds": 0.1}),
                )
            }
        };

        let c = calls.clone();
        let resident_ls_unload = resident.clone();
        let ls_unload = move |Json(body): Json<Value>| {
            let c = c.clone();
            let resident = resident_ls_unload.clone();
            async move {
                c.lock()
                    .unwrap()
                    .push(("/api/v1/models/unload".into(), body.clone()));
                if let Some(id) = body.get("instance_id").and_then(|v| v.as_str()) {
                    resident.lock().unwrap().remove(id);
                }
                Json(json!({"instance_id": body["instance_id"]}))
            }
        };

        let resident_ps = resident.clone();
        let resident_native = resident.clone();
        let app = Router::new()
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({"models": [{"name": "llama3:latest"}, {"name": "qwen2.5:7b"}]}))
                }),
            )
            .route(
                "/api/ps",
                get(move || async move {
                    let resident = resident_ps.lock().unwrap();
                    let models: Vec<Value> = resident
                        .iter()
                        .map(|(name, ctx)| json!({"name": name, "context_length": ctx}))
                        .collect();
                    Json(json!({ "models": models }))
                }),
            )
            .route("/api/generate", post(generate))
            .route(
                "/api/v1/models",
                get(move || async move {
                    let resident = resident_native.lock().unwrap();
                    let mut models = vec![json!({
                        "key": "mock/qwen2-7b",
                        "id": "mock/qwen2-7b",
                        "display_name": "Mock Qwen2 7B",
                        "loaded_instances": [{"id": "mock/qwen2-7b-instance"}]
                    })];
                    if let Some(ctx) = resident.get("qwen2.5:7b") {
                        models.push(json!({
                            "key": "qwen2.5:7b",
                            "id": "qwen2.5:7b",
                            "display_name": "Qwen 2.5 7B",
                            "loaded_instances": [{
                                "id": "qwen2.5:7b",
                                "config": {"context_length": ctx}
                            }]
                        }));
                    }
                    Json(json!({ "models": models }))
                }),
            )
            .route("/api/v1/models/load", post(ls_load))
            .route("/api/v1/models/unload", post(ls_unload));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), calls)
    }

    #[tokio::test]
    async fn probe_detects_ollama_and_lmstudio() {
        let (url, _calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let probe = probe_backend(&client, &url, &HashSet::new()).await;

        assert!(probe.is_online);
        // This mock speaks both Ollama and LM Studio -> Both + lmstudio flag
        assert!(probe.api_type == BackendApiType::Both || probe.api_type == BackendApiType::Ollama);
        assert!(probe.lmstudio);
        assert!(probe.available_models.contains("llama3:latest"));
        assert!(probe.loaded_models.contains("qwen2.5:7b")); // from /api/ps
        assert!(probe.loaded_models.contains("mock/qwen2-7b-instance")); // native
        assert_eq!(probe.native_models.len(), 2);
        assert!(probe.native_models.iter().any(|m| m.key == "mock/qwen2-7b"));
        assert_eq!(
            probe.loaded_ctx.get("qwen2.5:7b"),
            Some(&2048)
        );
    }

    #[tokio::test]
    async fn probe_classifies_and_skips_bad_endpoints() {
        // Mock mimics LM Studio for the Ollama endpoints: /api/tags works, but
        // /api/ps answers 200 with an error body and /v1/models is a 404.
        let extra_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h1 = extra_hits.clone();
        let h2 = extra_hits.clone();
        let app = Router::new()
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({"models": [{"name": "llama3:latest"}]}))
                }),
            )
            .route("/api/ps", get(move || async move {
                h1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    StatusCode::OK,
                    Json(json!({"error": "Unexpected endpoint or method. (GET /api/ps)"})),
                )
                    .into_response()
            }))
            .route("/v1/models", get(move || async move {
                h2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "Unexpected endpoint or method. (GET /v1/models)"})),
                )
                    .into_response()
            }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}");

        let client = reqwest::Client::new();
        // Full probe: both rejection styles are remembered as bad.
        let probe = probe_backend(&client, &url, &HashSet::new()).await;
        assert!(probe.is_online);
        assert!(probe.good_endpoints.contains("/api/tags"));
        assert!(probe.bad_endpoints.contains("/api/ps"), "200+error body must be bad");
        assert!(probe.bad_endpoints.contains("/v1/models"), "404 must be bad");

        // Second probe skipping the bad endpoints: they are not hit again,
        // while good endpoints stay fresh.
        let skip = probe.bad_endpoints.clone();
        let probe2 = probe_backend(&client, &url, &skip).await;
        assert_eq!(extra_hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(probe2.good_endpoints.contains("/api/tags"));
    }

    #[test]
    fn apply_probe_merges_endpoint_memory() {
        let mut b = test_backend(BackendApiType::Ollama, false, &[], &[]);
        b.known_bad_endpoints.insert("/v1/models".to_string());

        // A probe where the endpoint now works clears the bad memory.
        apply_probe(
            &mut b,
            BackendProbe {
                is_online: true,
                api_type: BackendApiType::Ollama,
                available_models: HashSet::new(),
                loaded_models: HashSet::new(),
                loaded_ctx: HashMap::new(),
                lmstudio: false,
                native_models: Vec::new(),
                bad_endpoints: HashSet::new(),
                good_endpoints: ["/v1/models".to_string()].into_iter().collect(),
            },
        );
        assert!(!b.known_bad_endpoints.contains("/v1/models"));

        // A probe where another endpoint is rejected extends the memory.
        apply_probe(
            &mut b,
            BackendProbe {
                is_online: true,
                api_type: BackendApiType::Ollama,
                available_models: HashSet::new(),
                loaded_models: HashSet::new(),
                loaded_ctx: HashMap::new(),
                lmstudio: false,
                native_models: Vec::new(),
                bad_endpoints: ["/api/ps".to_string()].into_iter().collect(),
                good_endpoints: HashSet::new(),
            },
        );
        assert!(b.known_bad_endpoints.contains("/api/ps"));
    }

    #[tokio::test]
    async fn ollama_load_and_unload_bodies() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        // Load: empty-prompt generate with long keep_alive
        execute_load(
            &client,
            &url,
            false,
            "llama3:latest",
            to,
            86400,
            &LoadOptions::default(),
        )
        .await
        .unwrap();
        // Unload: empty-prompt generate with keep_alive 0
        execute_unload(&client, &url, false, "llama3:latest", to, &[])
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "/api/generate");
        assert_eq!(calls[0].1["model"], "llama3:latest");
        assert_eq!(calls[0].1["keep_alive"], 86400);
        assert!(calls[0].1.get("prompt").is_none()); // empty prompt => load
        assert_eq!(calls[1].0, "/api/generate");
        assert_eq!(calls[1].1["keep_alive"], 0);
    }

    #[tokio::test]
    async fn ollama_load_options_num_ctx_and_keep_alive() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        let options = LoadOptions {
            num_ctx: Some(16384),
            keep_alive: Some(3600),
            identifier: Some("big-ctx".into()),
        };
        execute_load(&client, &url, false, "llama3:latest", to, 86400, &options)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["keep_alive"], 3600); // override beats default
        assert_eq!(calls[0].1["options"]["num_ctx"], 16384);
        assert!(calls[0].1.get("prompt").is_none()); // empty prompt => load
    }

    #[tokio::test]
    async fn lmstudio_load_options_context_length() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        let options = LoadOptions {
            num_ctx: Some(8192),
            ..LoadOptions::default()
        };
        execute_load(&client, &url, true, "mock/qwen2-7b", to, 86400, &options)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/api/v1/models/load");
        assert_eq!(calls[0].1["model"], "mock/qwen2-7b");
        assert_eq!(calls[0].1["context_length"], 8192);
    }

    #[tokio::test]
    async fn lmstudio_load_unload_uses_instance_id() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        execute_load(&client, &url, true, "mock/qwen2-7b", to, 86400, &LoadOptions::default())
            .await
            .unwrap();
        execute_unload(&client, &url, true, "mock/qwen2-7b", to, &[])
            .await
            .unwrap();

        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, "/api/v1/models/load");
            assert_eq!(calls[0].1["model"], "mock/qwen2-7b");
            assert_eq!(calls[1].0, "/api/v1/models/unload");
            // Unload must target the loaded instance id, not the model key
            assert_eq!(calls[1].1["instance_id"], "mock/qwen2-7b-instance");
        }

        // Cache-first: a cached instance id is used without re-fetching the list
        let cached = vec![LmModelInfo {
            key: "mock/qwen2-7b".into(),
            display_name: None,
            loaded_instance_ids: vec!["cached-instance".into()],
        }];
        execute_unload(&client, &url, true, "mock/qwen2-7b", to, &cached)
            .await
            .unwrap();
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[2].0, "/api/v1/models/unload");
            assert_eq!(calls[2].1["instance_id"], "cached-instance");
        }
    }

    #[test]
    fn already_loaded_resident_model() {
        let b = test_backend(
            BackendApiType::Ollama,
            false,
            &["llama3:latest", "qwen2.5:7b"],
            &["qwen2.5:7b"],
        );
        assert!(is_already_loaded(&b, "qwen2.5:7b"));
        // Smart match: tag/case variations of the resident model count too
        assert!(is_already_loaded(&b, "QWEN2.5"));
        assert!(!is_already_loaded(&b, "llama3")); // available but not resident
        assert!(!is_already_loaded(&b, "nope"));  // unresolvable -> false (error path)
    }

    #[test]
    fn already_loaded_lmstudio_instance() {
        let b = test_backend(
            BackendApiType::OpenAi,
            true,
            &["mock/qwen2-7b"],
            &["mock/qwen2-7b", "mock/qwen2-7b-instance"],
        );
        assert!(is_already_loaded(&b, "mock/qwen2-7b"));
    }

    #[tokio::test]
    async fn apply_model_config_skips_resident_models() {
        let (url, calls) = start_mock_backend().await;
        let state = Arc::new(AppState::new(
            vec![url.clone()],
            30,
            86400,
            60,
            1, // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
        ));
        // Simulate a probed backend: model available AND resident.
        {
            let mut backends = state.backends.lock().unwrap();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::Ollama;
            backends[0]
                .available_models
                .insert("qwen2.5:7b".into());
            backends[0].loaded_models.insert("qwen2.5:7b".into());
        }
        state.model_config.lock().unwrap().push(crate::config::ModelConfig {
            name: "qwen2.5:7b".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 1,
            backends: Vec::new(),
        });

        assert_eq!(apply_model_config(&state), 1);

        // Wait for the skip event to land in the log ring.
        let mut info = String::new();
        for _ in 0..200 {
            if let Some(ev) = state.logs.lock().unwrap().iter().find(|e| e.dir == "CTL") {
                info = ev.info.clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(info.contains("skipped"), "expected a skip event, got: {}", info);

        // No load request may have hit the backend.
        let calls = calls.lock().unwrap();
        assert!(calls.is_empty(), "no control call expected, got {:?}", calls);
    }

    #[tokio::test]
    async fn apply_model_config_reloads_resident_model_with_wrong_ctx() {
        let (url, calls) = start_mock_backend().await;
        let state = Arc::new(AppState::new(
            vec![url.clone()],
            30,
            86400,
            60,
            1, // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
        ));
        // Simulate a probed backend: model resident with context 2048 (the
        // mock's /api/ps reports context_length), config wants 16384.
        {
            let mut backends = state.backends.lock().unwrap();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::Ollama;
            backends[0]
                .available_models
                .insert("qwen2.5:7b".into());
            backends[0].loaded_models.insert("qwen2.5:7b".into());
            backends[0].loaded_ctx.insert("qwen2.5:7b".into(), 2048);
        }
        state.model_config.lock().unwrap().push(crate::config::ModelConfig {
            name: "qwen2.5:7b".into(),
            identifier: None,
            max_ctx: Some(16384),
            keep_alive: None,
            max_concurrent_requests: 1,
            backends: Vec::new(),
        });

        assert_eq!(apply_model_config(&state), 1);

        // Wait for the load op to finish and be recorded in history.
        let mut ok = false;
        for _ in 0..200 {
            if state.control_history.lock().unwrap().back().is_some_and(|r| r.ok) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ok, "reload should succeed");

        // Unload + reload with the configured context_length must have been
        // sent (the mock speaks LM Studio native, so the control path is
        // /api/v1/models/unload + /api/v1/models/load).
        let calls = calls.lock().unwrap();
        assert!(
            calls.iter().any(|(ep, b)| ep == "/api/v1/models/unload"
                && b["instance_id"] == "qwen2.5:7b"),
            "expected an unload call, got {:?}",
            *calls
        );
        assert!(
            calls.iter().any(|(ep, b)| ep == "/api/v1/models/load"
                && b["model"] == "qwen2.5:7b"
                && b["context_length"] == 16384),
            "expected a reload with context_length 16384, got {:?}",
            *calls
        );
    }

    #[tokio::test]
    async fn apply_model_config_loads_missing_models() {
        let (url, calls) = start_mock_backend().await;
        let state = Arc::new(AppState::new(
            vec![url.clone()],
            30,
            86400,
            60,
            1, // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
        ));
        // llama3 is available but NOT resident (mock /api/ps lists only qwen2.5:7b).
        {
            let mut backends = state.backends.lock().unwrap();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::Ollama;
            backends[0]
                .available_models
                .insert("llama3:latest".into());
        }
        state.model_config.lock().unwrap().push(crate::config::ModelConfig {
            name: "llama3".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 1,
            backends: Vec::new(),
        });

        apply_model_config(&state);

        // Wait for the load op to finish and be recorded in history.
        let mut ok = false;
        for _ in 0..200 {
            if state.control_history.lock().unwrap().back().is_some_and(|r| r.ok) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ok, "load should succeed");

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        // The mock speaks both Ollama and LM Studio, so the load may go out on
        // either path — what matters is exactly one load for the canonical name.
        assert_eq!(calls[0].1["model"], "llama3:latest"); // smart-matched canonical name
    }
}
