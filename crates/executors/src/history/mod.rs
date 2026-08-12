//! Native session history providers.
//!
//! Completed conversations are read from each agent's own persisted session
//! files (PI session JSONL, Codex rollout JSONL, Claude Code project JSONL),
//! not by replaying raw stdout logs. Raw logs remain a diagnostic surface
//! only. See `docs/exec-plan/agents/native-agent-session-history.md`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

use crate::{executors::BaseCodingAgent, logs::NormalizedEntry};

/// Stable machine-readable error codes for native history failures.
///
/// These cross the API boundary; keep them snake_case and append-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeHistoryErrorCode {
    NativeSessionIdMissing,
    NativeSessionFileNotFound,
    NativeSessionPermissionDenied,
    NativeSessionFormatUnsupported,
    NativeSessionCorrupt,
    NativeSessionNotFlushed,
}

impl NativeHistoryErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NativeSessionIdMissing => "native_session_id_missing",
            Self::NativeSessionFileNotFound => "native_session_file_not_found",
            Self::NativeSessionPermissionDenied => "native_session_permission_denied",
            Self::NativeSessionFormatUnsupported => "native_session_format_unsupported",
            Self::NativeSessionCorrupt => "native_session_corrupt",
            Self::NativeSessionNotFlushed => "native_session_not_flushed",
        }
    }
}

#[derive(Debug, Error)]
pub enum NativeHistoryError {
    #[error("no agent session id recorded for this session")]
    SessionIdMissing,
    #[error("native session file for session `{session_id}` not found under {}", root.display())]
    FileNotFound { session_id: String, root: PathBuf },
    #[error("permission denied reading native session file {}", path.display())]
    PermissionDenied { path: PathBuf },
    #[error("unsupported native session format: {0}")]
    FormatUnsupported(String),
    #[error("corrupt native session file {}: {reason}", path.display())]
    Corrupt { path: PathBuf, reason: String },
    #[error("agent has not finished persisting session `{0}`")]
    NotFlushed(String),
}

impl NativeHistoryError {
    pub fn code(&self) -> NativeHistoryErrorCode {
        match self {
            Self::SessionIdMissing => NativeHistoryErrorCode::NativeSessionIdMissing,
            Self::FileNotFound { .. } => NativeHistoryErrorCode::NativeSessionFileNotFound,
            Self::PermissionDenied { .. } => NativeHistoryErrorCode::NativeSessionPermissionDenied,
            Self::FormatUnsupported(_) => NativeHistoryErrorCode::NativeSessionFormatUnsupported,
            Self::Corrupt { .. } => NativeHistoryErrorCode::NativeSessionCorrupt,
            Self::NotFlushed(_) => NativeHistoryErrorCode::NativeSessionNotFlushed,
        }
    }

    /// Only `not_flushed` is worth retrying: every other failure is deterministic.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::NotFlushed(_))
    }

    pub(crate) fn from_io(error: std::io::Error, path: &Path) -> Self {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied {
                path: path.to_path_buf(),
            },
            _ => Self::Corrupt {
                path: path.to_path_buf(),
                reason: error.to_string(),
            },
        }
    }
}

/// Whether the producing process is still running. A trailing partial JSONL
/// line means "not flushed yet" while running, but "corrupt" once completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPolicy {
    Running,
    Completed,
}

/// Cheap change detector for cache invalidation: no content hashing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileFingerprint {
    pub path: PathBuf,
    pub len: u64,
    /// Milliseconds since Unix epoch; `None` when the platform has no mtime.
    pub modified_ms: Option<i64>,
}

impl FileFingerprint {
    pub fn of(path: &Path) -> Result<Self, NativeHistoryError> {
        let metadata = std::fs::metadata(path).map_err(|e| NativeHistoryError::from_io(e, path))?;
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        Ok(Self {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified_ms,
        })
    }
}

/// Final, materialized conversation entries converted from a native session file.
#[derive(Debug)]
pub struct NativeSessionHistory {
    pub agent: BaseCodingAgent,
    pub session_id: String,
    pub entries: Vec<NormalizedEntry>,
    pub fingerprint: FileFingerprint,
    /// The file ended mid-line and the tail was dropped (only possible when
    /// reading with `TailPolicy::Running`).
    pub partial_tail: bool,
}

