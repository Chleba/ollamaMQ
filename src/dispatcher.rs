use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use crate::lock::LockExt;
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

/// Minimal view of a request body used to pull out the requested model.
/// Deserializing into this instead of `serde_json::Value` lets serde skip
/// every other field (`IgnoredAny`) rather than allocating the whole tree —
/// request bodies routinely carry hundreds of KB of chat history.
#[derive(Deserialize)]
struct ModelField {
    #[serde(default)]
    model: Option<String>,
}

/// Pull the requested model name out of a request body.
///
/// Only JSON *objects* are considered: serde deserializes a top-level array
/// into `ModelField` positionally (`["llama3"]` would yield `llama3`), so the
/// leading byte is checked first. That also means non-JSON bodies — an
/// `/api/blobs` layer upload, say — are rejected without entering the parser.
fn extract_requested_model(body: &[u8]) -> Option<String> {
    if body
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_none_or(|b| *b != b'{')
    {
        return None;
    }
    serde_json::from_slice::<ModelField>(body)
        .ok()
        .and_then(|m| m.model)
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

/// The model-listing endpoint of an API family: (path, array key, id field).
fn model_list_shape(family: ApiFamily) -> Option<(&'static str, &'static str, &'static str)> {
    match family {
        ApiFamily::Ollama => Some(("/api/tags", "models", "name")),
        ApiFamily::OpenAi => Some(("/v1/models", "data", "id")),
        ApiFamily::Unknown => None,
    }
}

/// Answer a model listing from EVERY compatible backend at once.
///
/// `/api/tags` and `/v1/models` used to be proxied like any other request, so
/// a client saw whichever single backend the scheduler happened to pick: with
/// several backends holding different models the advertised list changed from
/// one poll to the next, and no client could see everything the proxy is able
/// to route to. Each backend is asked for its own listing and the entries are
/// merged, so per-model metadata (size, digest, details, ...) stays exactly as
/// that backend reported it instead of being synthesized here.
///
/// Entries are deduped by name/id with the first backend winning, and the
/// envelope of the first answering backend is reused so any extra top-level
/// fields survive. Returns `None` when no compatible backend produced a
/// listing — the caller then falls back to the normal queued proxy path.
///
/// Backends are queried without the client's headers (as health probes are),
/// so a backend that demands its own credentials on this endpoint contributes
/// nothing to the merge.
async fn aggregate_model_list(
    state: &AppState,
    family: ApiFamily,
) -> Option<axum::response::Response> {
    let (endpoint, array_key, id_field) = model_list_shape(family)?;

    let urls: Vec<String> = state
        .backends
        .lock_or_recover()
        .iter()
        .filter(|b| b.is_online && family_compatible(b, family))
        .map(|b| b.url.clone())
        .collect();
    if urls.is_empty() {
        return None;
    }

    let bodies = futures_util::future::join_all(urls.into_iter().map(|base| {
        let client = state.client.clone();
        let url = format!("{}{}", base, endpoint);
        async move {
            let res = client
                .get(&url)
                .timeout(crate::control::PROBE_TIMEOUT)
                .send()
                .await
                .ok()?;
            if !res.status().is_success() {
                return None;
            }
            res.json::<serde_json::Value>().await.ok()
        }
    }))
    .await;

    let mut envelope: Option<serde_json::Value> = None;
    let mut merged: Vec<serde_json::Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut answered = 0usize;
    for body in bodies.into_iter().flatten() {
        // A backend that answers 200 with an error object (LM Studio does this
        // for foreign endpoints) has no listing to contribute.
        let Some(entries) = body.get(array_key).and_then(|m| m.as_array()) else {
            continue;
        };
        for entry in entries {
            match entry.get(id_field).and_then(|v| v.as_str()) {
                // Already listed by an earlier backend.
                Some(id) if !seen.insert(id.to_string()) => continue,
                _ => merged.push(entry.clone()),
            }
        }
        answered += 1;
        if envelope.is_none() {
            envelope = Some(body);
        }
    }

    let mut envelope = envelope?;
    debug!(
        "aggregated {} at {} model(s) from {} backend(s)",
        endpoint,
        merged.len(),
        answered
    );
    envelope[array_key] = serde_json::Value::Array(merged);
    Some((StatusCode::OK, axum::Json(envelope)).into_response())
}

/// True for endpoints that only read backend metadata — no inference.
///
/// These neither wait for a free slot nor occupy one: they don't touch the
/// model runner, so a `/api/tags` poll (the kind chat UIs issue on a timer)
/// must not queue behind a multi-minute generation under the default
/// `max_concurrent_per_backend: 1`, nor behind a model load/unload. Everything
/// else — generate, chat, embeddings, blob uploads, pulls — is scheduled
/// normally.
pub fn is_metadata_path(path: &str) -> bool {
    matches!(
        path,
        "/" | "/api/tags" | "/api/ps" | "/api/version" | "/api/show" | "/v1/models"
    ) || path.starts_with("/v1/models/")
}

/// One entry from LM Studio's native model list (`GET /api/v1/models`):
/// the model key, optional display name, and ids of currently loaded instances
/// (used by model control to resolve the `instance_id` for unloading).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LmModelInfo {
    pub key: String,
    pub display_name: Option<String>,
    pub loaded_instance_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub url: String,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    pub api_type: BackendApiType,
    /// Reference-counted: replaced wholesale by each probe, but cloned by
    /// the TUI on every frame (a backend can list hundreds of models).
    pub available_models: Arc<HashSet<String>>,
    pub loaded_models: Arc<HashSet<String>>,
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
    /// In-flight request count keyed by `model_concurrency_key`. Only
    /// populated for requests that carry a model; enforces the per-model
    /// `max_concurrent_requests` limits from appconf.yaml (Plan D).
    pub active_by_model: HashMap<String, u32>,
    /// True when the backend speaks the LM Studio native REST API
    /// (`/api/v1/models*`), which enables model load/unload control.
    pub lmstudio: bool,
    /// LM Studio native model list (empty for other backend types).
    /// Reference-counted for the same reason as `available_models`.
    pub native_models: Arc<Vec<LmModelInfo>>,
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
    /// Round counter driving the boost user's every-other-turn slot. Atomic
    /// rather than a `Mutex` so it is not part of the lock stack the scheduler
    /// already holds while choosing a task.
    pub global_counter: std::sync::atomic::AtomicUsize,
    /// Fair-share scheduling scores, keyed by user. See [`FairShare`].
    pub fair_share: Mutex<HashMap<String, FairShare>>,
    pub notify: Notify,
    pub backend_freed: Notify,
    pub backends: Mutex<Vec<BackendStatus>>,
    /// Last backend index handed a request, for round-robin among equally
    /// loaded backends. Atomic for the same reason as `global_counter`.
    pub last_backend_idx: std::sync::atomic::AtomicUsize,
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
    /// Per-model concurrency limits keyed by `model_concurrency_key`; rebuilt
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
    /// Ceiling on `queued_bytes` (`settings.max_queued_bytes`).
    pub max_queued_bytes: u64,
    /// Bytes of request bodies currently waiting in the queues. Queued tasks
    /// own their whole body, so the per-user request cap alone still allowed
    /// gigabytes to pile up; this bounds it in bytes. Read and updated only
    /// under the `queues` lock, so the checks are atomic with the push/pop.
    pub queued_bytes: std::sync::atomic::AtomicU64,
}

/// One line of the TUI Logs panel: a request entering ("IN") or leaving
/// ("OUT") the proxy, or a model-control action ("CTL").
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Reference-counted: the TUI snapshots the newest events on every frame,
    /// and these strings run to `log_content_limit` (64 KiB by default).
    pub content: Option<Arc<String>>,
}

/// Half-life of a user's fair-share score: load older than this counts half
/// as much when deciding whose turn it is.
const FAIR_SHARE_HALF_LIFE: std::time::Duration = std::time::Duration::from_secs(300);

/// Recent scheduling load for one user, as an exponentially decaying score.
///
/// Ordering used to be by `processed_counts`, which is cumulative since
/// startup and only counts fully delivered responses. Two things went wrong
/// with that: a client whose requests always failed or disconnected never
/// incremented it and so kept permanent priority over everyone else, and on a
/// long-lived proxy a newcomer started thousands of requests "behind" an
/// incumbent and monopolized every backend until it caught up. This score is
/// charged when a request is DISPATCHED — whatever the outcome — and halves
/// every [`FAIR_SHARE_HALF_LIFE`], so the order reflects recent load only.
#[derive(Clone, Copy, Debug)]
pub struct FairShare {
    score: f64,
    /// Both the decay reference point and the user's last-seen time.
    updated: std::time::Instant,
}

impl FairShare {
    fn new(now: std::time::Instant) -> Self {
        Self { score: 0.0, updated: now }
    }

