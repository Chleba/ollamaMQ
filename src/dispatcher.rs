use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, warn};

const BLOCKED_FILE: &str = "blocked_items.json";

#[derive(Serialize, Deserialize, Default)]
struct BlockedConfig {
    ips: HashSet<IpAddr>,
    users: HashSet<String>,
}

pub enum ResponsePart {
    Status(StatusCode, HeaderMap),
    Chunk(Bytes),
    Error(reqwest::Error),
}

pub struct Task {
    pub method: Method,
    pub path: String,
    /// Client identity (ConnectInfo addr string) so OUT request-log records
    /// carry the same user as their IN record.
    pub user: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub responder: mpsc::Sender<ResponsePart>,
    pub requested_model: Option<String>,
    /// Set once a "no backend available" warning has been logged for this
    /// task, so stuck requests are visible without spamming the log.
    pub stuck_warned: bool,
    /// When the request was queued; used to fail fast (503) when no backend
    /// can ever serve it instead of letting the client hang forever.
    pub queued_at: std::time::Instant,
}

/// Which API flavours this backend speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BackendApiType {
    #[default]
    Unknown,
    Ollama,
    OpenAi,
    Both,
}

impl BackendApiType {
    pub fn supports(&self, api_family: ApiFamily) -> bool {
        match (self, api_family) {
            (BackendApiType::Both, _) => true,
            (_, ApiFamily::Unknown) => true, // unknown path → any backend is fine
            (BackendApiType::Ollama, ApiFamily::Ollama) => true,
            (BackendApiType::OpenAi, ApiFamily::OpenAi) => true,
            _ => false,
        }
    }

    pub fn merge(self, other: BackendApiType) -> BackendApiType {
        match (self, other) {
            (_, BackendApiType::Both) | (BackendApiType::Both, _) => BackendApiType::Both,
            (BackendApiType::Ollama, BackendApiType::OpenAi)
            | (BackendApiType::OpenAi, BackendApiType::Ollama) => BackendApiType::Both,
            (_, t) => t,
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            BackendApiType::Unknown => "???",
            BackendApiType::Ollama => "Ollama",
            BackendApiType::OpenAi => "OpenAI",
            BackendApiType::Both => "O+OA",
        }
    }
}

/// Which API family a request path belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiFamily {
    Ollama,
    OpenAi,
    Unknown,
}

impl ApiFamily {
    pub fn display(&self) -> &'static str {
        match self {
            ApiFamily::Ollama => "Ollama",
            ApiFamily::OpenAi => "OpenAI",
            ApiFamily::Unknown => "unknown",
        }
    }
}

pub fn detect_api_family(path: &str) -> ApiFamily {
    if path.starts_with("/api/") {
        ApiFamily::Ollama
    } else if path.starts_with("/v1/") {
        ApiFamily::OpenAi
    } else {
        ApiFamily::Unknown
    }
}

/// One entry from LM Studio's native model list (`GET /api/v1/models`):
/// the model key, optional display name, and ids of currently loaded instances
/// (used by model control to resolve the `instance_id` for unloading).
#[derive(Clone, Debug)]
pub struct LmModelInfo {
    pub key: String,
    pub display_name: Option<String>,
    pub loaded_instance_ids: Vec<String>,
}

#[derive(Clone)]
pub struct BackendStatus {
    pub url: String,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    pub api_type: BackendApiType,
    pub available_models: HashSet<String>,
    pub loaded_models: HashSet<String>,
    /// True when the backend reported loaded-model data in its latest probe
    /// (Ollama `/api/ps` or LM Studio `/api/v1/models`). `false` means the
    /// loaded state is UNKNOWN (no such endpoint, or it is skipped as bad) —
    /// for such backends (e.g. vLLM) "available" implies "ready to serve".
    pub loaded_state_known: bool,
    /// Actual context window of each resident model, keyed by model name/key
    /// (Ollama `/api/ps` `context_length`; LM Studio loaded instance
    /// `config.context_length`). Used to detect resident models whose
    /// context doesn't match the configured `max_ctx`.
    pub loaded_ctx: HashMap<String, u64>,
    pub current_model: Option<String>,
    /// In-flight request count per normalized model name. Only populated for
    /// requests that carry a model; enforces the per-model
    /// `max_concurrent_requests` limits from appconf.yaml (Plan D).
    pub active_by_model: HashMap<String, u32>,
    /// True when the backend speaks the LM Studio native REST API
    /// (`/api/v1/models*`), which enables model load/unload control.
    pub lmstudio: bool,
    /// LM Studio native model list (empty for other backend types).
    pub native_models: Vec<LmModelInfo>,
    /// Endpoints this backend is known to reject (remembered from probes);
    /// the health loop skips them until a full re-probe.
    pub known_bad_endpoints: HashSet<String>,
    /// API families learned (from real traffic) to be rejected by this
    /// backend, e.g. Ollama-family requests answered with an endpoint error
    /// by an LM Studio server. The scheduler excludes such backends for that
    /// family until a request of the family succeeds or the backend restarts.
    pub rejected_families: HashSet<ApiFamily>,
    /// Consecutive endpoint-rejection responses per API family; at 2 the
    /// family moves into `rejected_families`. Cleared on any success.
    pub family_fail_counts: HashMap<ApiFamily, u8>,
}

pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processing_counts: Mutex<HashMap<String, usize>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub user_ips: Mutex<HashMap<String, IpAddr>>,
    pub blocked_ips: Mutex<HashSet<IpAddr>>,
    pub blocked_users: Mutex<HashSet<String>>,
    pub vip_user: Mutex<Option<String>>,
    pub boost_user: Mutex<Option<String>>,
    pub global_counter: Mutex<usize>,
    pub notify: Notify,
    pub backend_freed: Notify,
    pub backends: Mutex<Vec<BackendStatus>>,
    pub last_backend_idx: Mutex<usize>,
    pub timeout: u64,
    /// How long a request may wait when no backend can *ever* serve it
    /// (all offline / wrong API family / model absent everywhere) before the
    /// proxy answers 503 instead of letting the client hang. Requests that
    /// are merely waiting for a busy or loading backend are exempt.
    pub stuck_timeout: std::time::Duration,
    /// Shared HTTP client for proxying and backend control probes.
    pub client: reqwest::Client,
    /// In-flight model control operations, keyed by backend index
    /// (at most one per backend). Backends with an entry are treated as busy
    /// by the scheduler.
    pub control_ops: Mutex<HashMap<usize, crate::control::ControlOp>>,
    /// Recent finished control operations (bounded), for TUI feedback.
    pub control_history: Mutex<VecDeque<crate::control::ControlResult>>,
    /// `keep_alive` (seconds) sent with control "load" requests so explicitly
    /// loaded models stay resident (Ollama's own default is only 5 minutes).
    pub load_keep_alive: i64,
    /// Max simultaneous in-flight requests per backend (default 1 — the
    /// historical behavior). Per-model sub-limits come from `model_limits`.
    pub max_concurrent_per_backend: u32,
    /// Per-model concurrency limits keyed by normalized model name; rebuilt
    /// whenever the model config changes. Unlisted models default to 1.
    pub model_limits: Mutex<HashMap<String, u32>>,
    /// Model configuration from `appconf.yaml` (models to load on backends,
    /// with load settings). Re-read with the TUI 'r' key.
    pub model_config: Mutex<Vec<crate::config::ModelConfig>>,
    /// Path of the model config file.
    pub model_config_path: String,
    /// Ring buffer of recent request/control events for the TUI Logs panel.
    pub logs: Mutex<VecDeque<LogEvent>>,
    /// Size-rotated request/response content logger (no-op when disabled).
    pub reqlog: crate::reqlog::RequestLogger,
    /// Max bytes of body content captured per record in the request log.
    pub log_content_limit: usize,
}