/// One provider per supported agent. Implementations live in the executor's
/// own submodule; this trait is the only dependency the service layer uses.
#[async_trait]
pub trait NativeSessionHistoryProvider: Send + Sync + std::fmt::Debug {
    fn agent(&self) -> BaseCodingAgent;

    /// Locate the native file for `session_id`, verifying file contents match
    /// the requested session (never trust the filename alone).
    async fn locate(&self, session_id: &str, cwd: &Path) -> Result<PathBuf, NativeHistoryError>;

    fn fingerprint(&self, path: &Path) -> Result<FileFingerprint, NativeHistoryError> {
        FileFingerprint::of(path)
    }

    /// Convert the native file into final normalized entries. Must be
    /// deterministic for a fixed file and must not modify the file.
    async fn read(
        &self,
        path: &Path,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError>;
}

/// Registry. Every supported agent has a provider; executor strings from old
/// databases fail `BaseCodingAgent::from_str` before reaching this function.
pub fn native_history_provider(agent: BaseCodingAgent) -> Box<dyn NativeSessionHistoryProvider> {
    match agent {
        BaseCodingAgent::Pi => Box::new(crate::executors::pi::history::PiNativeSessionHistory),
        BaseCodingAgent::Codex => Box::new(
            crate::executors::codex::history::CodexNativeSessionHistory,
        ),
        BaseCodingAgent::ClaudeCode => Box::new(
            crate::executors::claude::history::ClaudeNativeSessionHistory,
        ),
    }
}

#[derive(Debug)]
pub(crate) struct JsonlContent {
    pub lines: Vec<String>,
    pub partial_tail: bool,
}

/// Read a JSONL file once. A trailing unterminated line is dropped when
/// running, or rejected as corrupt when the process completed.
pub(crate) fn read_native_jsonl(
    path: &Path,
    tail: TailPolicy,
) -> Result<JsonlContent, NativeHistoryError> {
    let bytes = std::fs::read(path).map_err(|e| NativeHistoryError::from_io(e, path))?;
    let text = String::from_utf8(bytes).map_err(|_| NativeHistoryError::Corrupt {
        path: path.to_path_buf(),
        reason: "file is not valid UTF-8".to_string(),
    })?;

    let partial_tail = !text.is_empty() && !text.ends_with('\n');
    if partial_tail && tail == TailPolicy::Completed {
        return Err(NativeHistoryError::Corrupt {
            path: path.to_path_buf(),
            reason: "trailing partial JSONL line".to_string(),
        });
    }

    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if partial_tail {
        lines.pop();
    }
    Ok(JsonlContent {
        lines,
        partial_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_history")
    }

    #[test]
    fn native_session_history_error_codes_are_stable() {
        assert_eq!(
            NativeHistoryError::SessionIdMissing.code().as_str(),
            "native_session_id_missing"
        );
        assert_eq!(
            NativeHistoryError::FileNotFound {
                session_id: "s".into(),
                root: PathBuf::from("/x")
            }
            .code()
            .as_str(),
            "native_session_file_not_found"
        );
        assert!(NativeHistoryError::NotFlushed("s".into()).retryable());
        assert!(!NativeHistoryError::SessionIdMissing.retryable());
    }

    #[test]
    fn native_session_history_jsonl_tail_policies() {
        let path = fixture_dir().join("shared/trailing_partial.jsonl");

        let running = read_native_jsonl(&path, TailPolicy::Running).unwrap();
        assert!(running.partial_tail);
        assert_eq!(running.lines.len(), 2);

        let err = read_native_jsonl(&path, TailPolicy::Completed).unwrap_err();
        assert_eq!(err.code(), NativeHistoryErrorCode::NativeSessionCorrupt);
    }

    #[test]
    fn native_session_history_fingerprint_is_deterministic() {
        let path = fixture_dir().join("shared/trailing_partial.jsonl");
        let a = FileFingerprint::of(&path).unwrap();
        let b = FileFingerprint::of(&path).unwrap();
        assert_eq!(a, b);

        let path =
            std::env::temp_dir().join(format!("vk-fingerprint-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&path, "{\"type\":\"x\"}\n").unwrap();
        let before = FileFingerprint::of(&path).unwrap();
        std::fs::write(&path, "{\"type\":\"x\"}\n{\"type\":\"y\"}\n").unwrap();
        let after = FileFingerprint::of(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_ne!(before, after);
    }

}
