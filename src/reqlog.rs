//! Request/response content logging to a size-rotated JSONL file.
//!
//! Producers call [`RequestLogger::log`] with a [`ReqRecord`]; records are
//! handed over a bounded channel to a dedicated writer task that appends one
//! JSON line per record and rotates the file when it would grow past
//! `max_bytes` (current file becomes `{path}.0`, older rotations shift up, at
//! most `max_files` rotated files kept). The hot path never blocks: if the
//! channel is full, records are dropped and counted instead.

use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::warn;

/// One line of the request log file: a single inbound ("IN") or outbound
/// ("OUT") request/response with its (possibly truncated) content.
#[derive(Debug, Clone, Serialize)]
pub struct ReqRecord {
    /// Unix time in milliseconds.
    pub ts: u64,
    /// "IN" | "OUT".
    pub dir: &'static str,
    pub user: String,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    /// Total request/response body bytes.
    pub bytes: Option<u64>,
    pub content_type: Option<String>,
    /// Body content, truncated to the configured limit (lossy UTF-8).
    /// Shared (not copied) with the TUI log event built from the same body.
    pub content: Option<Arc<String>>,
}

/// Non-blocking producer for the size-rotated request log.
pub struct RequestLogger {
    tx: mpsc::Sender<ReqRecord>,
    /// True when logging is disabled (e.g., the file could not be opened);
    /// `.log()` then returns immediately without touching the channel.
    disabled: bool,
    dropped: AtomicU64,
}

/// Channel capacity between producers and the writer task.
const CHANNEL_CAPACITY: usize = 10_000;

impl RequestLogger {
    /// Start a dedicated writer task for `path` with size-based rotation.
    ///
    /// The initial open is done synchronously so a bad path or permission
    /// error surfaces here (callers fall back to [`Self::disabled`]); all
    /// subsequent I/O happens in the writer task via `tokio::fs`, so no
    /// runtime thread pool slot is ever blocked on disk during operation.
    pub fn start(
        path: String,
        max_bytes: u64,
        max_files: usize,
    ) -> Result<(Self, tokio::task::JoinHandle<()>), std::io::Error> {
        // Fail fast on unwritable paths; also creates the file so restarts
        // continue sizing from its existing length.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let (tx, rx) = mpsc::channel::<ReqRecord>(CHANNEL_CAPACITY);
        let handle = tokio::spawn(writer_loop(path, max_bytes, max_files.max(1), rx));
        Ok((Self { tx, disabled: false, dropped: AtomicU64::new(0) }, handle))
    }

    /// No-op logger used when `start` fails; `.log()` returns immediately.
    pub fn disabled() -> Self {
        let (tx, _rx) = mpsc::channel::<ReqRecord>(1);
        drop(_rx); // disconnect so any accidental send fails fast
        Self {
            tx,
            disabled: true,
            dropped: AtomicU64::new(0),
        }
    }

    /// Non-blocking log of one record. Drops (and counts) the record when the
    /// channel is full; warns once per 1024 drops.
    pub fn log(&self, rec: ReqRecord) {
        if self.disabled {
            return;
        }
        if self.tx.try_send(rec).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(1024) {
                warn!(dropped = n, "request log channel full: dropped {} records so far", n);
            }
        }
    }

    /// Number of records dropped because the channel was full.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Current unix time in milliseconds (0 when the system clock is unusable).
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncate `data` to at most `limit` bytes on a UTF-8 boundary. If the data
/// fits, it is returned lossily; otherwise the first valid prefix is kept and
/// a truncation marker with the original total size is appended. Never panics
/// on invalid UTF-8 input.
pub fn truncate_utf8(data: &[u8], limit: usize) -> String {
    if data.len() <= limit {
        return String::from_utf8_lossy(data).into_owned();
    }
    let mut end = limit;
    while end > 0 {
        match std::str::from_utf8(&data[..end]) {
            Ok(s) => return format!("{}\n...[truncated: {} bytes total]", s, data.len()),
            Err(e) => end = e.valid_up_to(),
        }
    }
    format!("\n...[truncated: {} bytes total]", data.len())
}

/// Open `path` for appending, creating it when missing.
async fn open_append(path: &str) -> Result<tokio::fs::File, std::io::Error> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
}

