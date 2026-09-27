//! Central configuration file (`appconf.yaml`, override with
//! `-c/--model-config`). Three sections:
//!
//! * `backends` — backend URLs ollamaMQ connects to (same as
//!   `--backend-urls`; CLI wins when given).
//! * `settings` — runtime settings; see [`Settings`] for the full list and
//!   the default each one falls back to. CLI flags win when given.
//! * `models` — models to load onto backends via the model-control
//!   load/unload logic, with per-model parameters.
//!
//! ```yaml
//! backends:
//!   - http://10.137.1.1:11434          # plain URL — no auth
//!   - url: http://10.137.1.2:11434     # object form with an optional token,
//!     token: "sekrit"                  # sent as `Authorization: Bearer <token>`
//!
//! settings:
//!   port: 11435
//!   timeout: 300
//!   load_keep_alive: 86400
//!
//! models:
//!   - name: "gpt-oss:120b"
//!     identifier: "my-gpt"
//!     max_ctx: 128000
//!     max_concurrent_requests: 3
//!     backends:
//!       - http://10.137.1.1:11434
//! ```

use serde::{Deserialize, Serialize};
use std::fs;

pub const DEFAULT_CONFIG_FILE: &str = "appconf.yaml";

/// Runtime settings (all optional; CLI flags take precedence).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Port to listen on (default 11435).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Host/interface to bind to (default 127.0.0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request timeout in seconds (default 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// `keep_alive` seconds sent with model-control loads (default 86400).
    /// `-1` keeps the model loaded indefinitely (Ollama semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_keep_alive: Option<i64>,
    /// Enable fallback proxy for non-standard endpoints (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_routes: Option<bool>,
    /// How long (seconds) a request may wait when no backend can ever serve
    /// it before answering 503 (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_timeout: Option<u64>,
    /// Max simultaneous in-flight requests per backend (default 1 — the
    /// historical one-request-per-backend behavior). Per-model sub-limits come
    /// from each model entry's `max_concurrent_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_per_backend: Option<u32>,
    /// Path of the size-rotated request log file (default
    /// "ollamamq-requests.jsonl" in the working directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_path: Option<String>,
    /// Max bytes per request-log file before rotation (default 10 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_max_bytes: Option<u64>,
    /// Number of rotated request-log files to keep (default 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_max_files: Option<usize>,
    /// Max bytes of request/response body content captured per record in the
    /// request log; values below 1024 are raised to 1024 (default 65536).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_content_limit: Option<usize>,
    /// Largest request body accepted on normal routes, in bytes (default
    /// 64 MiB). Bodies are buffered whole — they have to be, since a request
    /// may wait in a queue before a backend takes it — so this is the per
    /// request memory commitment. `/api/blobs/{digest}` keeps its own 1 GiB
    /// allowance for model-layer uploads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    /// Max total bytes of request bodies waiting in all queues at once
    /// (default 512 MiB). Past this the proxy answers 503 instead of
    /// accumulating; a request always gets in when the queues are empty, so
    /// one oversized body can never deadlock the proxy against itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_queued_bytes: Option<u64>,
}

/// One backend entry from the `backends:` list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backend {
    /// The backend's base URL (scheme included).
    pub url: String,
    /// Optional API key sent to this backend as
    /// `Authorization: Bearer <token>` on every request the proxy makes to
    /// it — proxied inference, model listings and metadata reads, health
    /// probes, and model load/unload control ops. A client's own auth header
    /// is never overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Pin this backend's API family instead of waiting for the health probe to
    /// classify it. `systemone` is the only meaningful value today: Laya-style
    /// decision-model servers expose no model-listing endpoint at all, so no
    /// probe can positively identify them, and an unclassified family makes
    /// every request to that backend unroutable. A pinned family is fixed for
    /// the process lifetime — probes never talk it out of existence.
    ///
    /// Absent, empty or unrecognised = classify from probes (the behaviour
    /// every existing config relies on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// YAML shape of one `backends:` entry: either a bare URL string or an object