/// One line of the TUI Logs panel: a request entering ("IN") or leaving
/// ("OUT") the proxy, or a model-control action ("CTL").
#[derive(Clone, Debug)]
pub struct LogEvent {
    pub at: std::time::SystemTime,
    /// "IN" | "OUT" | "CTL"
    pub dir: &'static str,
    pub user: String,
    pub model: Option<String>,
    pub backend: Option<String>,
    /// Status code or short note ("queued", "dropped", …).
    pub info: String,
    /// Optional body/response content preview (truncated to the configured
    /// limit) for IN/OUT events; None for drops and control actions.
    pub content: Option<String>,
}

/// How many log events to keep in the ring buffer.
const MAX_LOG_EVENTS: usize = 300;
/// How many of the newest events may keep their (potentially large) content; older ones have it stripped to bound memory.
const MAX_LOG_CONTENT_EVENTS: usize = 100;

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend_urls: Vec<String>,
        timeout: u64,
        load_keep_alive: i64,
        stuck_timeout_secs: u64,
        max_concurrent_per_backend: u32,
        model_config_path: String,
        model_config: Vec<crate::config::ModelConfig>,
        reqlog: crate::reqlog::RequestLogger,
        log_content_limit: usize,
    ) -> Self {
        let (blocked_ips, blocked_users) = Self::load_blocked_items();
        let backends = backend_urls
            .into_iter()
            .map(|url| BackendStatus {
                url,
                active_requests: 0,
                processed_count: 0,
                is_online: true,
                api_type: BackendApiType::Unknown,
                available_models: HashSet::new(),
                loaded_models: HashSet::new(),
                loaded_state_known: false,
                loaded_ctx: HashMap::new(),
                current_model: None,
                active_by_model: HashMap::new(),
                lmstudio: false,
                native_models: Vec::new(),
                known_bad_endpoints: HashSet::new(),
                rejected_families: HashSet::new(),
                family_fail_counts: HashMap::new(),
            })
            .collect();

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .unwrap();

        Self {
            queues: Mutex::new(HashMap::new()),
            processing_counts: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            user_ips: Mutex::new(HashMap::new()),
            blocked_ips: Mutex::new(blocked_ips),
            blocked_users: Mutex::new(blocked_users),
            vip_user: Mutex::new(None),
            boost_user: Mutex::new(None),
            global_counter: Mutex::new(0),
            notify: Notify::new(),
            backend_freed: Notify::new(),
            backends: Mutex::new(backends),
            last_backend_idx: Mutex::new(0),
            timeout,
            stuck_timeout: std::time::Duration::from_secs(stuck_timeout_secs),
            client,
            control_ops: Mutex::new(HashMap::new()),
            control_history: Mutex::new(VecDeque::new()),
            load_keep_alive,
            max_concurrent_per_backend,
            model_limits: Mutex::new(build_model_limits(&model_config)),
            model_config: Mutex::new(model_config),
            model_config_path,
            logs: Mutex::new(VecDeque::new()),
            reqlog,
            log_content_limit,
        }
    }

    fn load_blocked_items() -> (HashSet<IpAddr>, HashSet<String>) {
        if let Ok(content) = fs::read_to_string(BLOCKED_FILE) {
            if let Ok(config) = serde_json::from_str::<BlockedConfig>(&content) {
                return (config.ips, config.users);
            }
        }
        (HashSet::new(), HashSet::new())
    }

    fn save_blocked_items(&self) {
        let config = BlockedConfig {
            ips: self.blocked_ips.lock().unwrap().clone(),
            users: self.blocked_users.lock().unwrap().clone(),
        };
        if let Ok(content) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(BLOCKED_FILE, content);
        }
    }

    pub fn block_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.insert(ip);
        }
        self.save_blocked_items();
        warn!("IP blocked: {}", ip);
    }

    pub fn block_user(&self, user_id: String) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.insert(user_id.clone());
        }
        self.save_blocked_items();
        warn!("User blocked: {}", user_id);
    }

    #[allow(dead_code)]
    pub fn unblock_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.remove(&ip);
        }
        self.save_blocked_items();
        info!("IP unblocked: {}", ip);
    }

    #[allow(dead_code)]
    pub fn unblock_user(&self, user_id: &str) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.remove(user_id);
        }
        self.save_blocked_items();
        info!("User unblocked: {}", user_id);
    }

    pub fn is_ip_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ips.lock().unwrap().contains(ip)
    }

    pub fn is_user_blocked(&self, user_id: &str) -> bool {
        self.blocked_users.lock().unwrap().contains(user_id)
    }

    /// Append an event to the logs ring buffer (bounded).
    pub fn log_event(&self, ev: LogEvent) {
        let mut logs = self.logs.lock().unwrap();
        logs.push_back(ev);
        while logs.len() > MAX_LOG_EVENTS {
            logs.pop_front();
        }
        // Bound memory: only the newest MAX_LOG_CONTENT_EVENTS may keep
        // their (potentially large) content — strip it from older events,
        // walking the ring from the front (oldest first).
        let mut content_events = logs.iter().filter(|e| e.content.is_some()).count();
        let mut i = 0;
        while content_events > MAX_LOG_CONTENT_EVENTS {
            if let Some(e) = logs.get_mut(i).filter(|e| e.content.is_some()) {
                e.content = None;
                content_events -= 1;
            }
            i += 1;
        }
    }
}

/// Per-pair normalization used by both the set-level matcher and model-name
/// resolution in `control.rs`: compares names ignoring the `:tag` suffix and
/// case (so `llama3` matches `llama3:latest`).
pub fn smart_model_match_one(requested: &str, model: &str) -> bool {
    let requested_low = requested.to_lowercase();
    let requested_no_tag = requested_low.split(':').next().unwrap_or(&requested_low);
    let model_low = model.to_lowercase();
    let model_no_tag = model_low.split(':').next().unwrap_or(&model_low);
    requested_no_tag == model_no_tag
}

/// Bounded normalization for model-name comparison: lowercase, drop the
/// `:tag` suffix and any LM Studio-style `@quant` suffix.
fn normalize_model_id(id: &str) -> String {
    let low = id.to_lowercase();
    let no_tag = low.split(':').next().unwrap_or(&low);
    no_tag.split('@').next().unwrap_or(no_tag).to_string()
}

/// Publisher-agnostic part of a normalized id (`owner/model` -> `model`).
fn model_base(id: &str) -> String {
    let n = normalize_model_id(id);
    match n.rsplit_once('/') {
        Some((_, m)) => m.to_string(),
        None => n,
    }
}

/// True when `requested` matches `listed` under the bounded rules shared by
/// `model_routable`: tag/case-insensitive equality (`smart_model_match_one`),
/// or equality after full normalization (lowercase, strip `:tag` and `@quant`),
/// or — for bare requested names only — publisher-stripped base equality.
fn name_matches_listed(requested: &str, listed: &str) -> bool {
    if smart_model_match_one(requested, listed)
        || normalize_model_id(listed) == normalize_model_id(requested)
    {
        return true;
    }
    if !requested.contains('/') {
        return model_base(requested) == model_base(listed);
    }
    false
}

/// Scheduler-side routability check. Deterministic and bounded — no arbitrary
/// substring matching (which could route `qwen3.8-27b` to an unrelated
/// `huihui-qwen3.8-27b-abliterated`). A backend is eligible when any listed id
/// matches via `name_matches_listed`:
/// - equals the request after tag/case normalization (`smart_model_match_one`), or
/// - equals it after full normalization (handles `@quant` suffixes on either side), or,
/// - for bare requested names only — equals its publisher-stripped base, so a
///   client asking `llama3` can reach `meta-llama/llama3:latest`.
///
/// Owner-prefixed requests are strict: the backend must list that exact id up to
/// tag/quant normalization.
fn model_routable(requested: &str, available: &HashSet<String>) -> bool {
    available.iter().any(|m| name_matches_listed(requested, m))
}