/// Writer task: appends one JSON line per record and rotates the file when it
/// would exceed `max_bytes`. All I/O is async (`tokio::fs`).
async fn writer_loop(
    path: String,
    max_bytes: u64,
    max_files: usize,
    mut rx: mpsc::Receiver<ReqRecord>,
) {
    // Continue sizing from an existing file so restarts don't reset rotation.
    let mut current_size = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    // The append handle stays open across records: re-opening per line would
    // cost an open/close syscall pair for every logged request. It is dropped
    // (and re-opened on the next record) after a rotation or a failed write,
    // so a rotated-away or externally removed file is picked up again.
    let mut file: Option<tokio::fs::File> = None;

    while let Some(rec) = rx.recv().await {
        let line = match serde_json::to_string(&rec) {
            Ok(mut l) => {
                l.push('\n');
                l
            }
            Err(e) => {
                warn!("request log: failed to serialize record: {}", e);
                continue;
            }
        };

        if current_size + line.len() as u64 > max_bytes {
            // The open handle points at the file that rotation renames away.
            file = None;
            rotate(&path, max_files).await;
            current_size = 0;
        }

        if file.is_none() {
            match open_append(&path).await {
                Ok(f) => file = Some(f),
                Err(e) => {
                    warn!("request log: failed to open {} for append: {}", path, e);
                    continue;
                }
            }
        }
        let Some(f) = file.as_mut() else { continue };

        // `tokio::fs::File` buffers internally and only guarantees the bytes
        // reached the OS once flushed, so both calls are needed for anything
        // tailing the file to see complete lines.
        let written = match f.write_all(line.as_bytes()).await {
            Ok(()) => f.flush().await,
            Err(e) => Err(e),
        };
        match written {
            Ok(()) => current_size += line.len() as u64,
            Err(e) => {
                warn!("request log: failed to append to {}: {}", path, e);
                file = None; // re-open on the next record
            }
        }
    }
}

