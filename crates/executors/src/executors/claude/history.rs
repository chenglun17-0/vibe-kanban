//! Claude Code native session history provider.
//!
//! Reads `~/.claude/projects/<cwd-slug>/<session-id>.jsonl` — the session
//! transcript Claude Code persists — and converts the final records into
//! normalized entries. Entry/tool mapping reuses the live normalizer's
//! `ClaudeLogProcessor` helpers so history and live views agree on rendering.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use super::{ClaudeContentItem, ClaudeLogProcessor};
use crate::{
    executors::BaseCodingAgent,
    history::{
        FileFingerprint, NativeHistoryError, NativeSessionHistory, NativeSessionHistoryProvider,
        TailPolicy, read_native_jsonl,
    },
    logs::{
        ActionType, CommandRunResult, NormalizedEntry, NormalizedEntryType, ToolResult,
        ToolResultValueType, ToolStatus,
    },
};

#[derive(Debug)]
pub struct ClaudeNativeSessionHistory;

fn projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude/projects"))
}

/// Records in a Claude session file carry `sessionId`; a file only "contains"
/// the session if at least one record references it.
fn file_contains_session(path: &Path, session_id: &str) -> Result<bool, NativeHistoryError> {
    let content = read_native_jsonl(path, TailPolicy::Running)?;
    for line in &content.lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("sessionId").and_then(Value::as_str) == Some(session_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[async_trait]
impl NativeSessionHistoryProvider for ClaudeNativeSessionHistory {
    fn agent(&self) -> BaseCodingAgent {
        BaseCodingAgent::ClaudeCode
    }

    async fn locate(&self, session_id: &str, _cwd: &Path) -> Result<PathBuf, NativeHistoryError> {
        let root = projects_root().ok_or_else(|| {
            NativeHistoryError::FormatUnsupported(
                "cannot determine Claude projects root (no home directory)".to_string(),
            )
        })?;
        let file_name = format!("{session_id}.jsonl");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Err(NativeHistoryError::FileNotFound {
                session_id: session_id.to_string(),
                root,
            });
        };
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            let candidate = project_dir.join(&file_name);
            if candidate.is_file() {
                return match file_contains_session(&candidate, session_id) {
                    Ok(true) => Ok(candidate),
                    Ok(false) => Err(NativeHistoryError::Corrupt {
                        path: candidate,
                        reason: "filename matches but no record references the session id"
                            .to_string(),
                    }),
                    Err(err) => Err(err),
                };
            }
        }
        Err(NativeHistoryError::FileNotFound {
            session_id: session_id.to_string(),
            root,
        })
    }

    async fn read(
        &self,
        path: &Path,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError> {
        let content = read_native_jsonl(path, tail)?;
        let fingerprint = FileFingerprint::of(path)?;

        // Filter first, then select the main chain by walking `parentUuid`
        // links from the leaf (the last non-sidechain, non-meta record — the
        // rule locked by the plan). Branches abandoned by edit/retry are not
        // on the chain and never reach the transcript.
        let mut session_id: Option<String> = None;
        let mut records: Vec<Value> = Vec::new();
        for (line_no, line) in content.lines.iter().enumerate() {
            let value: Value =
                serde_json::from_str(line).map_err(|e| NativeHistoryError::Corrupt {
                    path: path.to_path_buf(),
                    reason: format!("line {} is not valid JSON: {e}", line_no + 1),
                })?;

            let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            if record_type != "user" && record_type != "assistant" {
                continue;
            }
            // Sidechain (subagent), meta, and replay records are not the main transcript.
            if value.get("isSidechain").and_then(Value::as_bool) == Some(true)
                || value.get("isMeta").and_then(Value::as_bool) == Some(true)
                || value.get("isReplay").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            if session_id.is_none()
                && let Some(id) = value.get("sessionId").and_then(Value::as_str)
            {
                session_id = Some(id.to_string());
            }
            records.push(value);
        }

        let session_id = session_id.ok_or_else(|| NativeHistoryError::Corrupt {
            path: path.to_path_buf(),
            reason: "no user/assistant record carries a sessionId".to_string(),
        })?;

        let chain = main_chain(&records);
        let mut out: Vec<NormalizedEntry> = Vec::new();
        // tool_use_id -> position in `out`
        let mut tool_slots: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut last_assistant_message: Option<String> = None;

        for value in &chain {
            let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let worktree = value.get("cwd").and_then(Value::as_str).unwrap_or("");
            let Some(message) = value.get("message") else {
                continue;
            };

            match message.get("content") {
                Some(Value::String(text)) => {
                    if record_type == "user" && !text.is_empty() {
                        out.push(NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::UserMessage,
                            content: text.clone(),
                            metadata: None,
                        });
                    }
                }
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        map_content_block(
                            block,
                            record_type,
                            worktree,
                            &mut out,
                            &mut tool_slots,
                            &mut last_assistant_message,
                        );
                    }
                }
                _ => {}
            }
        }

        // Tool uses without results never completed.
        for position in tool_slots.values() {
            if let Some(entry) = out.get_mut(*position)
                && let Some(updated) = entry.with_tool_status(ToolStatus::Failed)
            {
                *entry = updated;
            }
        }

        Ok(NativeSessionHistory {
            agent: BaseCodingAgent::ClaudeCode,
            session_id,
            entries: out,
            fingerprint,
            partial_tail: content.partial_tail,
        })
    }
}

