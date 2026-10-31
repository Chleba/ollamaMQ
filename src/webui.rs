//! Web dashboard: a JSON state snapshot, user actions that mirror the TUI
//! keys, and the single-file page that consumes them. The page is embedded in
//! the binary so the Docker image needs no extra files or build stage.

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::dispatcher::AppState;
use crate::lock::LockExt;

const INDEX_HTML: &str = include_str!("webui/index.html");
const ICON_SVG: &str = include_str!("webui/icon.svg");
/// Newest log events included in a snapshot (the ring buffer holds more).
const LOG_LIMIT: usize = 100;

/// `GET /ui` — the dashboard page. Served without auth; the page sends the
/// API key (when one is configured) with every `/admin/*` call. The build's
/// version replaces the page's `__VERSION__` placeholder (shown bottom-right,
/// as in the TUI).
pub async fn ui_page() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(INDEX_HTML.replace("__VERSION__", env!("CARGO_PKG_VERSION"))),
    )
}

/// `GET /icon.svg` — the dashboard's icon, also usable as an app-launcher icon.
/// Served without auth so a browser can load it before any login.
pub async fn icon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        ICON_SVG,
    )
}

/// `GET /admin/state` — everything the TUI shows, as one JSON document.
/// Request/response bodies are deliberately left out of the log events.
pub async fn admin_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let vip = state.vip_user.lock_or_recover().clone();
    let boost = state.boost_user.lock_or_recover().clone();
    let blocked_users = state.blocked_users.lock_or_recover().clone();
    let blocked_ips = state.blocked_ips.lock_or_recover().clone();

    let users: Vec<Value> = {
        let queues = state.queues.lock_or_recover();
        let processing = state.processing_counts.lock_or_recover();
        let processed = state.processed_counts.lock_or_recover();
        let dropped = state.dropped_counts.lock_or_recover();
        let ips = state.user_ips.lock_or_recover();

        let ids: BTreeSet<&String> = queues
            .keys()
            .chain(processing.keys())
            .chain(processed.keys())
            .chain(dropped.keys())
            .chain(blocked_users.iter())
            .collect();
        ids.into_iter()
            .map(|id| {
                let ip = ips.get(id).copied();
                json!({
                    "id": id,
                    "queued": queues.get(id).map_or(0, |q| q.len()),
                    "processing": processing.get(id).copied().unwrap_or(0),
                    "processed": processed.get(id).copied().unwrap_or(0),
                    "dropped": dropped.get(id).copied().unwrap_or(0),
                    "ip": ip.map(|ip| ip.to_string()),
                    "vip": vip.as_deref() == Some(id.as_str()),
                    "boost": boost.as_deref() == Some(id.as_str()),
                    "blocked": blocked_users.contains(id.as_str()),
                    "ip_blocked": ip.is_some_and(|ip| blocked_ips.contains(&ip)),
                })
            })
            .collect()
    };

    let backends: Vec<Value> = {
        let ops = state.control_ops.lock_or_recover();
        state
            .backends
            .lock_or_recover()
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let mut available: Vec<&String> = b.available_models.iter().collect();
                available.sort();
                let mut loaded: Vec<&String> = b.loaded_models.iter().collect();
                loaded.sort();
                json!({
                    "index": i,
                    "url": b.url,
                    "online": b.is_online,
                    "api": b.api_type.display(),
                    "active_requests": b.active_requests,
                    "processed": b.processed_count,
                    "current_model": b.current_model,
                    "available_models": available,
                    "loaded_models": loaded,
                    "operation": ops.get(&i).map(|op| json!({
                        "action": op.action.label(),
                        "model": op.canonical,
                        "elapsed_secs": op.started.elapsed().as_secs(),
                    })),
                })
            })
            .collect()
    };

    let logs: Vec<Value> = state
        .logs
        .lock_or_recover()
        .iter()
        .rev()
        .take(LOG_LIMIT)
        .map(|e| {
            json!({
                "ts": e.at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
                "dir": e.dir,
                "user": e.user,
                "model": e.model,
                "backend": e.backend,
                "info": e.info,
            })
        })
        .collect();

    Json(json!({
        "users": users,
        "backends": backends,
        "logs": logs,
        "blocked_ips": blocked_ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
    }))
}

/// `POST /admin/users/{id}/{action}` — `vip`, `boost`, `block` or `unblock`,
/// with the same semantics as the TUI keys (`p`, `b`, `x`, `u`): VIP and boost
/// toggle and are mutually exclusive.
pub async fn admin_user_action(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(String, String)>,
) -> impl IntoResponse {
    match action.as_str() {
        "vip" => state.toggle_vip(&id),
        "boost" => state.toggle_boost(&id),
        "block" => state.block_user(id),
        "unblock" => state.unblock_user(&id),
        _ => return (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /admin/ips/{ip}/{block|unblock}` — the TUI's `X` and `u` on an IP.
pub async fn admin_ip_action(
    State(state): State<Arc<AppState>>,
    Path((ip, action)): Path<(String, String)>,
) -> impl IntoResponse {
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return (StatusCode::BAD_REQUEST, "invalid IP address").into_response();
    };
    match action.as_str() {
        "block" => state.block_ip(ip),
        "unblock" => state.unblock_ip(ip),
        _ => return (StatusCode::NOT_FOUND, "unknown action").into_response(),
    }
    StatusCode::NO_CONTENT.into_response()
}