/// Shift the rotation chain: `{path}.{max_files-1}` is deleted, each existing
/// `{path}.{i}` moves up one slot (missing files skipped), and the current
/// file becomes `{path}.0`.
async fn rotate(path: &str, max_files: usize) {
    let oldest = format!("{}.{}", path, max_files - 1);
    let _ = tokio::fs::remove_file(&oldest).await;

    for i in (0..max_files.saturating_sub(1)).rev() {
        let from = format!("{}.{}", path, i);
        if tokio::fs::metadata(&from).await.is_ok() {
            let to = format!("{}.{}", path, i + 1);
            let _ = tokio::fs::rename(&from, &to).await;
        }
    }

    if tokio::fs::metadata(path).await.is_ok() {
        let _ = tokio::fs::rename(path, format!("{}.0", path)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: usize) -> ReqRecord {
        ReqRecord {
            ts: 1_700_000_000_000 + i as u64,
            dir: "IN",
            user: format!("user{}", i % 3),
            model: Some(format!("model-{}", i)),
            backend: None,
            method: "POST".into(),
            path: "/api/chat".into(),
            status: None,
            bytes: Some(100),
            content_type: Some("application/json".into()),
            // Padding keeps each serialized line in the ~100-250 byte range.
            content: Some(Arc::new(format!("x{}", "y".repeat(20)))),
        }
    }

    fn file_size(path: &str) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    #[tokio::test]
    async fn rotation_respects_max_bytes_and_file_count() {
        let dir = std::env::temp_dir().join(format!("ollamamq_reqlog_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("reqlog.jsonl");
        let path_str = path.to_string_lossy().into_owned();
        // Clean leftovers from a previous run of the same process.
        for i in 0..4 {
            let _ = tokio::fs::remove_file(format!("{}.{}", path_str, i)).await;
        }
        let _ = tokio::fs::remove_file(&path_str).await;

        const MAX_BYTES: u64 = 500;
        const MAX_FILES: usize = 2;
        let (logger, handle) = RequestLogger::start(path_str.clone(), MAX_BYTES, MAX_FILES).unwrap();

        for i in 0..30 {
            logger.log(rec(i));
        }

        // The writer appends lines strictly in order; once the last record's
        // line is at the end of the current file, every record has flushed.
        let expected_last = serde_json::to_string(&rec(29)).unwrap();
        let mut settled = false;
        for _ in 0..250 {
            if let Ok(content) = tokio::fs::read_to_string(&path_str).await
                && content.lines().last() == Some(expected_last.as_str())
            {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(settled, "writer did not flush all 30 records in time");

        // Current file stays within the size budget.
        let cur = file_size(&path_str);
        assert!(cur <= MAX_BYTES, "current file {}B exceeds max_bytes", cur);

        // Both rotated files exist after enough data flowed through...
        assert!(file_size(&format!("{}.0", path_str)) > 0, ".0 missing or empty");
        assert!(file_size(&format!("{}.1", path_str)) > 0, ".1 missing or empty");
        // ...and never more than max_files rotated files exist.
        for i in MAX_FILES..MAX_FILES + 3 {
            assert_eq!(
                file_size(&format!("{}.{}", path_str, i)),
                0,
                "unexpected .{} rotation file",
                i
            );
        }

        // Every surviving line is a valid JSON record with the expected shape.
        for f in [
            path_str.clone(),
            format!("{}.0", path_str),
            format!("{}.1", path_str),
        ] {
            let content = tokio::fs::read_to_string(&f).await.unwrap();
            assert!(!content.is_empty(), "{} is empty", f);
            for line in content.lines() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_eq!(v["dir"], "IN");
                assert_eq!(v["method"], "POST");
            }
        }

        handle.abort();
    }

    #[test]
    fn shared_content_serializes_as_a_plain_string() {
        // `content` is an `Arc<String>` so the body preview can be shared with
        // the TUI log event instead of copied. Serde's `rc` feature makes that
        // transparent on the wire — the log file format must not change.
        let mut r = rec(0);
        r.content = Some(Arc::new("hello \"world\"\nsecond line".to_string()));
        let line = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["content"], "hello \"world\"\nsecond line");

        r.content = None;
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(v["content"].is_null());
    }

    #[test]
    fn truncate_utf8_boundaries() {
        // ASCII under the limit is returned unchanged.
        assert_eq!(truncate_utf8(b"hello", 10), "hello");
        // Exact fit.
        assert_eq!(truncate_utf8(b"hello", 5), "hello");
        // ASCII over the limit keeps the prefix plus a marker with total size.
        let s = truncate_utf8(b"hello world", 5);
        assert_eq!(s, "hello\n...[truncated: 11 bytes total]");

        // Multibyte char split at the boundary: no panic, valid UTF-8 output.
        let data = "héllo".as_bytes(); // h + é (2 bytes) + llo → 6 bytes
        let s = truncate_utf8(data, 2);
        assert_eq!(s, "h\n...[truncated: 6 bytes total]");
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());

        // Limit landing exactly on a character boundary.
        let data = "hé".as_bytes(); // h (1) + é (2) = 3 bytes
        assert_eq!(truncate_utf8(data, 2), "h\n...[truncated: 3 bytes total]");
        assert_eq!(truncate_utf8(data, 4), "hé");

        // CJK split.
        let data = "a中b".as_bytes(); // a + 中 (3 bytes) + b → 5 bytes
        assert_eq!(truncate_utf8(data, 2), "a\n...[truncated: 5 bytes total]");

        // Exhaustive: every limit on mixed content yields valid UTF-8.
        let data = "aé中x".as_bytes();
        for limit in 0..=data.len() + 2 {
            let s = truncate_utf8(data, limit);
            assert!(
                std::str::from_utf8(s.as_bytes()).is_ok(),
                "invalid UTF-8 at limit={}",
                limit
            );
        }
    }

    #[test]
    fn disabled_logger_is_a_noop() {
        let logger = RequestLogger::disabled();
        for i in 0..5 {
            logger.log(rec(i));
        }
        assert_eq!(logger.dropped_count(), 0); // no-op: nothing counted
    }

    #[test]
    fn start_fails_on_unwritable_path() {
        let err = RequestLogger::start("/nonexistent-dir-ollamamq/reqlog.jsonl".into(), 1024, 2);
        assert!(err.is_err());
    }
}