/// Select the main transcript chain: start from the leaf (the last filtered
/// record) and walk `parentUuid` links to the root. Records off the chain —
/// abandoned edit/retry branches — are excluded. Falls back to file order
/// when no `uuid` links exist (older files).
fn main_chain(records: &[Value]) -> Vec<Value> {
    let by_uuid: std::collections::HashMap<&str, &Value> = records
        .iter()
        .filter_map(|record| Some((record.get("uuid")?.as_str()?, record)))
        .collect();
    if by_uuid.is_empty() {
        return records.to_vec();
    }

    let Some(mut cursor) = records
        .iter()
        .rev()
        .find(|record| record.get("uuid").and_then(Value::as_str).is_some())
    else {
        return records.to_vec();
    };

    let mut chain = vec![cursor.clone()];
    while let Some(parent_id) = cursor.get("parentUuid").and_then(Value::as_str) {
        let Some(parent) = by_uuid.get(parent_id) else {
            break;
        };
        chain.push((*parent).clone());
        cursor = parent;
    }
    chain.reverse();
    chain
}

fn map_content_block(
    block: &Value,
    role: &str,
    worktree: &str,
    out: &mut Vec<NormalizedEntry>,
    tool_slots: &mut std::collections::HashMap<String, usize>,
    last_assistant_message: &mut Option<String>,
) {
    let Ok(item) = serde_json::from_value::<ClaudeContentItem>(block.clone()) else {
        return;
    };
    match &item {
        ClaudeContentItem::Text { text } if role == "user" => {
            if !text.is_empty() {
                out.push(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::UserMessage,
                    content: text.clone(),
                    metadata: None,
                });
            }
        }
        ClaudeContentItem::ToolUse { id, .. } => {
            if let Some(entry) = ClaudeLogProcessor::content_item_to_normalized_entry(
                &item,
                role,
                worktree,
                last_assistant_message,
            ) {
                out.push(entry);
                tool_slots.insert(id.clone(), out.len() - 1);
            }
        }
        ClaudeContentItem::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            apply_tool_result(out, tool_slots, tool_use_id, content, *is_error);
        }
        _ => {
            // Text (assistant) and Thinking reuse the live mapping verbatim.
            if let Some(entry) = ClaudeLogProcessor::content_item_to_normalized_entry(
                &item,
                role,
                worktree,
                last_assistant_message,
            ) {
                out.push(entry);
            }
        }
    }
}

