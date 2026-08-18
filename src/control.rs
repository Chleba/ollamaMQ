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
use std::collections::HashSet;
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
    pub started: Instant,
}

/// Terminal record of a finished control op (kept for TUI feedback).
#[derive(Clone, Debug)]
pub struct ControlResult {
    pub backend_idx: usize,
    pub action: ControlAction,
    pub model: String,
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
    /// True when the backend speaks the LM Studio native REST API.
    pub lmstudio: bool,
    /// LM Studio native model list (keys, display names, loaded instance ids).
    pub native_models: Vec<LmModelInfo>,
}

/// Probe one backend: online status, API type, available models, loaded
/// models, and (for LM Studio) the native model list.
pub async fn probe_backend(client: &reqwest::Client, url: &str) -> BackendProbe {
    let mut is_online = false;
    let mut detected_type = BackendApiType::Unknown;
    let mut models = HashSet::new();
    let mut loaded = HashSet::new();
    let mut lmstudio = false;
    let mut native_models: Vec<LmModelInfo> = Vec::new();

    // Probe Ollama API: /api/tags → expects {"models": [...]}
    {
        let check_url = format!("{}/api/tags", url);
        match client.get(&check_url).send().await {
            Ok(res) if res.status().is_success() => {
                is_online = true;
                if let Ok(body) = res.text().await {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        if let Some(models_json) = json.get("models").and_then(|m| m.as_array()) {
                            detected_type = detected_type.merge(BackendApiType::Ollama);
                            debug!("Backend {} confirmed Ollama API via /api/tags", url);
                            for m in models_json {
                                if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                                    models.insert(name.to_string());
                                }
                            }
                        } else {
                            warn!(
                                "Backend {} responded 200 to /api/tags but 'models' array not found or invalid. Body: {}",
                                url, body
                            );
                        }
                    } else {
                        warn!(
                            "Backend {} responded 200 to /api/tags but body is not valid JSON",
                            url
                        );
                    }
                }
            }
            Ok(res) => {
                debug!(
                    "Backend {} /api/tags returned status: {}",
                    url,
                    res.status()
                );
            }
            Err(e) => {
                debug!("Backend {} /api/tags error: {}", url, e);
            }
        }

        // Also check for loaded models via /api/ps if it was an Ollama-like response
        if is_online {
            let ps_url = format!("{}/api/ps", url);
            if let Ok(res) = client.get(&ps_url).send().await
                && res.status().is_success()
                && let Ok(body) = res.text().await
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(models_json) = json.get("models").and_then(|m| m.as_array())
            {
                for m in models_json {
                    if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                        loaded.insert(name.to_string());
                    }
                }
            }
        }
    }

    // Probe OpenAI API: /v1/models → expects {"data": [...]}
    {
        let check_url = format!("{}/v1/models", url);
        match client.get(&check_url).send().await {
            Ok(res) if res.status().is_success() => {
                is_online = true;
                if let Ok(body) = res.text().await {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        if let Some(data_json) = json.get("data").and_then(|d| d.as_array()) {
                            detected_type = detected_type.merge(BackendApiType::OpenAi);
                            debug!("Backend {} confirmed OpenAI API via /v1/models", url);
                            for m in data_json {
                                if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                                    models.insert(id.to_string());
                                }
                            }
                        } else {
                            warn!(
                                "Backend {} responded 200 to /v1/models but 'data' array not found or invalid. Body: {}",
                                url, body
                            );
                        }
                    } else {
                        warn!(
                            "Backend {} responded 200 to /v1/models but body is not valid JSON",
                            url
                        );
                    }
                }
            }
            Ok(res) => {
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
    // generic OpenAI servers 404 it (handled gracefully inside the helper).
    if let Some((ls_loaded, ls_native)) = probe_lmstudio_native(client, url).await {
        lmstudio = true;
        native_models = ls_native;
        loaded.extend(ls_loaded);
    }

    // Fallback: just check root if both specific probes failed
    if !is_online {
        let check_url = format!("{}/", url);
        if let Ok(res) = client.get(&check_url).send().await
            && res.status().is_success()
        {
            is_online = true;
        }
    }

    BackendProbe {
        is_online,
        api_type: detected_type,
        available_models: models,
        loaded_models: loaded,
        lmstudio,
        native_models,
    }
}

/// Probe LM Studio's native `/api/v1/models` endpoint.
/// Returns `Some((loaded_names, native_model_list))` when the backend speaks
/// the LM Studio native REST API, otherwise `None` (404 / non-LM Studio).
async fn probe_lmstudio_native(
    client: &reqwest::Client,
    url: &str,
) -> Option<(HashSet<String>, Vec<LmModelInfo>)> {
    let res = client
        .get(format!("{}/api/v1/models", url))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let body = res.text().await.ok()?;
    let json = serde_json::from_str::<serde_json::Value>(&body).ok()?;
    let models_json = json.get("models").and_then(|m| m.as_array())?;

    let mut loaded = HashSet::new();
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
                loaded.insert(id.clone());
            }
        }
        native.push(LmModelInfo {
            key: key.clone(),
            display_name,
            loaded_instance_ids: instance_ids,
        });
    }
    Some((loaded, native))
}