    /// The score as of `now`, after decay. Cheap and side-effect free.
    fn value_at(&self, now: std::time::Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.score * 0.5f64.powf(elapsed / FAIR_SHARE_HALF_LIFE.as_secs_f64())
    }

    /// Roll the decay forward to `now` without adding load — marks the user
    /// as seen while keeping the decay maths exact.
    fn touch(&mut self, now: std::time::Instant) {
        self.score = self.value_at(now);
        self.updated = now;
    }

    /// Roll forward and charge one dispatched request.
    fn charge(&mut self, now: std::time::Instant) {
        self.touch(now);
        self.score += 1.0;
    }

    /// When this user was last seen (request arrived or was dispatched).
    fn last_seen(&self) -> std::time::Instant {
        self.updated
    }
}

/// Longest a user with nothing queued and nothing in flight is kept in the
/// per-user bookkeeping.
const USER_RETENTION: std::time::Duration = std::time::Duration::from_secs(3600);

/// Hard ceiling on tracked users; the least recently seen idle ones are
/// dropped beyond it, whatever their retention.
const MAX_TRACKED_USERS: usize = 10_000;

/// How often idle users are pruned.
const USER_PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Most requests one user may have waiting before further ones are refused.
const MAX_QUEUED_PER_USER: usize = 100;