fn apply_tool_result(
    out: &mut [NormalizedEntry],
    tool_slots: &mut std::collections::HashMap<String, usize>,
    tool_use_id: &str,
    content: &Value,
    is_error: Option<bool>,
) {
    let Some(position) = tool_slots.remove(tool_use_id) else {
        return;
    };
    let (value_type, value) = ClaudeLogProcessor::normalize_claude_tool_result_value(content);
    let result_text = match value_type {
        ToolResultValueType::Markdown => value.as_str().unwrap_or_default().to_string(),
        ToolResultValueType::Json => serde_json::to_string_pretty(&value).unwrap_or_default(),
    };
    let status = if is_error == Some(true) {
        ToolStatus::Failed
    } else {
        ToolStatus::Success
    };

    let Some(entry) = out.get_mut(position) else {
        return;
    };
    let NormalizedEntryType::ToolUse { action_type, .. } = &mut entry.entry_type else {
        return;
    };
    match action_type {
        ActionType::CommandRun { result, .. } => {
            *result = Some(CommandRunResult {
                exit_status: None,
                output: Some(result_text),
            });
        }
        ActionType::Tool { result, .. } | ActionType::TaskCreate { result, .. } => {
            *result = Some(ToolResult {
                r#type: value_type,
                value,
            });
        }
        _ => {}
    }
    if let Some(updated) = entry.with_tool_status(status) {
        *entry = updated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::NativeHistoryErrorCode;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native_history/claude")
            .join(name)
    }

    async fn read_fixture(
        name: &str,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError> {
        ClaudeNativeSessionHistory.read(&fixture(name), tail).await
    }

    #[tokio::test]
    async fn native_session_history_claude_maps_main_transcript() {
        let history = read_fixture("basic_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        assert_eq!(history.session_id, "33333333-4444-5555-6666-777777777777");

        let kinds: Vec<&'static str> = history
            .entries
            .iter()
            .map(|entry| match &entry.entry_type {
                NormalizedEntryType::UserMessage => "user",
                NormalizedEntryType::AssistantMessage => "assistant",
                NormalizedEntryType::Thinking => "thinking",
                NormalizedEntryType::ToolUse { .. } => "tool",
                _ => "other",
            })
            .collect();
        // queue-operation/meta/sidechain records are excluded.
        assert_eq!(kinds, vec!["user", "thinking", "tool", "assistant"]);

        let NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } = &history.entries[2].entry_type
        else {
            panic!("expected tool use entry");
        };
        assert_eq!(tool_name, "Bash");
        assert!(matches!(status, ToolStatus::Success));
        let ActionType::CommandRun { command, result } = action_type else {
            panic!("expected command run");
        };
        assert_eq!(command, "ls docs");
        assert!(
            result
                .as_ref()
                .and_then(|r| r.output.as_deref())
                .is_some_and(|output| output.contains("README.md"))
        );
    }

    #[tokio::test]
    async fn native_session_history_claude_is_deterministic() {
        let a = read_fixture("basic_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        let b = read_fixture("basic_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        let to_json = |h: &NativeSessionHistory| {
            h.entries
                .iter()
                .map(|entry| serde_json::to_value(&entry.entry_type).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(to_json(&a), to_json(&b));
    }

    #[tokio::test]
    async fn native_session_history_claude_follows_main_chain() {
        let history = read_fixture("branched_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        let texts: Vec<&str> = history
            .entries
            .iter()
            .map(|entry| entry.content.as_str())
            .collect();
        // Leaf is the last filtered record (a2), so the chain is u1b -> a2;
        // the abandoned u1 -> a1 branch never appears.
        assert_eq!(texts, vec!["编辑后的请求", "当前主链的回答"]);
    }

    #[tokio::test]
    async fn native_session_history_claude_tail_policies() {
        let history = read_fixture("trailing_partial.jsonl", TailPolicy::Running)
            .await
            .unwrap();
        assert!(history.partial_tail);

        let err = read_fixture("trailing_partial.jsonl", TailPolicy::Completed)
            .await
            .unwrap_err();
        assert_eq!(err.code(), NativeHistoryErrorCode::NativeSessionCorrupt);
    }
}