/// with `url` and optional `token`/`kind`. Both forms are accepted so existing
/// configs keep working unchanged.
#[derive(Deserialize)]
#[serde(untagged)]
enum BackendEntrySpec {
    Url(String),
    Full(Backend),
}

fn de_backends<'de, D>(d: D) -> Result<Vec<Backend>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let specs = Vec::<BackendEntrySpec>::deserialize(d)?;
    Ok(specs
        .into_iter()
        .map(|spec| match spec {
            BackendEntrySpec::Url(url) => Backend {
                url,
                token: None,
                kind: None,
            },
            BackendEntrySpec::Full(b) => b,
        })
        .collect())
}

/// Top-level config file structure.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Backends to connect to (CLI `--backend-urls` wins when given).
    #[serde(
        default,
        deserialize_with = "de_backends",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub backends: Vec<Backend>,
    /// Runtime settings.
    #[serde(default)]
    pub settings: Settings,
    /// Models to load onto backends.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// One model entry from `appconf.yaml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model name as the backend knows it.
    pub name: String,
    /// Label attached to the load operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Max context window (Ollama `num_ctx` / LM Studio `context_length`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u64>,
    /// `keep_alive` override in seconds (Ollama loads). `-1` keeps the
    /// model loaded indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<i64>,
    /// Max simultaneous in-flight requests for this model on one backend
    /// (default 1). Enforced by the scheduler; also bounded by the global
    /// `settings.max_concurrent_per_backend` cap.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub max_concurrent_requests: u32,
    /// Backend URLs to load this model on. Empty = any suitable backend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
}

fn one() -> u32 {
    1
}

fn is_one(v: &u32) -> bool {
    *v == 1
}

/// Load and parse the config file. Returns an error string suitable for
/// user-facing display (missing file, invalid YAML, etc.). Validates that
/// keep-alive values are `-1` (keep loaded indefinitely) or non-negative.
pub fn load_config(path: &str) -> Result<AppConfig, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;
    let cfg: AppConfig =
        serde_yaml::from_str(&content).map_err(|e| format!("invalid YAML in '{}': {}", path, e))?;

    let check = |v: i64, what: &str| -> Result<(), String> {
        if v < -1 {
            Err(format!(
                "invalid {} in '{}': {} (use -1 to keep the model loaded indefinitely)",
                what, path, v
            ))
        } else {
            Ok(())
        }
    };
    if let Some(v) = cfg.settings.load_keep_alive {
        check(v, "settings.load_keep_alive")?;
    }
    for m in &cfg.models {
        if let Some(v) = m.keep_alive {
            check(v, &format!("keep_alive of model '{}'", m.name))?;
        }
    }

    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let yaml = r#"
backends:
  - http://10.137.1.1:11434
  - http://10.137.1.2:11434

settings:
  port: 11500
  host: 0.0.0.0
  timeout: 120
  load_keep_alive: 3600
  allow_all_routes: true