/// Body limit for `/api/blobs/{digest}`, which carries raw model layers rather
/// than JSON. Every other route uses `settings.max_body_bytes`.
pub const BLOB_BODY_LIMIT: usize = 1024 * 1024 * 1024;

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
        max_queued_bytes: u64,
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
                available_models: Arc::default(),
                loaded_models: Arc::default(),
                loaded_state_known: false,
                loaded_ctx: HashMap::new(),
                current_model: None,
                active_by_model: HashMap::new(),
                lmstudio: false,
                native_models: Arc::default(),
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
            global_counter: std::sync::atomic::AtomicUsize::new(0),
            fair_share: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            backend_freed: Notify::new(),
            backends: Mutex::new(backends),
            last_backend_idx: std::sync::atomic::AtomicUsize::new(0),
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
            max_queued_bytes,
            queued_bytes: std::sync::atomic::AtomicU64::new(0),
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
            ips: self.blocked_ips.lock_or_recover().clone(),
            users: self.blocked_users.lock_or_recover().clone(),
        };
        if let Ok(content) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(BLOCKED_FILE, content);
        }
    }

    pub fn block_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock_or_recover();
            ips.insert(ip);
        }
        self.save_blocked_items();
        warn!("IP blocked: {}", ip);
    }

    pub fn block_user(&self, user_id: String) {
        {
            let mut users = self.blocked_users.lock_or_recover();
            users.insert(user_id.clone());
        }
        self.save_blocked_items();
        warn!("User blocked: {}", user_id);
    }

    #[allow(dead_code)]
    pub fn unblock_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock_or_recover();
            ips.remove(&ip);
        }
        self.save_blocked_items();
        info!("IP unblocked: {}", ip);
    }

    #[allow(dead_code)]
    pub fn unblock_user(&self, user_id: &str) {
        {
            let mut users = self.blocked_users.lock_or_recover();
            users.remove(user_id);
        }
        self.save_blocked_items();
        info!("User unblocked: {}", user_id);
    }

    pub fn is_ip_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ips.lock_or_recover().contains(ip)
    }

    pub fn is_user_blocked(&self, user_id: &str) -> bool {
        self.blocked_users.lock_or_recover().contains(user_id)
    }

    /// Append an event to the logs ring buffer (bounded).
    pub fn log_event(&self, ev: LogEvent) {
        let mut logs = self.logs.lock_or_recover();
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

/// If a model config entry pins `requested` (by normalized name or identifier)
/// to a non-empty backend list, return that list. First matching entry wins
/// (config file order). Returns None when the model is not pinned.
fn model_pin_for(
    requested: &str,
    configs: &[crate::config::ModelConfig],
) -> Option<Vec<String>> {
    let requested_norm = normalize_model_id(requested);
    configs
        .iter()
        .find(|c| {
            !c.backends.is_empty()
                && (requested_norm == normalize_model_id(&c.name)
                    || c.identifier
                        .as_deref()
                        .map(|id| !id.is_empty() && requested_norm == normalize_model_id(id))
                        .unwrap_or(false))
        })
        .map(|c| c.backends.clone())
}

/// Case-insensitive substring backend URL match for pin routing: the pin
/// URL is normalized with `trim_end_matches('/')` + `to_lowercase()`, and a
/// backend matches when its (lowercased) URL contains it — consistent with
/// `control::config_targets` load targeting, so the same `backends:` entry
/// that loads a model also routes requests for it.
fn pin_url_matches(pin_url: &str, backend_url: &str) -> bool {
    let pin_low = pin_url.trim_end_matches('/').to_lowercase();
    backend_url.to_lowercase().contains(&pin_low)
}

/// Publisher-agnostic part of a normalized id (`owner/model` -> `model`).
fn model_base(id: &str) -> String {
    let n = normalize_model_id(id);
    match n.rsplit_once('/') {
        Some((_, m)) => m.to_string(),
        None => n,
    }
}

/// Key under which per-model concurrency is both limited and counted.
///
/// Every site that reads `models[].max_concurrent_requests` or touches
/// `active_by_model` MUST use this one function. Keying the limits by one
/// normalization and the in-flight counters by another meant a config entry
/// `qwen/qwen3.8-27b` never matched a client asking for `qwen3.8-27b`: the
/// limit silently fell back to 1, and the two spellings accumulated in
/// separate counters, so the cap could be exceeded on a single backend.
///
/// It is the publisher-stripped base name — the same normalization the routing
/// rules apply to bare requests — so two publishers' builds of one model share
/// a budget. That errs toward protecting the backend.
fn model_concurrency_key(id: &str) -> String {
    model_base(id)
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

/// Per-model concurrency limits keyed by `model_concurrency_key`, from the
/// `models:` section of appconf.yaml. Unlisted models default to 1 at lookup.
pub fn build_model_limits(configs: &[crate::config::ModelConfig]) -> HashMap<String, u32> {
    configs
        .iter()
        .map(|c| (model_concurrency_key(&c.name), c.max_concurrent_requests))
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
    // The body must parse as JSON *in this prefix*. A dialect rejection is a
    // short error object, so it always fits; a prefix that is truncated
    // mid-JSON therefore belongs to some long body — a streaming completion,
    // say — and must not be scanned for "endpoint"/"method" as raw text. Doing
    // so let an assistant reply that happened to be cut mid-sentence while
    // discussing HTTP methods count as a strike against the backend, and two
    // strikes drop it from rotation for that API family.
    let Some(value) = serde_json::from_str::<serde_json::Value>(text).ok() else {
        return false;
    };
    let Some(error_text) = value.get("error").and_then(|e| e.as_str()) else {
        return false;
    };
    let error_text = error_text.to_lowercase();
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
    let mut backends = state.backends.lock_or_recover();
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

/// Give back the queue-memory budget held by a task that has left a queue.
/// Saturating, so a task queued outside the normal enqueue path can never wrap
/// the counter and lock out every later request.
fn release_queued_bytes(state: &AppState, bytes: u64) {
    let _ = state.queued_bytes.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |held| Some(held.saturating_sub(bytes)),
    );
}

/// Drop per-user bookkeeping for users that are gone.
///
/// Every one of these maps is keyed by the client-supplied `X-User-ID` header
/// and nothing ever removed from them, so a client could grow them without
/// bound just by varying the header. A user is prunable only while nothing of
/// theirs is queued or in flight; beyond that they are kept for
/// [`USER_RETENTION`] so the dashboard still shows recent activity, and the
/// least recently seen are dropped early once more than [`MAX_TRACKED_USERS`]
/// are tracked.
fn prune_idle_users(state: &AppState, retention: std::time::Duration) {
    let now = std::time::Instant::now();

    // Candidates: nothing queued, nothing being served.
    let mut idle: Vec<(String, std::time::Instant)> = {
        let queues = state.queues.lock_or_recover();
        let processing = state.processing_counts.lock_or_recover();
        let scores = state.fair_share.lock_or_recover();
        scores
            .iter()
            .filter(|(u, _)| {
                queues.get(*u).is_none_or(|q| q.is_empty())
                    && processing.get(*u).copied().unwrap_or(0) == 0
            })
            .map(|(u, s)| (u.clone(), s.last_seen()))
            .collect()
    };
    if idle.is_empty() {
        return;
    }

    // Stalest first, so an over-cap trim drops the least recently seen.
    idle.sort_by_key(|(_, seen)| *seen);
    let tracked = state.fair_share.lock_or_recover().len();
    let over_cap = tracked.saturating_sub(MAX_TRACKED_USERS);

    let mut remove: Vec<String> = idle
        .iter()
        .enumerate()
        .filter(|(i, (_, seen))| {
            *i < over_cap || now.saturating_duration_since(*seen) > retention
        })
        .map(|(_, (user, _))| user.clone())
        .collect();
    if remove.is_empty() {
        return;
    }

    // Re-check emptiness while removing: a request may have arrived since the
    // candidate list was built, and dropping a queue with a task still in it
    // would lose that request.
    {
        let mut queues = state.queues.lock_or_recover();
        remove.retain(|u| queues.get(u).is_none_or(|q| q.is_empty()));
        for u in &remove {
            queues.remove(u);
        }
    }
    for u in &remove {
        state.processing_counts.lock_or_recover().remove(u);
        state.processed_counts.lock_or_recover().remove(u);
        state.dropped_counts.lock_or_recover().remove(u);
        state.user_ips.lock_or_recover().remove(u);
        state.fair_share.lock_or_recover().remove(u);
    }
    debug!("pruned {} idle user(s) from per-user state", remove.len());
}

/// One pass of the background health check: probe every backend and merge the
/// results into `state`.
///
/// Backends are probed CONCURRENTLY. Probing them one after another let a
/// single unresponsive backend hold up everyone else's status for the whole
/// round — combined with the probe timeout (see [`crate::control::PROBE_TIMEOUT`])
/// that could stall the loop for minutes while the scheduler routed on stale
/// online/loaded state.
async fn health_check_round(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    full_reprobe: bool,
    probe_timeout: std::time::Duration,
) {
    // Snapshot what to probe, and which endpoints to skip, in one lock pass.
    let to_check: Vec<(usize, String, bool, HashSet<String>)> = {
        let backends = state.backends.lock_or_recover();
        backends
            .iter()
            .enumerate()
            .map(|(i, b)| {
                // Skip endpoints this backend is known to reject — except on a
                // full re-probe or right after recovery from offline (memory
                // may be stale after a restart). Never skip both primary API
                // probes: online status must stay re-establishable.
                let skip = if full_reprobe || !b.is_online {
                    HashSet::new()
                } else {
                    let mut s = b.known_bad_endpoints.clone();
                    if s.contains("/api/tags") && s.contains("/v1/models") {
                        s.clear();
                    }
                    s
                };
                (i, b.url.clone(), b.is_online, skip)
            })
            .collect()
    };

    let probes = futures_util::future::join_all(to_check.into_iter().map(
        |(idx, url, was_online, skip)| {
            let client = client.clone();
            async move {
                let probe =
                    crate::control::probe_backend(&client, &url, &skip, probe_timeout).await;
                (idx, url, was_online, probe)
            }
        },
    ))
    .await;

    let mut any_changed = false;
    for (idx, url, was_online, probe) in probes {
        let mut backends = state.backends.lock_or_recover();
        let Some(b) = backends.get_mut(idx) else {
            continue;
        };

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
            || *b.available_models != probe.available_models
            || *b.loaded_models != probe.loaded_models;

        let was_api_type = b.api_type;
        let now_online = probe.is_online;
        crate::control::apply_probe(b, probe);

        if !was_online && now_online {
            info!("Backend {} status changed to: ONLINE", url);
        } else if was_online && !now_online {
            info!("Backend {} status changed to: OFFLINE", url);
        }
        if b.api_type != was_api_type {
            info!("Backend {} API type detected: {}", url, b.api_type.display());
        }
        for e in &newly_bad {
            info!(
                "Backend {} does not serve {} — remembered, will skip it until re-check",
                url, e
            );
        }

        any_changed |= changed;
    }

    // Wake the dispatcher if anything relevant to scheduling changed (model
    // finished loading, backend came online, etc.) so queued requests are
    // re-evaluated immediately.
    if any_changed {
        state.notify.notify_one();
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
            health_check_round(
                &health_state,
                &health_client,
                full_reprobe,
                crate::control::PROBE_TIMEOUT,
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    // Bound the per-user bookkeeping (see `prune_idle_users`).
    let prune_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(USER_PRUNE_INTERVAL).await;
            prune_idle_users(&prune_state, USER_RETENTION);
        }
    });

    loop {
        let selection_opt = {
            // Snapshot the model config once, before the queues lock, so the
            // per-task pin lookup below never takes model_config (lock order:
            // model_config -> queues -> backends).
            let configs = state.model_config.lock_or_recover().clone();
            let mut queues = state.queues.lock_or_recover();
            // Backends with a model control op (load/unload) in flight are treated as
            // busy: loading a new model can evict whatever is currently running.
            let control_busy: HashSet<usize> =
                state.control_ops.lock_or_recover().keys().copied().collect();
            // Model pins for every queued task, computed BEFORE taking the
            // backends lock (the model_config lock must not nest under it —
            // config reloads run concurrently). A pin restricts a model's
            // requests to the backends listed in appconf.yaml (strict).
            let task_pins: HashMap<(String, usize), Vec<String>> = queues
                .iter()
                .flat_map(|(user, q)| {
                    q.iter().enumerate().filter_map(|(pos, t)| match &t.requested_model {
                        Some(model) => {
                            model_pin_for(model, &configs).map(|pin| ((user.clone(), pos), pin))
                        }
                        None => None,
                    })
                })
                .collect();
            let mut backends = state.backends.lock_or_recover();
            let mut last_idx = state
                .last_backend_idx
                .load(std::sync::atomic::Ordering::Relaxed);

            // 1. Pick a user and peek at their front task to know required API family
            let vip = state.vip_user.lock_or_recover().clone();
            let boost = state.boost_user.lock_or_recover().clone();
            let counter = state
                .global_counter
                .load(std::sync::atomic::Ordering::Relaxed);

            let mut active_users: Vec<String> = queues
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(u, _)| u.clone())
                .collect();

            if active_users.is_empty() {
                None
            } else {
                {
                    // Least recent load first. One acquisition for the whole
                    // sort: the comparator used to lock and release the counts
                    // twice per comparison — O(n log n) acquisitions, nested
                    // inside the four locks already held here.
                    let now = std::time::Instant::now();
                    let scores = state.fair_share.lock_or_recover();
                    active_users.sort_by(|a, b| {
                        let a_load = scores.get(a).map_or(0.0, |s| s.value_at(now));
                        let b_load = scores.get(b).map_or(0.0, |s| s.value_at(now));
                        a_load
                            .partial_cmp(&b_load)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.cmp(b))
                    });
                }

                // Build candidate order: VIP first, then boost on every other
                // turn, then fair-share round-robin over the rest. The order is
                // a permutation of `active_users` held as indices — it used to
                // be built by cloning the user ids into two more Vec<String>
                // and de-duplicating with O(n) string comparisons.
                let mut order: Vec<usize> = Vec::with_capacity(active_users.len());
                let position_of = |name: &String| active_users.iter().position(|u| u == name);
                if let Some(i) = vip.as_ref().and_then(&position_of) {
                    order.push(i);
                }
                if counter.is_multiple_of(2)
                    && let Some(i) = boost.as_ref().and_then(&position_of)
                    && !order.contains(&i)
                {
                    order.push(i);
                }
                if current_idx >= active_users.len() {
                    current_idx = 0;
                }
                let rest: Vec<usize> = (0..active_users.len())
                    .filter(|i| !order.contains(i))
                    .collect();
                current_idx += 1;
                if !rest.is_empty() {
                    let start = current_idx % rest.len();
                    order.extend(rest[start..].iter().copied());
                    order.extend(rest[..start].iter().copied());
                }

                let limits = state.model_limits.lock_or_recover();

                // Try users in candidate order. Within a user's queue, pick
                // the FIRST routable task (not just the front) so an
                // unroutable request — e.g. an Ollama-family call with every
                // Ollama backend offline — can't starve everything behind it.
                let mut selection: Option<(String, Task, usize, String, bool)> = None;
                'users: for &ui in &order {
                    let user_id = &active_users[ui];
                    let queue_len = queues.get(user_id).map(|q| q.len()).unwrap_or(0);
                    for pos in 0..queue_len {
                        let task_ref = match queues.get(user_id).and_then(|q| q.get(pos)) {
                            Some(t) => t,
                            None => continue,
                        };
                        let api_family = detect_api_family(&task_ref.path);
                        // Metadata reads are cheap and don't use the model
                        // runner: they ignore backend capacity and in-flight
                        // control ops, and are not accounted against either.
                        let metadata = is_metadata_path(&task_ref.path);
                        debug!(
                            "Request for user {}: pos={} path={} family={:?}",
                            user_id, pos, task_ref.path, api_family
                        );

                        // Backend pin for this task (pre-computed above, before
                        // the backends lock): Some(urls) restricts routing to
                        // exactly those backends; None = no restriction.
                        let pin = task_pins.get(&(user_id.clone(), pos)).cloned();

                        // Find eligible backends: online, has capacity (global per-backend
                        // cap + per-model limit), and support the required API + Model.
                        // `limits` is taken once per selection round, above —
                        // it used to be locked and dropped for every task examined.
                        let model_key =
                            task_ref.requested_model.as_deref().map(model_concurrency_key);
                        let model_limit = match &model_key {
                            Some(k) => limits.get(k).copied().unwrap_or(1),
                            None => 0, // unused for model-less requests
                        };
                        let mut eligible_indices: Vec<usize> = backends.iter()
                            .enumerate()
                            .filter(|(i, b)| {
                                let online = b.is_online;
                                let free = metadata
                                    || backend_has_capacity(
                                        b,
                                        model_key.as_deref(),
                                        model_limit,
                                        state.max_concurrent_per_backend,
                                    );
                                let no_op = metadata || !control_busy.contains(i);
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

                        // Apply the pin filter upstream of tiering/selection: a
                        // pinned model may only be served by its listed backends.
                        if let Some(ref pins) = pin {
                            eligible_indices.retain(|&i| {
                                pins.iter().any(|p| pin_url_matches(p, &backends[i].url))
                            });
                        }

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
                                        // A pinned model is only satisfiable by a backend
                                        // that also passes the pin filter — strict pinning
                                        // fails with 503 instead of routing elsewhere.
                                        Some(model) => {
                                            model_routable(model, &b.available_models)
                                                && pin.as_ref().map_or(true, |pins| {
                                                    pins.iter()
                                                        .any(|p| pin_url_matches(p, &b.url))
                                                })
                                        }
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
                                release_queued_bytes(&state, dropped.body.len() as u64);
                                state
                                    .global_counter
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let waited = dropped.queued_at.elapsed().as_secs();
                                // Pinned models get a distinct, user-diagnosable message.
                                let (log_info, error_msg) = match &pin {
                                    Some(_) => {
                                        let model =
                                            dropped.requested_model.clone().unwrap_or_default();
                                        (
                                            format!(
                                                "503 no pinned backend can serve model '{}' (waited {}s)",
                                                model, waited
                                            ),
                                            format!(
                                                "no configured backend available for model '{}'",
                                                model
                                            ),
                                        )
                                    }
                                    None => (
                                        format!("503 no backend can serve (waited {}s)", waited),
                                        "no backend available to serve this request".to_string(),
                                    ),
                                };
                                let mut dropped_counts = state.dropped_counts.lock_or_recover();
                                *dropped_counts.entry(user_id.clone()).or_insert(0) += 1;
                                drop(dropped_counts);
                                state.log_event(LogEvent {
                                    at: std::time::SystemTime::now(),
                                    dir: "OUT",
                                    user: user_id.clone(),
                                    model: dropped.requested_model.clone(),
                                    backend: None,
                                    info: log_info,
                                    content: None,
                                });
                                let responder = dropped.responder;
                                tokio::spawn(async move {
                                    let body =
                                        serde_json::json!({ "error": error_msg }).to_string();
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
                                    match &pin {
                                        // Satisfiable: some pinned backend may still serve
                                        // the request, so there is no 503 to promise — we
                                        // are merely waiting for it.
                                        Some(pins) => format!(
                                            "model '{}' is pinned to backend(s) {} which are not currently eligible; waiting for a pinned backend to become eligible",
                                            task_ref.requested_model.as_deref().unwrap_or("-"),
                                            pins.join(", ")
                                        ),
                                        None => "No backend free".to_string(),
                                    }
                                } else {
                                    match &pin {
                                        Some(pins) => format!(
                                            "no backend can serve model '{}' (pinned to: {}); will fail with 503 after {}s",
                                            task_ref.requested_model.as_deref().unwrap_or("-"),
                                            pins.join(", "),
                                            state.stuck_timeout.as_secs()
                                        ),
                                        None => format!(
                                            "No backend can serve; will fail with 503 after {}s",
                                            state.stuck_timeout.as_secs()
                                        ),
                                    }
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
                        release_queued_bytes(&state, task.body.len() as u64);
                        state
                            .global_counter
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Charge the user for the work now, not on success:
                        // a request that fails or is abandoned still consumed
                        // a backend, and must not buy priority for the next.
                        {
                            let now = std::time::Instant::now();
                            state
                                .fair_share
                                .lock_or_recover()
                                .entry(user_id.clone())
                                .or_insert_with(|| FairShare::new(now))
                                .charge(now);
                        }

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
                            .position(|&i| i > last_idx)
                            .unwrap_or(0);
                        let selected_backend_idx = candidates_backends[candidate_pos];

                        last_idx = selected_backend_idx;
                        state
                            .last_backend_idx
                            .store(last_idx, std::sync::atomic::Ordering::Relaxed);
                        if !metadata {
                            backends[selected_backend_idx].active_requests += 1;
                            if let Some(ref m) = task.requested_model {
                                let k = model_concurrency_key(m);
                                *backends[selected_backend_idx]
                                    .active_by_model
                                    .entry(k)
                                    .or_insert(0) += 1;
                            }
                            backends[selected_backend_idx].current_model =
                                task.requested_model.clone();
                        }

                        selection = Some((
                            user_id.clone(),
                            task,
                            selected_backend_idx,
                            backends[selected_backend_idx].url.clone(),
                            metadata,
                        ));
                        break 'users;
                    }
                }

                selection
            }
        };

        match selection_opt {
            Some((user_id, task, backend_idx, backend_url, metadata)) => {
                let state_clone = state.clone();
                let client_clone = client.clone();
                let url = format!("{}{}", backend_url, task.path);

                tokio::spawn(async move {
                    let log_model = task.requested_model.clone();
                    let api_family = detect_api_family(&task.path);
                    let log_out = |info: String,
                                   backend: Option<String>,
                                   content: Option<Arc<String>>| {
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

                    // Set only when a response was delivered to the client in
                    // full; `processed_count` used to be bumped on every exit
                    // path, so the TUI counted blocked, abandoned and errored
                    // requests as throughput.
                    let mut delivered = false;

                    let is_blocked = {
                        let user_ips = state_clone.user_ips.lock_or_recover();
                        let blocked_ips = state_clone.blocked_ips.lock_or_recover();
                        let blocked_users = state_clone.blocked_users.lock_or_recover();
                        blocked_users.contains(&user_id)
                            || user_ips
                                .get(&user_id)
                                .map(|ip| blocked_ips.contains(ip))
                                .unwrap_or(false)
                    };

                    if is_blocked || task.responder.is_closed() {
                        let mut dropped = state_clone.dropped_counts.lock_or_recover();
                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                        log_out(
                            "dropped (blocked)".into(),
                            Some(backend_url.clone()),
                            None,
                        );
                    } else {
                        {
                            let mut processing = state_clone.processing_counts.lock_or_recover();
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
                                        delivered = true;
                                        let mut counts =
                                            state_clone.processed_counts.lock_or_recover();
                                        *counts.entry(user_id.clone()).or_insert(0) += 1;
                                        drop(counts);
                                        let content_str = Arc::new(
                                            crate::reqlog::truncate_utf8(
                                                &content_acc,
                                                state_clone.log_content_limit,
                                            ),
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
                                            state_clone.dropped_counts.lock_or_recover();
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
                                let err_msg = Arc::new(format!("upstream error: {}", e));
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
                                let mut dropped = state_clone.dropped_counts.lock_or_recover();
                                *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                log_out(
                                    "dropped (backend error)".into(),
                                    Some(backend_url.clone()),
                                    Some(err_msg),
                                );
                            }
                        }

                        {
                            let mut processing = state_clone.processing_counts.lock_or_recover();
                            if let Some(count) = processing.get_mut(&user_id) {
                                *count = count.saturating_sub(1);
                            }
                        }
                    }

                    {
                        let mut backends = state_clone.backends.lock_or_recover();
                        let b = &mut backends[backend_idx];
                        // Metadata reads were never accounted at dispatch.
                        if !metadata {
                            b.active_requests = b.active_requests.saturating_sub(1);
                            if let Some(ref m) = task.requested_model {
                                let k = model_concurrency_key(m);
                                if let Some(c) = b.active_by_model.get_mut(&k) {
                                    *c = c.saturating_sub(1);
                                    if *c == 0 {
                                        b.active_by_model.remove(&k);
                                    }
                                }
                            }
                            // Nothing of ours is running here any more, so the
                            // backend has no current model. It used to keep
                            // showing the last model it ever touched.
                            if b.active_requests == 0 {
                                b.current_model = None;
                            }
                        }
                        if delivered {
                            b.processed_count += 1;
                        }
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
        let mut ips = state.user_ips.lock_or_recover();
        ips.insert(user_id.clone(), ip);
    }

    // Mark the user as seen (without charging them) so idle-user pruning has
    // an accurate last-seen even for requests that never get dispatched.
    {
        let now = std::time::Instant::now();
        state
            .fair_share
            .lock_or_recover()
            .entry(user_id.clone())
            .or_insert_with(|| FairShare::new(now))
            .touch(now);
    }

    // Model listings are answered from every backend at once instead of being
    // routed to one of them (see `aggregate_model_list`). If nothing can be
    // merged, fall through to the normal queued path.
    let family = detect_api_family(&path);
    if method == Method::GET
        && model_list_shape(family).is_some_and(|(endpoint, _, _)| endpoint == path)
        && let Some(response) = aggregate_model_list(&state, family).await
    {
        let ts = crate::reqlog::now_unix_millis();
        for (dir, info) in [
            ("IN", format!("GET {} body=0B", path)),
            ("OUT", format!("GET {} -> 200 aggregated", path)),
        ] {
            state.reqlog.log(crate::reqlog::ReqRecord {
                ts,
                dir,
                user: addr.to_string(),
                model: None,
                backend: Some("<all backends>".to_string()),
                method: "GET".to_string(),
                path: path.clone(),
                status: (dir == "OUT").then_some(200),
                bytes: Some(0),
                content_type: None,
                content: None,
            });
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir,
                user: user_id.clone(),
                model: None,
                backend: Some("<all backends>".to_string()),
                info,
                content: None,
            });
        }
        return response;
    }

    let (tx, rx) = mpsc::channel(32);
    let mut task_headers = headers.clone();
    task_headers.remove(axum::http::header::HOST);

    let requested_model = extract_requested_model(&body);

    let task_method = method.as_str().to_string();
    // Request-log content is captured before `body` moves into the Task.
    let body_len = body.len() as u64;
    let content = Arc::new(crate::reqlog::truncate_utf8(&body, state.log_content_limit));
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
        let mut queues = state.queues.lock_or_recover();
        let queue = queues.entry(user_id.clone()).or_default();
        // A per-user ceiling: queued tasks hold their whole request body, so
        // an unbounded queue is an unbounded memory commitment.
        if queue.len() >= MAX_QUEUED_PER_USER {
            drop(queues);
            warn!(
                "User {} already has {} requests queued; refusing this one",
                user_id, MAX_QUEUED_PER_USER
            );
            *state
                .dropped_counts
                .lock_or_recover()
                .entry(user_id.clone())
                .or_insert(0) += 1;
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir: "OUT",
                user: user_id.clone(),
                model: requested_model.clone(),
                backend: None,
                info: format!("429 queue full ({} waiting)", MAX_QUEUED_PER_USER),
                content: None,
            });
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({
                    "error": "too many queued requests for this user"
                })),
            )
                .into_response();
        }
        // Bodies are held for as long as the request waits, so the request
        // count alone did not bound the memory: 100 queued requests at the
        // maximum body size is gigabytes. A request is always admitted when
        // the queues are empty, so an oversized body can never lock the proxy
        // out of making progress.
        let queued_now = state
            .queued_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        if queued_now > 0 && queued_now + body_len > state.max_queued_bytes {
            drop(queues);
            warn!(
                "queued bodies at {} B (limit {}); refusing a {} B request from {}",
                queued_now, state.max_queued_bytes, body_len, user_id
            );
            *state
                .dropped_counts
                .lock_or_recover()
                .entry(user_id.clone())
                .or_insert(0) += 1;
            state.log_event(LogEvent {
                at: std::time::SystemTime::now(),
                dir: "OUT",
                user: user_id.clone(),
                model: requested_model.clone(),
                backend: None,
                info: format!("503 queue memory full ({} B waiting)", queued_now),
                content: None,
            });
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "proxy queue is full; retry shortly"
                })),
            )
                .into_response();
        }
        queue.push_back(task);
        state
            .queued_bytes
            .fetch_add(body_len, std::sync::atomic::Ordering::Relaxed);
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
    fn metadata_paths_are_classified() {
        for p in [
            "/",
            "/api/tags",
            "/api/ps",
            "/api/version",
            "/api/show",
            "/v1/models",
            "/v1/models/llama3",
        ] {
            assert!(is_metadata_path(p), "{p} should be a metadata read");
        }
        // Anything that can run inference, transfer a model or mutate state
        // is scheduled normally.
        for p in [
            "/api/generate",
            "/api/chat",
            "/api/embed",
            "/api/embeddings",
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/api/pull",
            "/api/push",
            "/api/create",
            "/api/copy",
            "/api/delete",
            "/api/blobs/sha256:abc",
        ] {
            assert!(!is_metadata_path(p), "{p} must not bypass scheduling");
        }
    }

    #[test]
    fn model_field_extracts_the_requested_model() {
        let parse = |b: &str| extract_requested_model(b.as_bytes());
        assert_eq!(
            parse(r#"{"model":"llama3","stream":true}"#).as_deref(),
            Some("llama3")
        );
        // Everything but `model` is skipped by the deserializer rather than
        // allocated — a big ignored field must not change the outcome.
        let big = format!(
            r#"{{"messages":[{{"role":"user","content":"{}"}}],"model":"qwen3"}}"#,
            "x".repeat(10_000)
        );
        assert_eq!(parse(&big).as_deref(), Some("qwen3"));
        // Field order must not matter either.
        assert_eq!(parse(r#"{"model":"a","options":{"num_ctx":8}}"#).as_deref(), Some("a"));

        // Leading whitespace is fine.
        assert_eq!(parse("  \n\t{\"model\":\"b\"}").as_deref(), Some("b"));

        // No model, not an object, not JSON, empty → None, as before.
        assert_eq!(parse(r#"{"stream":true}"#), None);
        // A top-level array must NOT be read positionally into `model`.
        assert_eq!(parse(r#"["llama3"]"#), None);
        assert_eq!(parse(r#""llama3""#), None);
        assert_eq!(parse("not json at all"), None);
        assert_eq!(parse(""), None);
        // A non-string `model` is not a usable name.
        assert_eq!(parse(r#"{"model":123}"#), None);
        assert_eq!(parse(r#"{"model":null}"#), None);
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
            available_models: Arc::default(),
            loaded_models: Arc::default(),
            loaded_state_known: false,
            loaded_ctx: HashMap::new(),
            current_model: None,
            active_by_model: HashMap::new(),
            lmstudio: false,
            native_models: Arc::default(),
            known_bad_endpoints: HashSet::new(),
            rejected_families: rejected.iter().copied().collect(),
            family_fail_counts: HashMap::new(),
        }
    }

    #[test]
    fn model_loaded_on_matches_ollama_ps() {
        let mut b = backend_with(BackendApiType::Ollama, &[]);
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["qwen3.8-27b:latest"]));
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "other-model"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_key_with_quant() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["unsloth/qwen3.8-27b@q8_0"]));
        // Bare request name reaches the publisher/quant-prefixed loaded key.
        assert!(model_loaded_on(&b, "qwen3.8-27b"));
    }

    #[test]
    fn model_loaded_on_matches_lmstudio_display_name() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.native_models = Arc::new(vec![LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: vec!["unsloth/qwen3.8-27b@q8_0".into()],
        }]);
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
        b.native_models = Arc::new(vec![LmModelInfo {
            key: "unsloth/qwen3.8-27b@q8_0".into(),
            display_name: Some("Qwen3.8 27B".into()),
            loaded_instance_ids: Vec::new(),
        }]);
        // Listed but with no loaded instances: not resident.
        assert!(!model_loaded_on(&b, "qwen3.8-27b"));
        assert!(!model_loaded_on(&b, "Qwen3.8 27B"));
    }

    #[test]
    fn model_loaded_on_no_match() {
        let mut b = backend_with(BackendApiType::OpenAi, &[]);
        b.lmstudio = true;
        b.loaded_state_known = true;
        b.loaded_models = Arc::new(set(&["ovisocr2@q4_k_m"]));
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

        // A body prefix truncated mid-JSON is NOT evidence either way: real
        // rejections are short and parse whole. This is the first 512 bytes of
        // a streaming answer that happens to discuss HTTP methods.
        let truncated = r#"{"model":"llama3","created_at":"2026-01-01T00:00:00Z","response":"The endpoint you want depends on the request method"#;
        assert!(!is_endpoint_rejection(StatusCode::OK, truncated));
        // Same shape, but complete and with no error field: still not one.
        let complete = r#"{"model":"llama3","response":"use the POST method on that endpoint"}"#;
        assert!(!is_endpoint_rejection(StatusCode::OK, complete));
        // A non-string error field is not a message to match on.
        assert!(!is_endpoint_rejection(StatusCode::OK, r#"{"error":{"code":42}}"#));
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
    fn model_limit_key_survives_publisher_and_quant_spellings() {
        // appconf.yaml.example pins this model with a publisher prefix while
        // clients ask for the bare name; both must land on the same key, or
        // the configured limit silently degrades to 1 and the two spellings
        // get separate in-flight budgets on one backend.
        let configs = vec![crate::config::ModelConfig {
            name: "qwen/qwen3.8-27b".into(),
            identifier: None,
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 3,
            backends: Vec::new(),
        }];
        let limits = build_model_limits(&configs);
        for requested in [
            "qwen3.8-27b",
            "qwen/qwen3.8-27b",
            "unsloth/qwen3.8-27b@q8_0",
            "QWEN3.8-27B:latest",
        ] {
            assert_eq!(
                limits.get(&model_concurrency_key(requested)),
                Some(&3),
                "'{requested}' must resolve to the configured limit"
            );
        }
        // A different model still gets its own (default) budget.
        assert_ne!(
            model_concurrency_key("qwen3.8-27b"),
            model_concurrency_key("qwen3.8-32b")
        );
        assert_eq!(limits.get(&model_concurrency_key("llama3")), None);
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
            512 * 1024 * 1024,
        ));

        let (tx, mut rx) = mpsc::channel(32);
        {
            let mut queues = state.queues.lock_or_recover();
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
        b.available_models = Arc::new(set(available));
        b.loaded_models = Arc::new(set(loaded));
        b.loaded_state_known = true;
        b.lmstudio = true;
        b.native_models = Arc::new(native);
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
        new_test_state_with_budget(urls, 512 * 1024 * 1024)
    }

    fn new_test_state_with_budget(urls: Vec<String>, max_queued_bytes: u64) -> Arc<AppState> {
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
            max_queued_bytes,
        ))
    }

    /// Enqueue one `POST /v1/chat/completions` request for `qwen3.8-27b`
    /// under user "tester" (full Task construction copied from
    /// `stuck_request_fails_fast_with_503`). Returns the receiver — keep it
    /// alive for the test's duration so the worker doesn't drop the request
    /// as client-gone.
    fn enqueue_qwen_request(state: &Arc<AppState>) -> mpsc::Receiver<ResponsePart> {
        let (tx, rx) = mpsc::channel(32);
        let body = Bytes::from(r#"{"model":"qwen3.8-27b","messages":[]}"#);
        state
            .queued_bytes
            .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::POST,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body,
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
                    .lock_or_recover()
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
                    let b = &state.backends.lock_or_recover()[idx];
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
            let mut backends = state.backends.lock_or_recover();
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
            let backends = state.backends.lock_or_recover();
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
            let mut backends = state.backends.lock_or_recover();
            lmstudio_backend_state(
                &mut backends[0],
                &["qwen3.8-27b", "ovisocr2@q4_k_m"],
                &["ovisocr2@q4_k_m"],
                lm_native_loaded("ovisocr2@q4_k_m", "OvisOCR2"),
            );
            // vLLM-style: lists the model, reports no loaded state.
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::OpenAi;
            backends[1].available_models = Arc::new(set(&["qwen3.8-27b"]));
            backends[1].loaded_models = Arc::default();
            backends[1].loaded_state_known = false;
            backends[1].lmstudio = false;
            backends[1].native_models = Arc::default();
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
            let backends = state.backends.lock_or_recover();
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
            let mut backends = state.backends.lock_or_recover();
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
            let backends = state.backends.lock_or_recover();
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
            let mut backends = state.backends.lock_or_recover();
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
            let backends = state.backends.lock_or_recover();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
            assert!(!backends[1].is_online);
        }

        worker.abort();
        mock0.1.abort();
    }

    /// Accept connections and never answer them — a backend that is reachable
    /// but black-holed, the case a probe must not wait out at the inference
    /// timeout. Returns (url, abort handle of the accept loop).
    async fn spawn_black_holed_backend() -> (String, tokio::task::AbortHandle) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind black-holed backend");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // hold the connection open, answer nothing
            }
        });
        (url, handle.abort_handle())
    }

    /// Read a finished response's JSON body.
    async fn response_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("response is JSON")
    }

    #[test]
    fn fair_share_decays_and_charges_every_dispatch() {
        let now = std::time::Instant::now();
        let mut u = FairShare::new(now);
        assert_eq!(u.value_at(now), 0.0);

        u.charge(now);
        assert!((u.value_at(now) - 1.0).abs() < 1e-9);
        // One half-life on, that unit of load counts half.
        let half = now + FAIR_SHARE_HALF_LIFE;
        assert!((u.value_at(half) - 0.5).abs() < 1e-9);

        // `touch` marks the user as seen without adding load.
        let mut v = FairShare::new(now);
        v.charge(now);
        v.touch(half);
        assert!((v.value_at(half) - 0.5).abs() < 1e-9);
        assert_eq!(v.last_seen(), half);
    }

    #[test]
    fn fair_share_lets_a_past_heavy_user_back_in() {
        // The old ordering key was cumulative since startup, so a user with a
        // big history stayed behind a newcomer for thousands of requests.
        let now = std::time::Instant::now();
        let mut heavy = FairShare::new(now);
        for _ in 0..10 {
            heavy.charge(now);
        }
        let mut light = FairShare::new(now);
        light.charge(now);

        // Twenty minutes (four half-lives) later the heavy user has been
        // quiet while the light user keeps working: 10/16 vs 1/16 + 1.
        let later = now + std::time::Duration::from_secs(1200);
        light.charge(later);
        assert!(
            heavy.value_at(later) < light.value_at(later),
            "the formerly heavy user must get a turn: {} vs {}",
            heavy.value_at(later),
            light.value_at(later)
        );
    }

    #[tokio::test]
    async fn queue_depth_is_capped_per_user() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        // Fill one user's queue to the cap. No worker runs, so nothing drains.
        // The receivers are held for the test's duration: a dropped one makes
        // the task look client-gone.
        let mut keep_alive = Vec::new();
        {
            let mut queues = state.queues.lock_or_recover();
            let q = queues.entry("tester".to_string()).or_default();
            for _ in 0..MAX_QUEUED_PER_USER {
                let (tx, rx) = mpsc::channel(32);
                keep_alive.push(rx);
                q.push_back(Task {
                    method: Method::POST,
                    user: "127.0.0.1:41000".to_string(),
                    path: "/api/chat".into(),
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(b"{}"),
                    responder: tx,
                    requested_model: None,
                    stuck_warned: false,
                    queued_at: std::time::Instant::now(),
                });
            }
        }

        let mut headers = HeaderMap::new();
        headers.insert("X-User-ID", "tester".parse().expect("header value"));
        let res = proxy_handler(
            State(state.clone()),
            ConnectInfo("127.0.0.1:41001".parse().expect("addr")),
            Method::POST,
            headers,
            axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
            Bytes::from_static(br#"{"model":"llama3"}"#),
        )
        .await
        .into_response();

        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        // The queue is unchanged and the refusal is counted as a drop.
        assert_eq!(
            state.queues.lock_or_recover()["tester"].len(),
            MAX_QUEUED_PER_USER
        );
        assert_eq!(
            state.dropped_counts.lock_or_recover().get("tester").copied(),
            Some(1)
        );
        drop(keep_alive);
    }

    #[tokio::test]
    async fn queued_body_bytes_are_capped() {
        let state = new_test_state_with_budget(vec!["http://127.0.0.1:9".into()], 1_000);

        let call = |body: &'static [u8]| {
            let state = state.clone();
            let mut headers = HeaderMap::new();
            headers.insert("X-User-ID", "tester".parse().expect("header value"));
            async move {
                proxy_handler(
                    State(state),
                    ConnectInfo("127.0.0.1:41002".parse().expect("addr")),
                    Method::POST,
                    headers,
                    axum::extract::OriginalUri("/api/chat".parse().expect("uri")),
                    Bytes::from_static(body),
                )
                .await
                .into_response()
            }
        };

        // Bodies already waiting take the budget close to its limit.
        state
            .queued_bytes
            .store(900, std::sync::atomic::Ordering::Relaxed);
        let res = call(&[b'x'; 200]).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            900,
            "a refused request must not consume budget"
        );
        assert_eq!(
            state.dropped_counts.lock_or_recover().get("tester").copied(),
            Some(1)
        );

        // With the queues empty, a body larger than the whole budget is still
        // admitted — it waits for a backend instead of being refused, so an
        // oversized request can never lock the proxy out of progress. No
        // worker runs, so an admitted request simply never completes.
        state
            .queued_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let admitted =
            tokio::time::timeout(std::time::Duration::from_millis(300), call(&[b'y'; 4_000]))
                .await;
        assert!(
            admitted.is_err(),
            "an oversized body on an empty queue must be admitted, not refused"
        );
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            4_000,
            "an admitted request must claim its body's budget"
        );
    }

    #[tokio::test]
    async fn idle_users_are_pruned_but_active_ones_kept() {
        let state = new_test_state(vec!["http://127.0.0.1:9".into()]);
        let now = std::time::Instant::now();
        let a_while_ago = now
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("monotonic clock has a second of history");

        {
            let mut scores = state.fair_share.lock_or_recover();
            // Seen a while back, nothing queued → prunable.
            scores.insert("stale".into(), FairShare { score: 0.0, updated: a_while_ago });
            // Seen just now → kept regardless.
            scores.insert("recent".into(), FairShare::new(now));
            // Seen a while back, but still has a request waiting → kept.
            scores.insert("waiting".into(), FairShare { score: 0.0, updated: a_while_ago });
        }
        state.processed_counts.lock_or_recover().insert("stale".into(), 7);
        state
            .user_ips
            .lock_or_recover()
            .insert("stale".into(), "127.0.0.1".parse().expect("ip"));
        let (tx, rx) = mpsc::channel(32);
        {
            state
                .queues
                .lock_or_recover()
                .entry("waiting".to_string())
                .or_default()
                .push_back(Task {
                    method: Method::POST,
                    user: "127.0.0.1:41000".to_string(),
                    path: "/api/chat".into(),
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(b"{}"),
                    responder: tx,
                    requested_model: None,
                    stuck_warned: false,
                    queued_at: now,
                });
        }

        prune_idle_users(&state, std::time::Duration::from_millis(500));

        let scores = state.fair_share.lock_or_recover();
        assert!(!scores.contains_key("stale"), "idle user should be dropped");
        assert!(scores.contains_key("recent"), "recently seen user stays");
        assert!(
            scores.contains_key("waiting"),
            "a user with a queued request must never be dropped"
        );
        // The stale user's other bookkeeping goes with them...
        assert!(!state.processed_counts.lock_or_recover().contains_key("stale"));
        assert!(!state.user_ips.lock_or_recover().contains_key("stale"));
        // ...and the waiting user's request is still queued.
        assert_eq!(state.queues.lock_or_recover()["waiting"].len(), 1);
        drop(rx);
    }

    #[tokio::test]
    async fn model_listing_is_merged_across_backends() {
        let (url_a, mock_a) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"qwen3.8-27b"},{"id":"shared"}]}"#,
            None,
        )
        .await;
        let (url_b, mock_b) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"shared"},{"id":"ovisocr2@q4_k_m"}]}"#,
            None,
        )
        .await;
        let state = new_test_state(vec![url_a, url_b]);
        {
            let mut backends = state.backends.lock_or_recover();
            for b in backends.iter_mut() {
                b.is_online = true;
                b.api_type = BackendApiType::OpenAi;
            }
        }

        let res = aggregate_model_list(&state, ApiFamily::OpenAi)
            .await
            .expect("both backends answer");
        let v = response_json(res).await;

        // The first answering backend's envelope is kept...
        assert_eq!(v["object"], "list");
        // ...and every backend's models are visible at once, deduped, in
        // backend order. Proxying to one backend showed only half of these.
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["qwen3.8-27b", "shared", "ovisocr2@q4_k_m"]);

        mock_a.abort();
        mock_b.abort();
    }

    #[tokio::test]
    async fn model_listing_skips_incompatible_and_offline_backends() {
        let (url_a, mock_a) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let (url_b, mock_b) = spawn_mock_backend(
            r#"{"object":"list","data":[{"id":"never-listed"}]}"#,
            None,
        )
        .await;
        let state = new_test_state(vec![url_a, url_b]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            // Speaks a different dialect: must not be asked for /v1/models.
            backends[1].is_online = true;
            backends[1].api_type = BackendApiType::Ollama;
        }

        let v = response_json(
            aggregate_model_list(&state, ApiFamily::OpenAi)
                .await
                .expect("the OpenAI backend answers"),
        )
        .await;
        let ids: Vec<&str> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, vec!["qwen3.8-27b"]);

        // With no compatible backend online there is nothing to merge, and the
        // caller falls back to the normal queued proxy path.
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = false;
        }
        assert!(aggregate_model_list(&state, ApiFamily::OpenAi).await.is_none());

        mock_a.abort();
        mock_b.abort();
    }

    #[tokio::test]
    async fn metadata_request_is_served_by_a_busy_backend() {
        let (url, mock) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            backends[0].available_models = Arc::new(set(&["qwen3.8-27b"]));
            // A generation holds the only slot (max_concurrent_per_backend = 1)
            // and a model load is running on top of it.
            backends[0].active_requests = 1;
        }
        state.control_ops.lock_or_recover().insert(
            0,
            crate::control::ControlOp {
                backend_idx: 0,
                action: crate::control::ControlAction::Load,
                requested: "qwen3.8-27b".into(),
                canonical: "qwen3.8-27b".into(),
                identifier: None,
                started: std::time::Instant::now(),
            },
        );

        // Queued first: a chat request that cannot run until the slot frees.
        let _chat_rx = enqueue_qwen_request(&state);
        // Queued behind it, same user: a metadata read.
        let (tx, mut rx) = mpsc::channel(32);
        state
            .queues
            .lock_or_recover()
            .entry("tester".to_string())
            .or_default()
            .push_back(Task {
                method: Method::GET,
                user: "127.0.0.1:41000".to_string(),
                path: "/v1/models".into(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
                responder: tx,
                requested_model: None,
                stuck_warned: false,
                queued_at: std::time::Instant::now(),
            });

        let worker = tokio::spawn(run_worker(state.clone()));

        // The metadata read is answered even though the backend is at capacity
        // AND control-busy — it never waits on the generation ahead of it.
        let part = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("metadata request was not served within 10s");
        match part {
            Some(ResponsePart::Status(status, _)) => assert_eq!(status, StatusCode::OK),
            other => panic!("expected a status part, got {:?}", other.is_some()),
        }

        {
            let backends = state.backends.lock_or_recover();
            // It never took the inference slot: still just the generation.
            assert_eq!(backends[0].active_requests, 1);
            // And it did not evict the chat request, which is still waiting.
            assert_eq!(state.queues.lock_or_recover()["tester"].len(), 1);
        }

        worker.abort();
        mock.abort();
    }

    #[tokio::test]
    async fn abandoned_requests_are_not_counted_as_throughput() {
        let (url, mock) = spawn_mock_backend(QWEN_V1_MODELS, None).await;
        let state = new_test_state(vec![url]);
        {
            let mut backends = state.backends.lock_or_recover();
            backends[0].is_online = true;
            backends[0].api_type = BackendApiType::OpenAi;
            backends[0].available_models = Arc::new(set(&["qwen3.8-27b"]));
        }

        // The client hangs up before the request is dispatched.
        drop(enqueue_qwen_request(&state));
        let worker = tokio::spawn(run_worker(state.clone()));

        let dropped = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if state
                    .dropped_counts
                    .lock_or_recover()
                    .get("tester")
                    .copied()
                    .unwrap_or(0)
                    > 0
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(dropped.is_ok(), "the abandoned request was never resolved");

        {
            let backends = state.backends.lock_or_recover();
            // It reached a backend, but no response was delivered — it is not
            // throughput. `processed_count` used to be bumped on every exit
            // path, including this one.
            assert_eq!(backends[0].processed_count, 0);
            assert_eq!(backends[0].active_requests, 0);
            // With nothing in flight the backend has no current model; it used
            // to keep displaying the last one it ever touched.
            assert!(backends[0].current_model.is_none());
        }
        // Leaving the queue returns the body's share of the memory budget.
        assert_eq!(
            state
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        worker.abort();
        mock.abort();
    }

    #[tokio::test]
    async fn health_round_probes_backends_concurrently() {
        // Three black-holed backends: every endpoint burns the full probe
        // budget, so a sequential round would take three times as long.
        let mut urls = Vec::new();
        let mut aborts = Vec::new();
        for _ in 0..3 {
            let (url, abort) = spawn_black_holed_backend().await;
            urls.push(url);
            aborts.push(abort);
        }
        let state = new_test_state(urls);
        let client = reqwest::Client::builder()
            .build()
            .expect("build probe client");
        let probe_timeout = std::time::Duration::from_millis(200);

        let started = std::time::Instant::now();
        health_check_round(&state, &client, true, probe_timeout).await;
        let elapsed = started.elapsed();
        for a in aborts {
            a.abort();
        }

        // One backend's own endpoint probes stay sequential (~4 x 200 ms);
        // three backends in sequence would be ~2.4 s.
        assert!(
            elapsed < std::time::Duration::from_millis(1800),
            "health round took {:?} — backends look sequential",
            elapsed
        );
        for b in state.backends.lock_or_recover().iter() {
            assert!(!b.is_online, "black-holed backend must be marked offline");
        }
    }

    // -- Backend-pinning tests -------------------------------------------------
    //
    // A non-empty `backends:` list in the model config pins routing: requests
    // for that model may only go to the listed backends (strict — no fallback
    // to other online backends, 503 after stuck_timeout when none can serve).

    /// OpenAI-style mock model list for a single bare model id "m".
    const M_V1_MODELS: &str = r#"{"data":[{"id":"m"}]}"#;

    /// Marks `b` as an online OpenAI backend listing the given models (no
    /// loaded-state reporting) — exactly the state the matching mock's probe
    /// answers produce.
    fn openai_backend_state(b: &mut BackendStatus, available: &[&str]) {
        b.is_online = true;
        b.api_type = BackendApiType::OpenAi;
        b.available_models = Arc::new(set(available));
        b.loaded_models = Arc::default();
        b.loaded_state_known = false;
    }

    /// Enqueue one `POST /v1/chat/completions` request for `model` under user
    /// "tester" (same shape as enqueue_qwen_request). Returns the receiver —
    /// keep it alive for the test's duration.
    fn enqueue_chat_request(
        state: &Arc<AppState>,
        model: &str,
    ) -> mpsc::Receiver<ResponsePart> {
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
                body: Bytes::from(format!(r#"{{"model":"{}","messages":[]}}"#, model)),
                responder: tx,
                requested_model: Some(model.to_string()),
                stuck_warned: false,
                queued_at: std::time::Instant::now(),
            });
        rx
    }

    /// Append a model entry to the test state's model config (the pin under
    /// test). `backends` empty = the entry does NOT pin.
    fn pin_model(state: &Arc<AppState>, name: &str, identifier: Option<&str>, backends: Vec<String>) {
        state.model_config.lock().unwrap().push(crate::config::ModelConfig {
            name: name.to_string(),
            identifier: identifier.map(|s| s.to_string()),
            max_ctx: None,
            keep_alive: None,
            max_concurrent_requests: 1,
            backends,
        });
    }

    /// Two-backend test AppState with a per-backend cap high enough that a
    /// pre-busy backend (active_requests = 1) still has capacity for the
    /// enqueued request (same shape as new_test_state).
    fn new_test_state_cap(urls: Vec<String>, cap: u32) -> Arc<AppState> {
        Arc::new(AppState::new(
            urls,
            5,     // request timeout
            86400, // load keep alive
            1,     // stuck_timeout: fail fast after 1s
            cap,   // max concurrent per backend
            "appconf.yaml".into(),
            Vec::new(),
            crate::reqlog::RequestLogger::disabled(),
            65_536,
            512 * 1024 * 1024,
        ))
    }

    /// Wait until the single enqueued request has been COMPLETED by one of the
    /// backends (processed_count == 1) and return which one. Unlike
    /// wait_for_picked_backend this ignores pre-set active_requests counts.
    async fn wait_for_served_by(state: &Arc<AppState>) -> Option<usize> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let done = state
                    .backends
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|b| b.processed_count)
                    .collect::<Vec<_>>();
                if let Some(pos) = done.iter().position(|&c| c == 1) {
                    return pos;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .ok()
    }

    /// Pinning restricts eligibility: a model pinned to backend A must be
    /// served by A even when the unpinned min-active-requests selection would
    /// pick the freer backend B.
    #[tokio::test]
    async fn pin_restricts_eligibility() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await; // B: freer

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            // B has fewer active requests: unpinned selection would pick B.
            backends[0].active_requests = 1;
        }
        pin_model(&state, "m", None, vec![mock0.0.clone()]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // The pin forces backend A despite B being freer.
        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 0);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A pin matches by identifier as well as name: entry name "x/y" with
    /// identifier "m" pins requests for model "m" to backend A only.
    #[tokio::test]
    async fn pin_matches_by_identifier() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
        }
        pin_model(&state, "x/y", Some("m"), vec![mock0.0.clone()]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // Unpinned round-robin (last_backend_idx = 0) would pick backend 1;
        // the identifier pin forces backend A.
        let picked = wait_for_picked_backend(&state)
            .await
            .expect("no backend picked up the request within 10s");
        assert_eq!(picked, 0);
        wait_for_completion(&state, picked).await;
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A pin URL matches by substring (case-insensitive), consistent with
    /// `control::config_targets` load targeting: a scheme-less `backends:`
    /// entry (as in appconf.yaml.example) routes as well as loads. The pin is
    /// derived from backend A's actual URL with the `http://` prefix
    /// stripped, so only A's full URL contains it.
    #[tokio::test]
    async fn pin_matches_by_substring_url() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await; // A: pinned
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![mock0.0.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
        }
        // Substring pin: A's URL without the scheme (B is a different port).
        let pin_url = mock0.0.trim_start_matches("http://").to_string();
        assert!(
            !pin_url.is_empty()
                && pin_url != mock0.0
                && !mock1.0.contains(&pin_url),
            "pin must be a proper substring of A's URL only: pin={} a={} b={}",
            pin_url,
            mock0.0,
            mock1.0
        );
        pin_model(&state, "m", None, vec![pin_url]);

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 0);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[0].processed_count, 1);
            assert_eq!(backends[1].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A model absent from the config is unpinned: both backends stay
    /// eligible and selection follows the existing min-active-requests rule
    /// (the freer backend B wins).
    #[tokio::test]
    async fn unpinned_model_unchanged() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await;
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            // Backend A is busier: min-active selection must pick B.
            backends[0].active_requests = 1;
        }
        // No config entry for "m" at all (state was built with an empty list).

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 1);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// A config entry with an empty `backends:` list is NOT a pin: the model
    /// stays eligible on every backend (min-active selection picks the freer
    /// backend B; a spurious pin to nothing would 503 instead).
    #[tokio::test]
    async fn empty_backends_not_a_pin() {
        let mock0 = spawn_mock_backend(M_V1_MODELS, None).await;
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state_cap(vec![mock0.0.clone(), mock1.0.clone()], 2);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &["m"]);
            openai_backend_state(&mut backends[1], &["m"]);
            backends[0].active_requests = 1; // B is freer
        }
        pin_model(&state, "m", None, Vec::new()); // empty list → no pin

        let _rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        let served = wait_for_served_by(&state)
            .await
            .expect("no backend served the request within 10s");
        assert_eq!(served, 1);
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 1);
            assert_eq!(backends[0].processed_count, 0);
        }

        worker.abort();
        mock0.1.abort();
        mock1.1.abort();
    }

    /// Strict pinning: when the only pinned backend is offline, an online
    /// unpinned backend that lists the model must NOT serve it — the request
    /// fails with 503 and the pinned error message after the stuck timeout.
    #[tokio::test]
    async fn pin_all_pinned_offline_fails_503() {
        // A: a port that was just closed: the "offline" pinned backend never
        // answers a probe (its available list is empty, like after an
        // outage), so nothing about it is satisfiable.
        let offline_url = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind temp listener");
            let addr = l.local_addr().expect("temp addr");
            drop(l);
            format!("http://{}", addr)
        };
        // B: online and lists the model, but is not in the pin.
        let mock1 = spawn_mock_backend(M_V1_MODELS, None).await;

        let state = new_test_state(vec![offline_url.clone(), mock1.0.clone()]);
        {
            let mut backends = state.backends.lock().unwrap();
            openai_backend_state(&mut backends[0], &[]);
            backends[0].is_online = false; // pinned backend offline
            openai_backend_state(&mut backends[1], &["m"]);
        }
        pin_model(&state, "m", None, vec![offline_url.clone()]);

        let mut rx = enqueue_chat_request(&state, "m");
        let worker = tokio::spawn(run_worker(state.clone()));

        // stuck_timeout is 1s: the request must be dropped with 503 and the
        // pinned (user-diagnosable) error message.
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
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no body within 2s")
            .expect("responder closed");
        match &chunk {
            ResponsePart::Chunk(bytes) => {
                let body = String::from_utf8_lossy(&bytes);
                assert!(
                    body.contains(r#"no configured backend available for model 'm'"#),
                    "unexpected 503 body: {}",
                    body
                );
            }
            _ => panic!("expected Chunk with the error body, got another part"),
        }

        // The unpinned online backend must never have served the request.
        {
            let backends = state.backends.lock().unwrap();
            assert_eq!(backends[1].processed_count, 0);
            assert_eq!(backends[1].active_requests, 0);
        }

        worker.abort();
        mock1.1.abort();
    }
}
