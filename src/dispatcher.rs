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
    pub headers: HeaderMap,
    pub body: Bytes,
    pub responder: mpsc::Sender<ResponsePart>,
    pub requested_model: Option<String>,
    /// Set once a "no backend available" warning has been logged for this
    /// task, so stuck requests are visible without spamming the log.
    pub stuck_warned: bool,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiFamily {
    Ollama,
    OpenAi,
    Unknown,
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
    pub current_model: Option<String>,
    /// True when the backend speaks the LM Studio native REST API
    /// (`/api/v1/models*`), which enables model load/unload control.
    pub lmstudio: bool,
    /// LM Studio native model list (empty for other backend types).
    pub native_models: Vec<LmModelInfo>,
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
    /// Model configuration from `appconf.yaml` (models to load on backends,
    /// with load settings). Re-read with the TUI 'r' key.
    pub model_config: Mutex<Vec<crate::config::ModelConfig>>,
    /// Path of the model config file.
    pub model_config_path: String,
    /// Ring buffer of recent request/control events for the TUI Logs panel.
    pub logs: Mutex<VecDeque<LogEvent>>,
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
}

/// How many log events to keep in the ring buffer.
const MAX_LOG_EVENTS: usize = 300;

impl AppState {
    pub fn new(
        backend_urls: Vec<String>,
        timeout: u64,
        load_keep_alive: i64,
        model_config_path: String,
        model_config: Vec<crate::config::ModelConfig>,
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
                current_model: None,
                lmstudio: false,
                native_models: Vec::new(),
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
            client,
            control_ops: Mutex::new(HashMap::new()),
            control_history: Mutex::new(VecDeque::new()),
            load_keep_alive,
            model_config: Mutex::new(model_config),
            model_config_path,
            logs: Mutex::new(VecDeque::new()),
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

fn smart_model_match(requested: &str, available: &HashSet<String>) -> bool {
    // 1. Exact match
    if available.contains(requested) {
        return true;
    }

    // 2. Normalized match (handle :latest and case sensitivity)
    available
        .iter()
        .any(|m| smart_model_match_one(requested, m))
}

/// Relaxed routing match used by the scheduler as a fallback when no backend
/// has an exact/tag-normalized id for the requested model. Clients often send
/// short names (`qwen3.8-27b`) while servers list publisher-qualified or
/// quantization-suffixed ids (`qwen/qwen3.8-27b`, `x@q8_0`). Mirrors the
/// substring step of `control::resolve_model_name`.
pub fn fuzzy_model_match(requested: &str, available: &HashSet<String>) -> bool {
    let req_low = requested.to_lowercase();
    let req_base = req_low.split(':').next().unwrap_or(&req_low);
    available.iter().any(|m| {
        let m_low = m.to_lowercase();
        let m_base = m_low.split(':').next().unwrap_or(&m_low);
        m_low.contains(&req_low)
            || m_low.contains(req_base)
            || req_base.contains(&m_base)
    })
}

/// Scheduler-side routability check: strict first, fuzzy fallback.
fn model_routable(requested: &str, available: &HashSet<String>) -> bool {
    smart_model_match(requested, available) || fuzzy_model_match(requested, available)
}

pub async fn run_worker(state: Arc<AppState>) {
    let client = state.client.clone();
    let mut current_idx = 0;

    // Background Health Check
    let health_state = state.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        loop {
            let backends_to_check: Vec<(usize, String)> = {
                let backends = health_state.backends.lock().unwrap();
                backends
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i, b.url.clone()))
                    .collect()
            };

            for (idx, url) in backends_to_check {
                let probe = crate::control::probe_backend(&health_client, &url).await;

                let mut backends = health_state.backends.lock().unwrap();
                let mut changed = false;
                if backends[idx].is_online != probe.is_online {
                    info!(
                        "Backend {} status changed to: {}",
                        url,
                        if probe.is_online { "ONLINE" } else { "OFFLINE" }
                    );
                    backends[idx].is_online = probe.is_online;
                    changed = true;
                }
                if backends[idx].api_type != probe.api_type {
                    info!(
                        "Backend {} API type detected: {}",
                        url,
                        probe.api_type.display()
                    );
                    backends[idx].api_type = probe.api_type;
                    changed = true;
                }
                if backends[idx].available_models != probe.available_models
                    || backends[idx].loaded_models != probe.loaded_models
                {
                    changed = true;
                }
                backends[idx].available_models = probe.available_models;
                backends[idx].loaded_models = probe.loaded_models;
                backends[idx].lmstudio = probe.lmstudio;
                backends[idx].native_models = probe.native_models;

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

                        // Find eligible backends: online, not busy, and support the required API + Model
                        let eligible_indices: Vec<usize> = backends.iter()
                            .enumerate()
                            .filter(|(i, b)| {
                                let online = b.is_online;
                                let free = b.active_requests < 1;
                                let no_op = !control_busy.contains(i);
                                if !online || !free || !no_op {
                                    debug!(
                                        "Backend {} rejected: online={}, active={}, control_op={}",
                                        b.url, online, b.active_requests, !no_op
                                    );
                                }
                                online && free && no_op
                            })
                            .filter(|(_, b)| {
                                // If a specific model is requested, backend MUST have it.
                                // If no model is requested, fall back to API family check.
                                let supported = if let Some(ref model) = task_ref.requested_model {
                                    let has_model = model_routable(model, &b.available_models);
                                    if !has_model {
                                        debug!("Backend {} rejected: model '{}' not found. Available: {:?}", b.url, model, b.available_models);
                                    }
                                    has_model
                                } else {
                                    // Unknown type backends are allowed (health check will classify them)
                                    let family_supported = matches!(b.api_type, BackendApiType::Unknown | BackendApiType::Both)
                                        || b.api_type.supports(api_family);
                                    if !family_supported {
                                        debug!("Backend {} rejected: api_family {:?} not supported by {:?}", b.url, api_family, b.api_type);
                                    }
                                    family_supported
                                };
                                supported
                            })
                            .map(|(i, _)| i)
                            .collect();

                        // Prefer backends where the requested model is already loaded in GPU memory
                        // (LM Studio: loaded_instances; Ollama: /api/ps). available_models stays the
                        // HARD requirement; loaded_models is only a PREFERENCE among eligible backends.
                        // If no eligible backend has it loaded, fall back to the full available set
                        // so on-demand loading still works.
                        let eligible_indices = if let Some(ref model) = task_ref.requested_model {
                            let loaded_eligible: Vec<usize> = eligible_indices
                                .iter()
                                .cloned()
                                .filter(|&i| model_routable(model, &backends[i].loaded_models))
                                .collect();
                            if loaded_eligible.is_empty() {
                                eligible_indices
                            } else {
                                loaded_eligible
                            }
                        } else {
                            eligible_indices
                        };

                        if eligible_indices.is_empty() {
                            if !task_ref.stuck_warned {
                                warn!(
                                    "No backend available for {} {} (model: {}, family: {:?}) for user {}; request parked in queue",
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
                    let log_out = |info: String, backend: Option<String>| {
                        state_clone.log_event(LogEvent {
                            at: std::time::SystemTime::now(),
                            dir: "OUT",
                            user: user_id.clone(),
                            model: log_model.clone(),
                            backend,
                            info,
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
                        );
                    } else {
                        {
                            let mut processing = state_clone.processing_counts.lock().unwrap();
                            *processing.entry(user_id.clone()).or_insert(0) += 1;
                        }

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

                                if task
                                    .responder
                                    .send(ResponsePart::Status(status, headers))
                                    .await
                                    .is_ok()
                                {
                                    let mut stream = response.bytes_stream();
                                    let mut client_disconnected = false;
                                    while let Some(chunk_res) = stream.next().await {
                                        match chunk_res {
                                            Ok(chunk) => {
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

                                    if !client_disconnected {
                                        let mut counts =
                                            state_clone.processed_counts.lock().unwrap();
                                        *counts.entry(user_id.clone()).or_insert(0) += 1;
                                        log_out(
                                            format!(
                                                "{} {}",
                                                status.as_u16(),
                                                status.canonical_reason().unwrap_or("")
                                            ),
                                            Some(backend_url.clone()),
                                        );
                                    } else {
                                        let mut dropped =
                                            state_clone.dropped_counts.lock().unwrap();
                                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                        log_out(
                                            "dropped (client gone)".into(),
                                            Some(backend_url.clone()),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = task.responder.send(ResponsePart::Error(e)).await;
                                let mut dropped = state_clone.dropped_counts.lock().unwrap();
                                *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                log_out(
                                    "dropped (backend error)".into(),
                                    Some(backend_url.clone()),
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
                        backends[backend_idx].active_requests =
                            backends[backend_idx].active_requests.saturating_sub(1);
                        backends[backend_idx].processed_count += 1;
                    }
                    state_clone.backend_freed.notify_one();
                });
            }
            None => {
                tokio::select! {
                    _ = state.notify.notified() => {},
                    _ = state.backend_freed.notified() => {},
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
    let task = Task {
        path: path.clone(),
        method,
        headers: task_headers,
        responder: tx,
        body,
        requested_model: requested_model.clone(),
        stuck_warned: false,
    };

    {
        let mut queues = state.queues.lock().unwrap();
        queues
            .entry(user_id.clone())
            .or_insert_with(VecDeque::new)
            .push_back(task);
    }

    state.log_event(crate::dispatcher::LogEvent {
        at: std::time::SystemTime::now(),
        dir: "IN",
        user: user_id.clone(),
        model: requested_model.clone(),
        backend: None,
        info: format!("queued {} {}", task_method, path),
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
    fn smart_match_exact_and_tag() {
        let avail = set(&["qwen3.8-27b:latest", "llama3"]);
        assert!(smart_model_match("qwen3.8-27b", &avail));
        assert!(smart_model_match("llama3:latest", &avail));
        assert!(!smart_model_match("mistral", &avail));
    }

    #[test]
    fn fuzzy_matches_publisher_and_quant_suffixed_ids() {
        // Client asks "qwen3.8-27b", server lists publisher/quant variants.
        let avail = set(&[
            "ovisocr2@q8_0",
            "unsloth/qwen3.8-27b@q8_0",
            "qwen/qwen3.8-27b",
            "huihui-qwen3.8-27b-abliterated",
        ]);
        assert!(fuzzy_model_match("qwen3.8-27b", &avail));
        assert!(model_routable("qwen3.8-27b", &avail));
    }

    #[test]
    fn fuzzy_does_not_match_unrelated_models() {
        let avail = set(&["llama3:latest", "mistral-7b", "text-embedding-nomic"]);
        assert!(!fuzzy_model_match("qwen3.8-27b", &avail));
        assert!(!model_routable("qwen3.8-27b", &avail));
    }

    #[test]
    fn strict_match_preferred_over_fuzzy() {
        // model_routable stays true for exact ids even when fuzzy also would
        let avail = set(&["qwen/qwen3.8-27b", "unsloth/qwen3.8-27b@q8_0"]);
        assert!(model_routable("unsloth/qwen3.8-27b@q8_0", &avail));
    }
}