models:
  - name: "gpt-oss:120b"
    identifier: "my-gpt"
    max_ctx: 128000
    keep_alive: 3600
    max_concurrent_requests: 3
    backends:
      - http://10.137.1.1:11434
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_full.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 2);
        assert_eq!(cfg.settings.port, Some(11500));
        assert_eq!(cfg.settings.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(cfg.settings.timeout, Some(120));
        assert_eq!(cfg.settings.load_keep_alive, Some(3600));
        assert_eq!(cfg.settings.allow_all_routes, Some(true));
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "gpt-oss:120b");
        assert_eq!(cfg.models[0].identifier.as_deref(), Some("my-gpt"));
        assert_eq!(cfg.models[0].max_ctx, Some(128000));
        assert_eq!(cfg.models[0].keep_alive, Some(3600));
        assert_eq!(cfg.models[0].max_concurrent_requests, 3);
        assert_eq!(cfg.models[0].backends.len(), 1);
    }

    #[test]
    fn parse_backends_plain_and_token_forms() {
        let yaml = r#"
backends:
  - http://10.137.1.1:11434
  - url: http://10.137.1.2:11434
    token: "sekrit"
  - url: http://10.137.1.5:1234
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_backends.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 3);
        // Bare string: no token.
        assert_eq!(cfg.backends[0].url, "http://10.137.1.1:11434");
        assert_eq!(cfg.backends[0].token, None);
        // Object with a token.
        assert_eq!(cfg.backends[1].url, "http://10.137.1.2:11434");
        assert_eq!(cfg.backends[1].token.as_deref(), Some("sekrit"));
        // Object without a token is valid too.
        assert_eq!(cfg.backends[2].url, "http://10.137.1.5:1234");
        assert_eq!(cfg.backends[2].token, None);
    }

    /// `kind:` pins a backend's API family — needed because Laya answers no
    /// endpoint that identifies it. Bare-string entries and object entries
    /// without the key keep parsing exactly as before, and an unrecognised
    /// value parses (the caller ignores it) rather than rejecting the file.
    #[test]
    fn parse_backend_kind_key() {
        let yaml = r#"
backends:
  - http://127.0.0.1:11434
  - url: http://127.0.0.1:8772
    kind: systemone
  - url: http://127.0.0.1:8700
    kind: "who-knows"
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_kind.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.backends.len(), 3);
        // Bare string and object-without-kind: unchanged behaviour.
        assert_eq!(cfg.backends[0].kind, None);
        assert_eq!(cfg.backends[2].kind.as_deref(), Some("who-knows"));
        assert_eq!(cfg.backends[1].kind.as_deref(), Some("systemone"));
        // The key is only serialised when set, so configs round-trip unchanged.
        assert!(
            !serde_yaml::to_string(&cfg.backends[0])
                .unwrap()
                .contains("kind")
        );
    }

    #[test]
    fn backend_object_without_url_is_rejected() {
        let yaml = "backends:\n  - token: \"sekrit\"\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_backend_bad.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        assert!(load_config(tmp.to_str().unwrap()).is_err());
    }

    #[test]
    fn parse_minimal_config_defaults() {
        let yaml = "models:\n  - name: llama3\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_min.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert!(cfg.backends.is_empty());
        assert_eq!(cfg.settings.port, None); // built-in default applies
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "llama3");
        assert_eq!(cfg.models[0].max_concurrent_requests, 1); // default
        assert!(cfg.models[0].backends.is_empty()); // any backend
        assert!(cfg.models[0].max_ctx.is_none());
        assert!(cfg.models[0].identifier.is_none());
    }

    #[test]
    fn empty_file_is_valid() {
        let tmp = std::env::temp_dir().join("ollamamq_test_empty.yaml");
        std::fs::write(&tmp, "").unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert!(cfg.backends.is_empty());
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn keep_alive_minus_one_is_infinite() {
        let yaml = r#"
settings:
  load_keep_alive: -1
models:
  - name: llama3
    keep_alive: -1
"#;
        let tmp = std::env::temp_dir().join("ollamamq_test_ka_neg.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        let cfg = load_config(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.settings.load_keep_alive, Some(-1));
        assert_eq!(cfg.models[0].keep_alive, Some(-1));
    }

    #[test]
    fn keep_alive_below_minus_one_is_rejected() {
        let yaml = "models:\n  - name: llama3\n    keep_alive: -5\n";
        let tmp = std::env::temp_dir().join("ollamamq_test_ka_bad.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        assert!(load_config(tmp.to_str().unwrap())
            .err()
            .unwrap()
            .contains("-5"));
    }

    #[test]
    fn missing_file_is_error() {
        assert!(load_config("/nonexistent/appconf.yaml").is_err());
    }
}