/// True when the requested model is resident on this backend per its latest
/// probe: either in `loaded_models` (Ollama `/api/ps`, LM Studio loaded
/// instance keys/ids) or — for LM Studio — a native-list entry with loaded
/// instances whose key or display name matches the request.
pub fn model_loaded_on(b: &BackendStatus, requested: &str) -> bool {
    if model_routable(requested, &b.loaded_models) {
        return true;
    }
    if b.lmstudio {
        return b.native_models.iter().any(|m| {
            !m.loaded_instance_ids.is_empty()
                && (name_matches_listed(requested, &m.key)
                    || m.display_name
                        .as_deref()
                        .map(|d| name_matches_listed(requested, d))
                        .unwrap_or(false))
        });
    }
    false
}

/// Per-model concurrency limits keyed by normalized model name, from the
/// `models:` section of appconf.yaml. Unlisted models default to 1 at lookup.
pub fn build_model_limits(configs: &[crate::config::ModelConfig]) -> HashMap<String, u32> {
    configs
        .iter()
        .map(|c| (normalize_model_id(&c.name), c.max_concurrent_requests))
        .collect()
}

/// True when the backend has room for one more in-flight request: under the
/// global per-backend cap, and — for model requests — under that model's limit.
fn backend_has_capacity(
    b: &BackendStatus,
    model_key: Option<&str>,
    model_limit: u32,
    cap: u32,
) -> bool {
    if b.active_requests >= cap as usize {
        return false;
    }
    match model_key {
        Some(k) => *b.active_by_model.get(k).unwrap_or(&0) < model_limit,
        None => true,
    }
}

/// True when the backend can speak the request's API dialect. Unknown-type
/// backends are allowed until the health check classifies them; families this
/// backend has been observed rejecting in real traffic (see
/// `apply_family_learning`) are excluded until a request of that family
/// succeeds or the backend restarts.
fn family_compatible(b: &BackendStatus, family: ApiFamily) -> bool {
    if b.rejected_families.contains(&family) {
        return false;
    }
    matches!(b.api_type, BackendApiType::Unknown | BackendApiType::Both)
        || b.api_type.supports(family)
}

/// Heuristic: did this response reject the request's API *dialect* (as opposed
/// to a normal application error like model-not-found)? Conservative on purpose
/// — only structural endpoint/method rejections count, so ordinary errors never
/// poison the scheduler.
fn is_endpoint_rejection(status: StatusCode, body_prefix: &str) -> bool {
    // 405 Method Not Allowed is always structural.
    if status.as_u16() == 405 {
        return true;
    }
    let text = body_prefix.trim();
    if !text.starts_with('{') {
        return false;
    }
    // Prefer a full JSON parse (error bodies are small); fall back to a raw
    // substring scan when the prefix is truncated mid-JSON.
    let error_text = match serde_json::from_str::<serde_json::Value>(text).ok() {
        Some(v) => v
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_lowercase)
            .unwrap_or_default(),
        None => text.to_lowercase(),
    };
    if error_text.is_empty() {
        return false;
    }
    error_text.contains("endpoint") || error_text.contains("method")
}

/// Record a traffic observation for one backend + API family. Two consecutive
/// endpoint rejections mark the family as rejected (the scheduler then excludes
/// this backend for that family); any non-rejection response clears the memory.
fn apply_family_learning(
    state: &AppState,
    backend_idx: usize,
    family: ApiFamily,
    rejected: bool,
) {
    if family == ApiFamily::Unknown {
        return; // nothing to learn from unrecognized paths
    }
    let mut backends = state.backends.lock().unwrap();
    let Some(b) = backends.get_mut(backend_idx) else {
        return;
    };
    if rejected {
        let count = b.family_fail_counts.entry(family).or_insert(0);
        *count += 1;
        if *count >= 2 && !b.rejected_families.contains(&family) {
            b.rejected_families.insert(family);
            info!(
                "Backend {} learned: rejecting {}-family requests ({} consecutive endpoint errors)",
                b.url,
                family.display(),
                count
            );
        }
    } else if b.family_fail_counts.remove(&family).is_some()
        || b.rejected_families.remove(&family)
    {
        debug!(
            "Backend {} serves {}-family requests again; cleared rejection memory",
            b.url,
            family.display()
        );
    }
}