/// Resolve a user-supplied model name to the backend's canonical name.
///
/// Order: exact match → smart match (`:latest` / case-insensitive) →
/// case-insensitive substring (unique match only) → LM Studio native list
/// (key or display name). Returns `None` when nothing or multiple candidates
/// match.
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

/// Does this backend expose a model load/unload API?
fn supports_control(b: &BackendStatus) -> bool {
    b.lmstudio || matches!(b.api_type, BackendApiType::Ollama | BackendApiType::Both)
}

async fn execute_load(
    client: &reqwest::Client,
    url: &str,
    lmstudio: bool,
    canonical: &str,
    control_timeout: Duration,
    load_keep_alive: u64,
) -> Result<(), String> {
    if lmstudio {
        let body = json!({ "model": canonical });
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
        let body = json!({
            "model": canonical,
            "stream": false,
            "keep_alive": load_keep_alive,
        });
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
        "Model control: {} '{}' (requested: {}) on {}",
        action.label(),
        canonical,
        model.trim(),
        url
    );

    state.control_ops.lock().unwrap().insert(
        backend_idx,
        ControlOp {
            backend_idx,
            action,
            requested: model.trim().to_string(),
            canonical: canonical.clone(),
            started: Instant::now(),
        },
    );

    let state = state.clone();
    let op_model = canonical.clone();
    tokio::spawn(async move {
        let canonical = op_model;
        let (backend_url, lmstudio, timeout, keep_alive, cached_native) = {
            let backends = state.backends.lock().unwrap();
            let b = &backends[backend_idx];
            (
                b.url.clone(),
                b.lmstudio,
                state.timeout,
                state.load_keep_alive,
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
                ok: result.is_ok(),
                error: result.clone().err(),
                finished_at: Instant::now(),
            });
            if hist.len() > MAX_HISTORY {
                hist.pop_front();
            }
        }

        let probe = probe_backend(&state.client, &backend_url).await;
        {
            let mut backends = state.backends.lock().unwrap();
            if let Some(b) = backends.get_mut(backend_idx) {
                b.is_online = probe.is_online;
                b.api_type = probe.api_type;
                b.available_models = probe.available_models;
                b.loaded_models = probe.loaded_models;
                b.lmstudio = probe.lmstudio;
                b.native_models = probe.native_models;
                if result.is_ok() && action == ControlAction::Load {
                    b.current_model = Some(canonical.clone());
                }
            }
        }
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

    match start_model_control(&state, idx, action, model) {
        Ok(canonical) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "action": action.label(),
                "backend": url,
                "model": canonical,
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
            current_model: None,
            lmstudio,
            native_models: Vec::new(),
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
    /// LM Studio endpoints; records received control requests.
    async fn start_mock_backend() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    ) {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let c = calls.clone();
        let generate = move |Json(body): Json<Value>| {
            let c = c.clone();
            async move {
                c.lock()
                    .unwrap()
                    .push(("/api/generate".into(), body.clone()));
                if body.get("keep_alive").and_then(|k| k.as_i64()) == Some(0) {
                    Json(json!({"model": body["model"], "done": true, "done_reason": "unload"}))
                } else {
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
        let ls_unload = move |Json(body): Json<Value>| {
            let c = c.clone();
            async move {
                c.lock()
                    .unwrap()
                    .push(("/api/v1/models/unload".into(), body.clone()));
                Json(json!({"instance_id": body["instance_id"]}))
            }
        };

        let app = Router::new()
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({"models": [{"name": "llama3:latest"}, {"name": "qwen2.5:7b"}]}))
                }),
            )
            .route(
                "/api/ps",
                get(|| async { Json(json!({"models": [{"name": "qwen2.5:7b"}]})) }),
            )
            .route("/api/generate", post(generate))
            .route(
                "/api/v1/models",
                get(|| async {
                    Json(json!({"models": [{
                        "key": "mock/qwen2-7b",
                        "id": "mock/qwen2-7b",
                        "display_name": "Mock Qwen2 7B",
                        "loaded_instances": [{"id": "mock/qwen2-7b-instance"}]
                    }]}))
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
        let probe = probe_backend(&client, &url).await;

        assert!(probe.is_online);
        // This mock speaks both Ollama and LM Studio -> Both + lmstudio flag
        assert!(probe.api_type == BackendApiType::Both || probe.api_type == BackendApiType::Ollama);
        assert!(probe.lmstudio);
        assert!(probe.available_models.contains("llama3:latest"));
        assert!(probe.loaded_models.contains("qwen2.5:7b")); // from /api/ps
        assert!(probe.loaded_models.contains("mock/qwen2-7b-instance")); // native
        assert_eq!(probe.native_models.len(), 1);
        assert_eq!(probe.native_models[0].key, "mock/qwen2-7b");
    }

    #[tokio::test]
    async fn ollama_load_and_unload_bodies() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        // Load: empty-prompt generate with long keep_alive
        execute_load(&client, &url, false, "llama3:latest", to, 86400)
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
    async fn lmstudio_load_unload_uses_instance_id() {
        let (url, calls) = start_mock_backend().await;
        let client = reqwest::Client::new();
        let to = Duration::from_secs(5);

        execute_load(&client, &url, true, "mock/qwen2-7b", to, 86400)
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
}