pub async fn run_worker(state: Arc<AppState>) {
    let client = state.client.clone();
    let mut current_idx = 0;

    // Background Health Check
    let health_state = state.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        let mut cycle: u32 = 0;
        loop {
            // Every ~1 min do a full re-probe of all endpoints so remembered
            // "bad" ones are re-verified (the backend may have been upgraded).
            let full_reprobe = cycle.is_multiple_of(6);
            cycle += 1;

            let backends_to_check: Vec<(usize, String, bool)> = {
                let backends = health_state.backends.lock().unwrap();
                backends
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i, b.url.clone(), b.is_online))
                    .collect()
            };

            for (idx, url, was_online) in backends_to_check {
                // Skip endpoints this backend is known to reject — except on a
                // full re-probe or right after recovery from offline (memory
                // may be stale after a restart). Never skip both primary API
                // probes: online status must stay re-establishable.
                let skip = {
                    let backends = health_state.backends.lock().unwrap();
                    if full_reprobe || !was_online {
                        HashSet::new()
                    } else {
                        let mut s = backends[idx].known_bad_endpoints.clone();
                        if s.contains("/api/tags") && s.contains("/v1/models") {
                            s.clear();
                        }
                        s
                    }
                };

                let probe = crate::control::probe_backend(&health_client, &url, &skip).await;

                let mut backends = health_state.backends.lock().unwrap();
                let b = &mut backends[idx];

                // A backend that just recovered from offline may have been
                // restarted: clear learned endpoint/family memory so it is
                // re-learned from scratch.
                if !was_online && probe.is_online {
                    b.known_bad_endpoints.clear();
                    b.rejected_families.clear();
                    b.family_fail_counts.clear();
                }

                let newly_bad: Vec<String> = probe
                    .bad_endpoints
                    .iter()
                    .filter(|e| !b.known_bad_endpoints.contains(*e))
                    .cloned()
                    .collect();

                let changed = b.is_online != probe.is_online
                    || b.api_type != probe.api_type
                    || b.available_models != probe.available_models
                    || b.loaded_models != probe.loaded_models;

                let was_api_type = b.api_type;
                let now_online = probe.is_online;
                crate::control::apply_probe(b, probe);

                if !was_online && now_online {
                    info!("Backend {} status changed to: ONLINE", url);
                } else if was_online && !now_online {
                    info!("Backend {} status changed to: OFFLINE", url);
                }
                if b.api_type != was_api_type {
                    info!(
                        "Backend {} API type detected: {}",
                        url,
                        b.api_type.display()
                    );
                }
                for e in &newly_bad {
                    info!(
                        "Backend {} does not serve {} — remembered, will skip it until re-check",
                        url, e
                    );
                }

                // Wake the dispatcher if anything relevant to scheduling
                // changed (model finished loading, backend came online, etc.)
                // so queued requests are re-evaluated immediately.
                if changed {
                    drop(backends);
                    health_state.notify.notify_one();
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    loop {
        let selection_opt = {
            let mut queues = state.queues.lock().unwrap();
            // Backends with a model control op (load/unload) in flight are treated as
            // busy: loading a new model can evict whatever is currently running.
            let control_busy: HashSet<usize> =
                state.control_ops.lock().unwrap().keys().copied().collect();
            let mut backends = state.backends.lock().unwrap();
            let mut last_idx = state.last_backend_idx.lock().unwrap();

            // 1. Pick a user and peek at their front task to know required API family
            let vip = state.vip_user.lock().unwrap().clone();
            let boost = state.boost_user.lock().unwrap().clone();
            let mut counter = state.global_counter.lock().unwrap();

            let mut active_users: Vec<String> = queues
                .keys()
                .filter(|u| !queues.get(*u).unwrap().is_empty())
                .cloned()
                .collect();

            if active_users.is_empty() {
                None
            } else {
                active_users.sort_by(|a, b| {
                    let a_total = state
                        .processed_counts
                        .lock()
                        .unwrap()
                        .get(a)
                        .cloned()
                        .unwrap_or(0);
                    let b_total = state
                        .processed_counts
                        .lock()
                        .unwrap()
                        .get(b)
                        .cloned()
                        .unwrap_or(0);
                    a_total.cmp(&b_total).then_with(|| a.cmp(b))
                });

                // Build candidate order: VIP first, then boost on every other
                // turn, then fair-share round-robin over the rest.
                let mut candidates: Vec<String> = Vec::new();
                if let Some(ref v) = vip {
                    if active_users.contains(v) {
                        candidates.push(v.clone());
                    }
                }
                if *counter % 2 == 0 {
                    if let Some(ref b) = boost {
                        if active_users.contains(b) && !candidates.contains(b) {
                            candidates.push(b.clone());
                        }
                    }
                }
                if current_idx >= active_users.len() {
                    current_idx = 0;
                }
                let rest: Vec<String> = active_users
                    .iter()
                    .filter(|u| !candidates.contains(u))
                    .cloned()
                    .collect();
                current_idx += 1;
                if !rest.is_empty() {
                    let start = current_idx % rest.len();
                    candidates.extend(rest[start..].iter().cloned());
                    candidates.extend(rest[..start].iter().cloned());
                }

                // Try users in candidate order. Within a user's queue, pick
                // the FIRST routable task (not just the front) so an
                // unroutable request — e.g. an Ollama-family call with every
                // Ollama backend offline — can't starve everything behind it.
                let mut selection: Option<(String, Task, usize, String)> = None;
                'users: for user_id in &candidates {
                    let queue_len = queues.get(user_id).map(|q| q.len()).unwrap_or(0);
                    for pos in 0..queue_len {
                        let task_ref = match queues.get(user_id).and_then(|q| q.get(pos)) {
                            Some(t) => t,
                            None => continue,
                        };
                        let api_family = detect_api_family(&task_ref.path);
                        debug!(
                            "Request for user {}: pos={} path={} family={:?}",
                            user_id, pos, task_ref.path, api_family
                        );

                        // Find eligible backends: online, has capacity (global per-backend
                        // cap + per-model limit), and support the required API + Model.
                        let limits = state.model_limits.lock().unwrap();
                        let model_key = task_ref.requested_model.as_deref().map(normalize_model_id);
                        let model_limit = match &model_key {
                            Some(k) => limits.get(k).copied().unwrap_or(1),
                            None => 0, // unused for model-less requests
                        };
                        drop(limits);
                        let eligible_indices: Vec<usize> = backends.iter()
                            .enumerate()
                            .filter(|(i, b)| {
                                let online = b.is_online;
                                let free = backend_has_capacity(
                                    b,
                                    model_key.as_deref(),
                                    model_limit,
                                    state.max_concurrent_per_backend,
                                );
                                let no_op = !control_busy.contains(i);
                                if !online || !free || !no_op {
                                    debug!(
                                        "Backend {} rejected: online={}, active={}/{}, control_op={}",
                                        b.url,
                                        online,
                                        b.active_requests,
                                        state.max_concurrent_per_backend,
                                        !no_op
                                    );
                                }
                                online && free && no_op
                            })
                            .filter(|(_, b)| {
                                // The backend must speak the request's API dialect — ALWAYS,
                                // even when a specific model is requested. (Previously this was
                                // only checked in the no-model case, which let Ollama-family
                                // requests reach LM Studio backends that merely listed a
                                // matching model.) Unknown-type backends are allowed until the
                                // health check classifies them; families observed rejecting
                                // real traffic are excluded (see apply_family_learning).
                                if !family_compatible(b, api_family) {
                                    debug!(
                                        "Backend {} rejected: api_family {:?} not supported by {:?}",
                                        b.url, api_family, b.api_type
                                    );
                                    return false;
                                }

                                // If a specific model is requested, backend MUST also have it.
                                match &task_ref.requested_model {
                                    Some(model) => {
                                        let has_model = model_routable(model, &b.available_models);
                                        if !has_model {
                                            debug!(
                                                "Backend {} rejected: model '{}' not found. Available: {:?}",
                                                b.url, model, b.available_models
                                            );
                                        }
                                        has_model
                                    }
                                    None => true,
                                }
                            })
                            .map(|(i, _)| i)
                            .collect();

                        // Model-load-aware selection among eligible backends (see model_loaded_on):
                        //   tier 1 — the requested model is confirmed loaded on the backend, or the
                        //            backend doesn't report loaded state at all (e.g. vLLM: "available"
                        //            means "ready to serve");
                        //   tier 2 — the backend reports loaded state but has nothing loaded (cold
                        //            start; loading the requested model evicts nothing);
                        //   tier 3 — last resort: the backend has a DIFFERENT model loaded; on-demand
                        //            loading would evict it (slow, and may fail).
                        let eligible_indices = if let Some(ref model) = task_ref.requested_model {
                            let tier1: Vec<usize> = eligible_indices
                                .iter()
                                .cloned()
                                .filter(|&i| {
                                    let b = &backends[i];
                                    model_loaded_on(b, model) || !b.loaded_state_known
                                })
                                .collect();
                            if !tier1.is_empty() {
                                if tier1.len() < eligible_indices.len() {
                                    debug!(
                                        "model '{}' ready on {} of {} eligible backend(s) — routing to loaded backend(s)",
                                        model,
                                        tier1.len(),
                                        eligible_indices.len()
                                    );
                                }
                                tier1
                            } else {
                                let tier2: Vec<usize> = eligible_indices
                                    .iter()
                                    .cloned()
                                    .filter(|&i| {
                                        let b = &backends[i];
                                        b.loaded_state_known && b.loaded_models.is_empty()
                                    })
                                    .collect();
                                if !tier2.is_empty() {
                                    debug!(
                                        "no backend has model '{}' loaded; using cold backend(s) (nothing loaded to evict)",
                                        model
                                    );
                                    tier2
                                } else {
                                    warn!(
                                        "no backend has model '{}' loaded and every eligible backend has a different model loaded; falling back to on-demand load (may evict a loaded model)",
                                        model
                                    );
                                    eligible_indices
                                }
                            }
                        } else {
                            eligible_indices
                        };

                        if eligible_indices.is_empty() {
                            // Can this request EVER be served in the current known state? If
                            // some backend is family-compatible and (has the model / needs no
                            // model), it may just be busy or loading — keep waiting. Otherwise
                            // nothing will ever pick it up: warn once, then fail fast with 503
                            // after stuck_timeout instead of letting the client hang forever.
                            let satisfiable = backends.iter().any(|b| {
                                family_compatible(b, api_family)
                                    && match &task_ref.requested_model {
                                        Some(model) => model_routable(model, &b.available_models),
                                        None => true,
                                    }
                            });

                            if !satisfiable
                                && task_ref.queued_at.elapsed() >= state.stuck_timeout
                            {
                                let dropped = queues
                                    .get_mut(user_id)
                                    .unwrap()
                                    .remove(pos)
                                    .unwrap();
                                *counter += 1;
                                let waited = dropped.queued_at.elapsed().as_secs();
                                let mut dropped_counts = state.dropped_counts.lock().unwrap();
                                *dropped_counts.entry(user_id.clone()).or_insert(0) += 1;
                                drop(dropped_counts);
                                state.log_event(LogEvent {
                                    at: std::time::SystemTime::now(),
                                    dir: "OUT",
                                    user: user_id.clone(),
                                    model: dropped.requested_model.clone(),
                                    backend: None,
                                    info: format!(
                                        "503 no backend can serve (waited {}s)",
                                        waited
                                    ),
                                    content: None,
                                });
                                let responder = dropped.responder;
                                tokio::spawn(async move {
                                    let body = serde_json::json!({
                                        "error": "no backend available to serve this request"
                                    })
                                    .to_string();
                                    let mut headers = HeaderMap::new();
                                    headers.insert(
                                        axum::http::header::CONTENT_TYPE,
                                        axum::http::HeaderValue::from_static("application/json"),
                                    );
                                    let _ = responder
                                        .send(ResponsePart::Status(
                                            StatusCode::SERVICE_UNAVAILABLE,
                                            headers,
                                        ))
                                        .await;
                                    let _ = responder
                                        .send(ResponsePart::Chunk(Bytes::from(body)))
                                        .await;
                                });
                                continue 'users;
                            }

                            if !task_ref.stuck_warned {
                                let msg_prefix = if satisfiable {
                                    "No backend free".to_string()
                                } else {
                                    format!(
                                        "No backend can serve; will fail with 503 after {}s",
                                        state.stuck_timeout.as_secs()
                                    )
                                };
                                warn!(
                                    "{} for {} {} (model: {}, family: {:?}) for user {}",
                                    msg_prefix,
                                    task_ref.method,
                                    task_ref.path,
                                    task_ref.requested_model.as_deref().unwrap_or("-"),
                                    api_family,
                                    user_id
                                );
                                if let Some(task_mut) =
                                    queues.get_mut(user_id).and_then(|q| q.get_mut(pos))
                                {
                                    task_mut.stuck_warned = true;
                                }
                            }
                            continue;
                        }

                        let task = queues.get_mut(user_id).unwrap().remove(pos).unwrap();
                        *counter += 1;

                        // Round-Robin among eligible backends with min connections
                        let min_conns = eligible_indices
                            .iter()
                            .map(|&i| backends[i].active_requests)
                            .min()
                            .unwrap();
                        let candidates_backends: Vec<usize> = eligible_indices
                            .iter()
                            .cloned()
                            .filter(|&i| backends[i].active_requests == min_conns)
                            .collect();
                        let candidate_pos = candidates_backends
                            .iter()
                            .position(|&i| i > *last_idx)
                            .unwrap_or(0);
                        let selected_backend_idx = candidates_backends[candidate_pos];

                        *last_idx = selected_backend_idx;
                        backends[selected_backend_idx].active_requests += 1;
                        if let Some(ref m) = task.requested_model {
                            let k = normalize_model_id(m);
                            *backends[selected_backend_idx]
                                .active_by_model
                                .entry(k)
                                .or_insert(0) += 1;
                        }
                        backends[selected_backend_idx].current_model = task.requested_model.clone();

                        selection = Some((
                            user_id.clone(),
                            task,
                            selected_backend_idx,
                            backends[selected_backend_idx].url.clone(),
                        ));
                        break 'users;
                    }
                }

                selection
            }
        };

        match selection_opt {
            Some((user_id, task, backend_idx, backend_url)) => {
                let state_clone = state.clone();
                let client_clone = client.clone();
                let url = format!("{}{}", backend_url, task.path);

                tokio::spawn(async move {
                    let log_model = task.requested_model.clone();
                    let api_family = detect_api_family(&task.path);
                    let log_out = |info: String, backend: Option<String>, content: Option<String>| {
                        state_clone.log_event(LogEvent {
                            at: std::time::SystemTime::now(),
                            dir: "OUT",
                            user: user_id.clone(),
                            model: log_model.clone(),
                            backend,
                            info,
                            content,
                        });
                    };

                    let is_blocked = {
                        let user_ips = state_clone.user_ips.lock().unwrap();
                        let blocked_ips = state_clone.blocked_ips.lock().unwrap();
                        let blocked_users = state_clone.blocked_users.lock().unwrap();
                        blocked_users.contains(&user_id)
                            || user_ips
                                .get(&user_id)
                                .map(|ip| blocked_ips.contains(ip))
                                .unwrap_or(false)
                    };

                    if is_blocked || task.responder.is_closed() {
                        let mut dropped = state_clone.dropped_counts.lock().unwrap();
                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                        log_out(
                            "dropped (blocked)".into(),
                            Some(backend_url.clone()),
                            None,
                        );
                    } else {
                        {
                            let mut processing = state_clone.processing_counts.lock().unwrap();
                            *processing.entry(user_id.clone()).or_insert(0) += 1;
                        }

                        // Response size + bounded content prefix for the
                        // request log (appends stop at log_content_limit, but
                        // total_bytes keeps counting).
                        let mut total_bytes: u64 = 0;
                        let mut content_acc: Vec<u8> = Vec::new();
                        // `task.method`/`task.path` are moved into the request
                        // builder below; capture strings for log records.
                        let method_str = task.method.to_string();
                        let path_str = task.path.clone();

                        let res_fut = client_clone
                            .request(task.method, &url)
                            .headers(task.headers)
                            .body(task.body)
                            .send();

                        match res_fut.await {
                            Ok(response) => {
                                let status = response.status();
                                let mut headers = response.headers().clone();
                                headers.remove(axum::http::header::TRANSFER_ENCODING);
                                headers.remove(axum::http::header::CONTENT_LENGTH);
                                let resp_content_type = headers
                                    .get(axum::http::header::CONTENT_TYPE)
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_string);

                                if task
                                    .responder
                                    .send(ResponsePart::Status(status, headers))
                                    .await
                                    .is_ok()
                                {
                                    let mut stream = response.bytes_stream();
                                    let mut client_disconnected = false;
                                    // Keep a short prefix of the first body chunk so we can
                                    // tell "endpoint rejected" error bodies apart from normal
                                    // application errors (learning hook below).
                                    let mut first_prefix: Option<String> = None;
                                    while let Some(chunk_res) = stream.next().await {
                                        match chunk_res {
                                            Ok(chunk) => {
                                                if first_prefix.is_none() {
                                                    let n = chunk.len().min(512);
                                                    first_prefix = Some(
                                                        String::from_utf8_lossy(&chunk[..n])
                                                            .into_owned(),
                                                    );
                                                }
                                                let n = chunk.len();
                                                total_bytes += n as u64;
                                                if content_acc.len() < state_clone.log_content_limit {
                                                    let room =
                                                        state_clone.log_content_limit
                                                            - content_acc.len();
                                                    content_acc.extend_from_slice(
                                                        &chunk[..n.min(room)],
                                                    );
                                                }
                                                if task
                                                    .responder
                                                    .send(ResponsePart::Chunk(chunk))
                                                    .await
                                                    .is_err()
                                                {
                                                    client_disconnected = true;
                                                    break;
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }

                                    // Learn from the response: endpoint rejections (405 /
                                    // "Unexpected endpoint or method") mark this backend as
                                    // incompatible with the request's API family after two
                                    // strikes; any other response clears that memory.
                                    let prefix = first_prefix.unwrap_or_default();
                                    apply_family_learning(
                                        &state_clone,
                                        backend_idx,
                                        api_family,
                                        is_endpoint_rejection(status, &prefix),
                                    );

                                    if !client_disconnected {
                                        let mut counts =
                                            state_clone.processed_counts.lock().unwrap();
                                        *counts.entry(user_id.clone()).or_insert(0) += 1;
                                        drop(counts);
                                        let content_str = crate::reqlog::truncate_utf8(
                                            &content_acc,
                                            state_clone.log_content_limit,
                                        );
                                        state_clone.reqlog.log(crate::reqlog::ReqRecord {
                                            ts: crate::reqlog::now_unix_millis(),
                                            dir: "OUT",
                                            user: task.user.clone(),
                                            model: log_model.clone(),
                                            backend: Some(backend_url.clone()),
                                            method: method_str.clone(),
                                            path: path_str.clone(),
                                            status: Some(status.as_u16()),
                                            bytes: Some(total_bytes),
                                            content_type: resp_content_type,
                                            content: Some(content_str.clone()),
                                        });
                                        log_out(
                                            format!(
                                                "{} {} -> {} resp={}B",
                                                method_str, path_str, status.as_u16(), total_bytes
                                            ),
                                            Some(backend_url.clone()),
                                            Some(content_str),
                                        );
                                    } else {
                                        let mut dropped =
                                            state_clone.dropped_counts.lock().unwrap();
                                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                        log_out(
                                            "dropped (client gone)".into(),
                                            Some(backend_url.clone()),
                                            None,
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                let err_msg = format!("upstream error: {}", e);
                                state_clone.reqlog.log(crate::reqlog::ReqRecord {
                                    ts: crate::reqlog::now_unix_millis(),
                                    dir: "OUT",
                                    user: task.user.clone(),
                                    model: log_model.clone(),
                                    backend: Some(backend_url.clone()),
                                    method: method_str,
                                    path: path_str,
                                    status: None,
                                    bytes: Some(total_bytes),
                                    content_type: None,
                                    content: Some(err_msg.clone()),
                                });
                                let _ = task.responder.send(ResponsePart::Error(e)).await;
                                let mut dropped = state_clone.dropped_counts.lock().unwrap();
                                *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                log_out(
                                    "dropped (backend error)".into(),
                                    Some(backend_url.clone()),
                                    Some(err_msg),
                                );
                            }
                        }

                        {
                            let mut processing = state_clone.processing_counts.lock().unwrap();
                            if let Some(count) = processing.get_mut(&user_id) {
                                *count = count.saturating_sub(1);
                            }
                        }
                    }

                    {
                        let mut backends = state_clone.backends.lock().unwrap();
                        let b = &mut backends[backend_idx];
                        b.active_requests = b.active_requests.saturating_sub(1);
                        if let Some(ref m) = task.requested_model {
                            let k = normalize_model_id(m);
                            if let Some(c) = b.active_by_model.get_mut(&k) {
                                *c = c.saturating_sub(1);
                                if *c == 0 {
                                    b.active_by_model.remove(&k);
                                }
                            }
                        }
                        b.processed_count += 1;
                    }
                    state_clone.backend_freed.notify_one();
                });
            }
            None => {
                // Tick at least once per second so stuck-timeout drops are
                // enforced even when no new events arrive.
                tokio::select! {
                    _ = state.notify.notified() => {},
                    _ = state.backend_freed.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                }
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    method: Method,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path().to_string();
    let ip = addr.ip();
    let user_id = headers
        .get("X-User-ID")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();

    if state.is_ip_blocked(&ip) {
        warn!("Blocked request from IP: {} for user: {}", ip, user_id);
        return (StatusCode::FORBIDDEN, "IP blocked").into_response();
    }

    if state.is_user_blocked(&user_id) {
        warn!("Blocked request from user: {} (IP: {})", user_id, ip);
        return (StatusCode::FORBIDDEN, "User blocked").into_response();
    }

    {
        let mut ips = state.user_ips.lock().unwrap();
        ips.insert(user_id.clone(), ip);
    }

    let (tx, rx) = mpsc::channel(32);
    let mut task_headers = headers.clone();
    task_headers.remove(axum::http::header::HOST);

    let requested_model = if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
        json.get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    let task_method = method.as_str().to_string();
    // Request-log content is captured before `body` moves into the Task.
    let body_len = body.len() as u64;
    let content = crate::reqlog::truncate_utf8(&body, state.log_content_limit);
    let task = Task {
        path: path.clone(),
        method,
        user: addr.to_string(),
        headers: task_headers,
        responder: tx,
        body,
        requested_model: requested_model.clone(),
        stuck_warned: false,
        queued_at: std::time::Instant::now(),
    };

    {
        let mut queues = state.queues.lock().unwrap();
        queues
            .entry(user_id.clone())
            .or_insert_with(VecDeque::new)
            .push_back(task);
    }

    state.reqlog.log(crate::reqlog::ReqRecord {
        ts: crate::reqlog::now_unix_millis(),
        dir: "IN",
        user: addr.to_string(),
        model: requested_model.clone(),
        backend: None,
        method: task_method.clone(),
        path: path.clone(),
        status: None,
        bytes: Some(body_len),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        content: Some(content.clone()),
    });

    state.log_event(crate::dispatcher::LogEvent {
        at: std::time::SystemTime::now(),
        dir: "IN",
        user: user_id.clone(),
        model: requested_model.clone(),
        backend: None,
        info: format!("{} {} body={}B", task_method, path, body_len),
        content: Some(content),
    });

    state.notify.notify_one();

    let mut rx = rx;
    match rx.recv().await {
        Some(ResponsePart::Status(status, headers)) => {
            let stream = ReceiverStream::new(rx).map(|part| match part {
                ResponsePart::Chunk(chunk) => Ok(chunk),
                ResponsePart::Error(e) => Err(e),
                _ => Ok(Bytes::new()),
            });

            let mut res = Body::from_stream(stream).into_response();
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            res
        }
        Some(ResponsePart::Error(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Backend error: {}", e),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Worker failed to respond",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pair_matching_handles_tags_and_case() {
        // Case-insensitive, tag-stripped — but publisher-strict at pair level;
        // cross-publisher matching happens in model_routable via base compare.
        assert!(smart_model_match_one("qwen3.8-27b", "QWEN3.8-27B"));
        assert!(smart_model_match_one("llama3:latest", "llama3"));
        assert!(!smart_model_match_one("mistral", "llama3:latest"));
    }

    #[test]
    fn normalized_matching_strips_owner_and_quant() {
        // Client asks "qwen3.8-27b"; server lists publisher/quant variants of it.
        assert!(model_routable("qwen3.8-27b", &set(&["unsloth/qwen3.8-27b@q8_0"])));
        assert!(model_routable("qwen3.8-27b", &set(&["qwen/qwen3.8-27b"])));
        // Quant suffixes on either side are normalized away (same model family).
        assert!(model_routable(
            "unsloth/qwen3.8-27b@q8_0",
            &set(&["unsloth/qwen3.8-27b@q4_k_m"])
        ));
    }

    #[test]
    fn matching_rejects_substring_lookalikes() {
        // No arbitrary substring matching: these share name fragments but are
        // different models and must NOT be routable for "qwen3.8-27b".
        let lookalikes = set(&[
            "huihui-qwen3.8-27b-abliterated",
            "ovisocr2@q8_0",
            "llama3:latest",
            "mistral-7b",
            "text-embedding-nomic",
        ]);
        assert!(!model_routable("qwen3.8-27b", &lookalikes));
    }

    #[test]
    fn owner_prefixed_requests_are_strict() {
        // A request that names a publisher must reach a backend listing that
        // exact id (up to tag/quant normalization), not just the same base name.
        let avail = set(&["qwen/qwen3.8-27b", "unsloth/qwen3.8-27b@q8_0"]);
        assert!(model_routable("unsloth/qwen3.8-27b@q8_0", &avail));
        assert!(!model_routable(
            "unsloth/qwen3.8-27b@q4_k_m",
            &set(&["qwen/qwen3.8-27b"])
        ));
    }

    #[test]
    fn bare_names_match_across_publishers() {
        // "llama3" reaches meta-llama/llama3:latest (publisher stripped), but a
        // different base name does not match on prefix alone.
        assert!(model_routable("llama3", &set(&["meta-llama/llama3:latest"])));
        assert!(!model_routable("llama3", &set(&["meta-llama/llama3.1:latest"])));
    }

    fn backend_with(api_type: BackendApiType, rejected: &[ApiFamily]) -> BackendStatus {
        BackendStatus {
            url: "http://test".into(),
            active_requests: 0,
            processed_count: 0,
            is_online: true,
            api_type,
            available_models: HashSet::new(),
            loaded_models: HashSet::new(),
            loaded_state_known: false,
            loaded_ctx: HashMap::new(),
            current_model: None,
            active_by_model: HashMap::new(),
            lmstudio: false,
            native_models: Vec::new(),
            known_bad_endpoints: HashSet::new(),
            rejected_families: rejected.iter().copied().collect(),
            family_fail_counts: HashMap::new(),
        }
    }

    #[test]
    fn model_loaded_on_matches_ollama_ps() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        b.loaded_state_known = true;
        b.loaded_models = set(&["qwen3.8-27b:latest"]);
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "other-model"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_key_with_quant() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = set(&["unsloth/qwen3.8-27b@q8_0"]);
        // Bare request name reaches the publisher/quant-prefixed loaded key.
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_display_name() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.native_models.push(LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: vec!["unsloth/qwen3.8-27b@q8_0".into()],
        });
        // The display name matches directly (no `:tag`/`@quant` on either side).
        assert!(model_loaded_on(&b, "Qwen3.8 27B"));
        // The key matches via the shared bounded rules (bare-name base compare).
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn model_loaded_on_ignores_unloaded_native_entries() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.native_models.push(LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: Vec::new(),
        });
        // Listed but with no loaded instances: not resident.
        assert!(!model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "Qwen3.8 27B"));
    }

    #[test]
    fn model_loaded_on_no_match() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = set(&["ovisocr2@q4_k_m"]);
        assert!(!model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn endpoint_rejection_heuristic() {
        // 405 is always structural, regardless of the body.
        assert!(is_endpoint_rejection(StatusCode::METHOD_NOT_ALLOWED, ""));
        assert!(is_endpoint_rejection(
            StatusCode::METHOD_NOT_ALLOWED,
            "whatever"
        ));
        // LM Studio's signature: 200 + error JSON mentioning endpoint/method.
        let lm = r#"{"error":"Unexpected endpoint or method. (GET /api/tags)"}"#;
        assert!(is_endpoint_rejection(StatusCode::OK, lm));
        // Normal application errors are not dialect rejections.
        assert!(!is_endpoint_rejection(
            StatusCode::NOT_FOUND,
            r#"{"error":"model 'x' not found"}"#
        ));
        assert!(!is_endpoint_rejection(
            StatusCode::INTERNAL_SERVER_ERROR,
            "boom"
        ));
        assert!(!is_endpoint_rejection(StatusCode::OK, r#"{"message":"ok"}"#));
    }

    #[test]
    fn family_compatibility_rules() {
        // Unknown type is allowed until the health check classifies it.
        assert!(family_compatible(
            &backend_with(BackendApiType::Unknown, &[]),
            ApiFamily::Ollama
        ));
        // Ollama-only backend cannot serve OpenAI-family requests (and vice versa).
        assert!(!family_compatible(
            &backend_with(BackendApiType::Ollama, &[]),
            ApiFamily::OpenAi
        ));
        assert!(family_compatible(
            &backend_with(BackendApiType::Ollama, &[]),
            ApiFamily::Ollama
        ));
        // Both serves everything.
        assert!(family_compatible(
            &backend_with(BackendApiType::Both, &[]),
            ApiFamily::OpenAi
        ));
        // Learned rejections override the detected type.
        let b = backend_with(BackendApiType::Both, &[ApiFamily::Ollama]);
        assert!(!family_compatible(&b, ApiFamily::Ollama));
        assert!(family_compatible(&b, ApiFamily::OpenAi));
    }

    #[test]
    fn model_limits_are_normalized_and_lookup_defaults() {
        let configs = vec![crate::config::ModelConfig {
            name: "gpt-oss:120b".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 3,
            backends: Vec::new(),
        }];
        let limits = build_model_limits(&configs);
        // ':tag' is stripped when keying.
        assert_eq!(limits.get("gpt-oss"), Some(&3));
        // Lookup happens on the normalized (lowercased) key; unlisted models
        // fall back to 1 at lookup time in the scheduler.
        assert_eq!(limits.get("GPT-OSS"), None);
        assert_eq!(limits.get("qwen2.5-coder"), None);
    }

    #[test]
    fn backend_capacity_enforces_global_and_per_model() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        let cap: u32 = 3;
        assert!(backend_has_capacity(&b, Some("a"), 2, cap));

        // Two in-flight for "a" (at its limit) and one for "b": global cap hit.
        b.active_requests = 3;
        b.active_by_model.insert("a".into(), 2);
        b.active_by_model.insert("b".into(), 1);
        assert!(!backend_has_capacity(&b, Some("c"), 5, cap)); // cap reached

        // One slot frees up: "a" is at its per-model limit, others still fit.
        b.active_requests = 2;
        assert!(!backend_has_capacity(&b, Some("a"), 2, cap));
        assert!(backend_has_capacity(&b, Some("b"), 3, cap));
        // Model-less requests are bounded by the global cap only.
        assert!(backend_has_capacity(&b, None, 0, cap));
    }

    #[test]
    fn backend_capacity_defaults_to_single_slot() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        // Historical behavior: one in-flight request per backend.
        assert!(backend_has_capacity(&b, Some("m"), 1, 1));
        b.active_requests = 1;
        assert!(!backend_has_capacity(&b, Some("m"), 5, 1)); // global cap binds first
    }

    #[tokio::test]
    async fn stuck_request_fails_fast_with_503() {
        // Backend that can never serve the requested model (nothing listens).
        let state = Arc::new(AppState::new(
            vec!["http://127.0.0.1:9".to_string()],
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            1,     // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
        ));

        let (tx, mut rx) = mpsc::channel(32);
        {
            let mut queues = state.queues.lock().unwrap();
            queues.entry("tester".to_string()).or_default().push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/api/chat".into(),
                headers: HeaderMap::new(),
                body: Bytes::from(r#"{"model":"no-such-model-xyz","messages":[]}"#),
                responder: tx,
                requested_model: Some("no-such-model-xyz".to_string()),
                stuck_warned: false,
                queued_at: std::time::Instant::now(),
            });
        }

        // Enqueued before the worker starts: its first scan pass sees it.
        let worker = tokio::spawn(run_worker(state.clone()));

        let part = tokio::time::timeout(std::time::Duration::from_secs(6), rx.recv())
            .await
            .expect("no response within 6s")
            .expect("responder closed");
        match part {
            ResponsePart::Status(status, _) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE)
            }
            _ => panic!("expected Status(503), got another part"),
        }

        worker.abort();
    }

    // -- Scheduler-tier integration tests ------------------------------------
    //
    // Each test stands up tiny local HTTP mocks (real TCP listeners on
    // 127.0.0.1:0) that mimic what a backend reports to the health probes,
    // and pre-sets state.backends to exactly the state those probe answers
    // produce BEFORE spawning the worker: the first probe cycle runs
    // immediately and overwrites backend state via apply_probe, so the
    // scenario the scheduler sees is the same whether or not that cycle
    // lands before the selection. Selection is observed via state.backends.

    /// Tiny backend mock: for every accepted connection it reads the
    /// request line and answers by path —
    ///  - GET /v1/models     → 200 + the OpenAI-style model list (`v1_models`)
    ///  - GET /api/v1/models → 200 + LM Studio native list, or 404 when None
    ///  - GET /api/tags, /api/ps → 200 + LM Studio-style rejection (remembered
    ///    as a bad endpoint by the health loop)
    ///  - anything else (the actual proxied POST) → 200 {"ok":true}, quickly
    ///
    /// Returns (URL, abort handle of the accept loop).
    async fn spawn_mock_backend(
        v1_models: &str,
        native_models: Option<&str>,
    ) -> (String, tokio::task::AbortHandle) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock backend");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("mock local addr")
        );
        let v1 = v1_models.to_string();
        let native = native_models.map(|s| s.to_string());
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break, // accept loop aborted
                };
                let v1 = v1.clone();
                let native = native.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = match sock.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    let (status, body) = match path {
                        "/v1/models" => ("200 OK", v1.clone()),
                        "/api/v1/models" => match &native {
                            Some(json) => ("200 OK", json.clone()),
                            None => ("404 Not Found", r#"{"detail":"Not Found"}"#.to_string()),
                        },
                        "/api/tags" | "/api/ps" => (
                            "200 OK",
                            r#"{"error":"Unexpected endpoint or method."}"#.to_string(),
                        ),
                        _ => ("200 OK", r#"{"ok":true}"#.to_string()),
                    };
                    let response = format!(
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (url, handle.abort_handle())
    }

    /// LM Studio backend scenario shared by the tier tests: ovisocr2 loaded,
    /// qwen listed but not loaded (probe answers that reproduce exactly this
    /// state, loaded_state_known = true).
    const OVISOCR2_V1_MODELS: &str =
        r#"{"data":[{"id":"qwen3.8-27b"},{"id":"ovisocr2@q4_k_m"}]}"#;
    const OVISOCR2_NATIVE: &str = r#"{"models":[{"key":"ovisocr2@q4_k_m","display_name":"OvisOCR2","loaded_instances":[{"id":"ovisocr2@q4_k_m"}]}]}"#;
    /// OpenAI-style list for the second backend in each scenario.
    const QWEN_V1_MODELS: &str = r#"{"data":[{"id":"qwen3.8-27b"}]}"#;

    /// Marks `b` as an online LM Studio backend with the given available /
    /// loaded model sets and native model list — exactly the state the
    /// matching mock's probe answers produce.
    fn lmstudio_backend_state(
        b: &mut BackendStatus,
        available: &[&str],
        loaded: &[&str],
        native: Vec<LmModelInfo>,
    ) {
        b.is_online = true;
        b.api_type = BackendApiType::Both;
        b.available_models = set(available);
        b.loaded_models = set(loaded);
        b.loaded_state_known = true;
        b.lmstudio = true;
        b.native_models = native;
    }

    /// One LM Studio native-list entry with a loaded instance (id == key).
    fn lm_native_loaded(key: &str, display: &str) -> Vec<LmModelInfo> {
        vec![LmModelInfo {
            key: key.to_string(),
            display_name: Some(display.to_string()),
            loaded_instance_ids: vec![key.to_string()],
        }]
    }

    /// Two-backend test AppState (same shape as the one in
    /// `stuck_request_fails_fast_with_503`).
    fn new_test_state(urls: Vec<String>) -> Arc<AppState> {
        Arc::new(AppState::new(
            urls,
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            1,     // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
        ))
    }

    /// Enqueue one `POST /v1/chat/completions` request for `qwen3.8-27b`
    /// under user "tester" (full Task construction copied from
    /// `stuck_request_fails_fast_with_503`). Returns the receiver — keep it
    /// alive for the test's duration so the worker doesn't drop the request
    /// as client-gone.
    fn enqueue_qwen_request(state: &Arc<AppState>) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        state
            .queues
            .lock()
            .unwrap()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body: Bytes::from(r#"{"model":"qwen3.8-27b","messages":[]}"#),
                responder: tx,
                requested_model: Some("qwen3.8-27b".to_string()),
                stuck_warned: false,
                queued_at: std::time::Instant::now(),
            });
        rx
    }

    /// Poll state.backends every 100 ms (10 s overall) until exactly one
    /// backend has taken the single enqueued request: in-flight
    /// (`active_requests == 1`) or completed (`processed_count == 1`). The
    /// mock answers instantly, so the in-flight window is shorter than the
    /// poll interval; the stable completion counter is the reliable signal.
    /// Returns the backend index that took the request.
    async fn wait_for_picked_backend(state: &Arc<AppState>) -> Option<usize> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let signals: Vec<usize> = state
                    .backends
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|b| b.active_requests + b.processed_count)
                    .collect();
                let total: usize = signals.iter().sum();
                if total == 1 {
                    // Only one request exists, so the signal is on one backend.
                    return signals.into_iter().position(|s| s == 1);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Wait until the picked backend has completed the request
    /// (`processed_count == 1` is stable; the in-flight counter is back to 0).
    async fn wait_for_completion(state: &Arc<AppState>, idx: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let (processed, active) = {
                    let b = &state.backends.lock().unwrap()[idx];
                    (b.processed_count, b.active_requests)
                };
                if processed == 1 && active == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("picked backend never completed the request");
    }

    /// Tier 1: an eligible backend where the requested model is LOADED wins
    /// over an eligible backend that only lists it (routing there would
    /// evict the other's loaded model).
    #[tokio::test]
    async fn tier1_routes_to_backend_with_model_loaded() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: LM Studio
        // with qwen loaded.
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(
            QWEN_V1_MODELS,
            Some(
                r#"{"models":[{"key":"unsloth/qwen3.8-27b@q8_0","display_name":"Qwen3.8 27B","loaded_instances":[{"id":"unsloth/qwen3.8-27b@q8_0"}]}]}"#,
            ),
        )
        .await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);

        // Same state the mocks' probe answers produce: the immediate first
        // health cycle may run and apply them without changing the scenario.
        {
            let mut backends = state.backends.lock().unwrap();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(
                &mut backends[1],
                &["qwen3.8-27b"],
                &["unsloth/qwen3.8-27b@q8_0"],
                lm_native_loaded("unsloth/qwen3.8-27b@q8_0", "Qwen3.8 27B"),
            );
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // qwen is loaded on backend1 (tier 1); backend0 would have to evict
        // ovisocr2 — the request must go to backend1.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 1: a backend that does not report loaded state at all
    /// (vLLM-style: no /api/ps or /api/v1/models) counts as READY when it
    /// lists the requested model, and is preferred over a backend that
    /// would have to evict a loaded model.
    #[tokio::test]
    async fn tier1_treats_unknown_loaded_state_as_ready() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: vLLM-style
        // (404 on the native list, so loaded state stays unknown).
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(QWEN_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            // vLLM-style: lists the model, reports no loaded state.
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::OpenAi;
            backends[1].available_models = set(&["qwen3.8-27b"]);
            backends[1].loaded_models = HashSet::new();
            backends[1].loaded_state_known = false;
            backends[1].lmstudio = false;
            backends[1].native_models = Vec::new();
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // Unknown loaded state implies ready: backend1 is tier 1 even though
        // it cannot prove qwen is resident.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 2: a backend that reports loaded state but has nothing loaded
    /// (cold — loading the requested model evicts nothing) is preferred
    /// over an eligible backend with a different model loaded.
    #[tokio::test]
    async fn tier2_prefers_cold_backend_over_loaded_other() {
        // backend0: LM Studio with ovisocr2 loaded; backend1: cold LM Studio
        // (native list is empty: state known, nothing loaded).
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        let mock1 = spawn_mock_backend(QWEN_V1_MODELS, Some(r#"{"models":[]}"#)).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(&mut backends[1], &["qwen3.8-27b"], &[], Vec::new());
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // No backend has qwen loaded; the cold backend1 is tier 2 and wins
        // over backend0, whose ovisocr2 would be evicted.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 1);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Tier 3: when every eligible backend has a different model loaded and
    /// none is cold or ready, on-demand loading (which may evict the loaded
    /// model) is preserved as the last resort. Offline backends never enter
    /// the eligible set.
    #[tokio::test]
    async fn tier3_last_resort_on_demand_load() {
        let mock0 = spawn_mock_backend(OVISOCR2_V1_MODELS, Some(OVISOCR2_NATIVE)).await;
        // A port that was just closed: the "offline" backend never answers a
        // probe, so the immediate first health cycle keeps it offline.
        let offline_url = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind temp listener");
            let addr = l.local_addr().expect("temp addr");
            drop(l);
            format!("http://{}", addr)
        };

        let state = new_test_state(vec![mock0.0.clone(), offline_url]);
        {
            let mut backends = state.backends.lock().unwrap();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            lmstudio_backend_state(
                &mut backends[1],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            backends[1].is_online = false; // not eligible
        }

        let _rx = enqueue_qwen_request(&state);
        let worker = tokio::spawn(run_worker(state.clone()));

        // Only backend0 is eligible and it has ovisocr2 loaded — qwen is
        // served there via on-demand load (last-resort tier), not dropped.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
            assert!(!backends[1].is_online);
        }

        worker.abort();
        mock0.1.abort();
    }
}
