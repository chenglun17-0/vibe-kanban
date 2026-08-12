//! PI native session history provider.
//!
//! Reads `~/.pi/agent/sessions/**/<timestamp>_<session-id>.jsonl` — the
//! materialized session tree PI persists — instead of replaying the RPC
//! stdout stream. Linear sessions are mapped directly; sessions with
//! branches/compaction are resolved through PI's own SessionManager via a
//! Node helper (`session_helper.mjs`) so tree semantics are never reimplemented
//! here.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde_json::Value;

use super::normalize_logs::{display_tool_name, map_action, truncate_for_display};
use crate::{
    executors::BaseCodingAgent,
    history::{
        FileFingerprint, NativeHistoryError, NativeSessionHistory, NativeSessionHistoryProvider,
        TailPolicy, read_native_jsonl,
    },
    logs::{
        ActionType, CommandExitStatus, CommandRunResult, NormalizedEntry, NormalizedEntryType,
        ToolStatus,
    },
};

#[derive(Debug)]
pub struct PiNativeSessionHistory;

fn sessions_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".pi/agent/sessions"))
}

/// Recursive scan bounded to `depth`; PI nests sessions under per-project
/// directories and (for subagents) per-run subdirectories.
fn find_session_file(dir: &Path, session_id: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_session_file(&path, session_id, depth - 1) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(session_id) && name.ends_with(".jsonl"))
            && header_session_id(&path).as_deref() == Some(session_id)
        {
            return Some(path);
        }
    }
    None
}

fn header_session_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let value: Value = serde_json::from_str(first_line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    value.get("id").and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl NativeSessionHistoryProvider for PiNativeSessionHistory {
    fn agent(&self) -> BaseCodingAgent {
        BaseCodingAgent::Pi
    }

    async fn locate(&self, session_id: &str, _cwd: &Path) -> Result<PathBuf, NativeHistoryError> {
        let root = sessions_root().ok_or_else(|| {
            NativeHistoryError::FormatUnsupported(
                "cannot determine PI sessions root (no home directory)".to_string(),
            )
        })?;
        find_session_file(&root, session_id, 6).ok_or_else(|| NativeHistoryError::FileNotFound {
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

        let mut entries_raw: Vec<Value> = Vec::with_capacity(content.lines.len());
        let mut session_id: Option<String> = None;
        for (line_no, line) in content.lines.iter().enumerate() {
            let value: Value =
                serde_json::from_str(line).map_err(|e| NativeHistoryError::Corrupt {
                    path: path.to_path_buf(),
                    reason: format!("line {} is not valid JSON: {e}", line_no + 1),
                })?;
            if line_no == 0 {
                if value.get("type").and_then(Value::as_str) != Some("session") {
                    return Err(NativeHistoryError::Corrupt {
                        path: path.to_path_buf(),
                        reason: "first line is not a session header".to_string(),
                    });
                }
                session_id = value.get("id").and_then(Value::as_str).map(str::to_string);
                continue;
            }
            entries_raw.push(value);
        }
        let session_id = session_id.ok_or_else(|| NativeHistoryError::Corrupt {
            path: path.to_path_buf(),
            reason: "session header has no id".to_string(),
        })?;

        let entries = if has_tree_features(&entries_raw) {
            let resolved = resolve_active_branch(path).await?;
            map_messages(&resolved)
        } else {
            map_messages(&entries_raw)
        };

        Ok(NativeSessionHistory {
            agent: BaseCodingAgent::Pi,
            session_id,
            entries,
            fingerprint,
            partial_tail: content.partial_tail,
        })
    }
}

/// PI sessions are trees. Linear files map directly; anything with
/// branches or compaction goes through the SessionManager helper — we never
/// guess tree rules.
fn has_tree_features(entries: &[Value]) -> bool {
    let mut parents_with_children: HashSet<&str> = HashSet::new();
    entries.iter().any(|entry| {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        matches!(entry_type, "compaction" | "branch_summary")
            || entry
                .get("parentId")
                .and_then(Value::as_str)
                .is_some_and(|parent_id| !parents_with_children.insert(parent_id))
    })
}

const SESSION_HELPER_SOURCE: &str = include_str!("session_helper.mjs");

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file().then_some(candidate)
    })
}

/// Locate the installed PI package's ESM entry by resolving the `pi` binary's
/// symlink and walking up to its package directory.
fn pi_package_index() -> Option<PathBuf> {
    let pi_bin = std::fs::canonicalize(find_on_path("pi")?).ok()?;
    for ancestor in pi_bin.ancestors() {
        let manifest = ancestor.join("package.json");
        if !manifest.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if content.contains("@earendil-works/pi-coding-agent") {
            let index = ancestor.join("dist/index.js");
            if index.is_file() {
                return Some(index);
            }
        }
    }
    None
}

#[cfg(test)]
fn helper_available() -> bool {
    find_on_path("node").is_some() && pi_package_index().is_some()
}

/// Write the bundled helper to a managed temp file (mirrors
/// `Pi::materialize_bridge`). The file contains no secrets.
fn materialize_session_helper() -> Result<PathBuf, NativeHistoryError> {
    let dir = std::env::temp_dir().join("vibe-kanban");
    std::fs::create_dir_all(&dir).map_err(|e| NativeHistoryError::from_io(e, &dir))?;
    let path = dir.join(format!("pi-session-helper-{}.mjs", uuid::Uuid::new_v4()));
    std::fs::write(&path, SESSION_HELPER_SOURCE)
        .map_err(|e| NativeHistoryError::from_io(e, &path))?;
    Ok(path)
}

/// Resolve the active branch (compaction applied) through PI's SessionManager.
async fn resolve_active_branch(path: &Path) -> Result<Vec<Value>, NativeHistoryError> {
    let node = find_on_path("node").ok_or_else(|| {
        NativeHistoryError::FormatUnsupported(
            "node executable not found; cannot run the PI SessionManager helper".to_string(),
        )
    })?;
    let index = pi_package_index().ok_or_else(|| {
        NativeHistoryError::FormatUnsupported(
            "installed @earendil-works/pi-coding-agent package not found; cannot resolve branched/compacted sessions"
                .to_string(),
        )
    })?;
    let helper = materialize_session_helper()?;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(node)
            .arg(helper)
            .arg(index)
            .arg(path)
            .output(),
    )
    .await;
    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(NativeHistoryError::from_io(e, path)),
        Err(_) => {
            return Err(NativeHistoryError::FormatUnsupported(
                "PI SessionManager helper timed out".to_string(),
            ));
        }
    };
    if !output.status.success() {
        return Err(NativeHistoryError::FormatUnsupported(format!(
            "PI SessionManager helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| NativeHistoryError::Corrupt {
        path: path.to_path_buf(),
        reason: "helper output is not valid UTF-8".to_string(),
    })?;
    let mut entries = Vec::new();
    for (line_no, line) in stdout.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        entries.push(
            serde_json::from_str(line).map_err(|e| NativeHistoryError::Corrupt {
                path: path.to_path_buf(),
                reason: format!("helper output line {} is not valid JSON: {e}", line_no + 1),
            })?,
        );
    }
    Ok(entries)
}

fn text_blocks_join(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn user_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => text_blocks_join(blocks),
        _ => String::new(),
    }
}

fn map_messages(entries: &[Value]) -> Vec<NormalizedEntry> {
    let mut out: Vec<NormalizedEntry> = Vec::new();
    // toolCallId -> (position in `out`, tool name, arguments)
    let mut tool_slots: std::collections::HashMap<String, (usize, String, Option<Value>)> =
        std::collections::HashMap::new();

    for entry in entries {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(entry_type, "compaction" | "branch_summary") {
            let summary = entry
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !summary.is_empty() {
                out.push(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::SystemMessage,
                    content: summary,
                    metadata: None,
                });
            }
            continue;
        }
        if entry_type == "custom_message" {
            if entry.get("display").and_then(Value::as_bool) == Some(true) {
                let content = match entry.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(blocks)) => text_blocks_join(blocks),
                    _ => String::new(),
                };
                if !content.is_empty() {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::SystemMessage,
                        content,
                        metadata: None,
                    });
                }
            }
            continue;
        }

        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");

        match role {
            "user" => {
                let content = user_text(message);
                if !content.is_empty() {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::UserMessage,
                        content,
                        metadata: None,
                    });
                }
            }
            "assistant" => {
                let blocks = match message.get("content") {
                    Some(Value::Array(blocks)) => blocks.clone(),
                    _ => Vec::new(),
                };
                for block in &blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            let thinking = block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if !thinking.is_empty() {
                                out.push(NormalizedEntry {
                                    timestamp: None,
                                    entry_type: NormalizedEntryType::Thinking,
                                    content: thinking,
                                    metadata: None,
                                });
                            }
                        }
                        Some("text") => {
                            let text = block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if !text.is_empty() {
                                out.push(NormalizedEntry {
                                    timestamp: None,
                                    entry_type: NormalizedEntryType::AssistantMessage,
                                    content: text,
                                    metadata: None,
                                });
                            }
                        }
                        Some("toolCall") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let args = block.get("arguments").cloned();
                            let entry = NormalizedEntry {
                                timestamp: None,
                                entry_type: NormalizedEntryType::ToolUse {
                                    tool_name: display_tool_name(&name),
                                    action_type: map_action(&name, args.clone(), None),
                                    status: ToolStatus::Created,
                                },
                                content: name.clone(),
                                metadata: None,
                            };
                            out.push(entry);
                            if let Some(call_id) = block.get("id").and_then(Value::as_str) {
                                tool_slots.insert(call_id.to_string(), (out.len() - 1, name, args));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "toolResult" => {
                let call_id = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result_text = match message.get("content") {
                    Some(Value::Array(blocks)) => text_blocks_join(blocks),
                    Some(Value::String(text)) => text.clone(),
                    _ => String::new(),
                };
                if let Some((position, name, args)) = tool_slots.remove(call_id) {
                    let result_value = Value::String(result_text);
                    out[position] = NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::ToolUse {
                            tool_name: display_tool_name(&name),
                            action_type: map_action(&name, args, Some(result_value)),
                            status: if is_error {
                                ToolStatus::Failed
                            } else {
                                ToolStatus::Success
                            },
                        },
                        content: name.clone(),
                        metadata: None,
                    };
                }
            }
            "bashExecution" => {
                let command = message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let output = message
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let exit_code = message
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .map(|code| code as i32);
                out.push(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::ToolUse {
                        tool_name: display_tool_name("bash"),
                        action_type: ActionType::CommandRun {
                            command: command.clone(),
                            result: Some(CommandRunResult {
                                exit_status: exit_code
                                    .map(|code| CommandExitStatus::ExitCode { code }),
                                output: Some(truncate_for_display(&output)),
                            }),
                        },
                        status: if exit_code == Some(0) {
                            ToolStatus::Success
                        } else {
                            ToolStatus::Failed
                        },
                    },
                    content: command,
                    metadata: None,
                });
            }
            _ => {}
        }
    }

    // Tool calls without a result never completed; surface them as failed
    // instead of leaving a perpetual "created" spinner in history.
    for (_, (position, name, args)) in tool_slots {
        out[position] = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: display_tool_name(&name),
                action_type: map_action(&name, args, None),
                status: ToolStatus::Failed,
            },
            content: name,
            metadata: None,
        };
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native_history/pi")
            .join(name)
    }

    async fn read_fixture(
        name: &str,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError> {
        PiNativeSessionHistory.read(&fixture(name), tail).await
    }

    #[tokio::test]
    async fn native_session_history_pi_maps_final_messages() {
        let history = read_fixture("basic_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        assert_eq!(history.session_id, "11111111-2222-3333-4444-555555555555");
        assert!(!history.partial_tail);

        let kinds: Vec<&'static str> = history
            .entries
            .iter()
            .map(|entry| match &entry.entry_type {
                NormalizedEntryType::UserMessage => "user",
                NormalizedEntryType::AssistantMessage => "assistant",
                NormalizedEntryType::Thinking => "thinking",
                NormalizedEntryType::ToolUse { .. } => "tool",
                NormalizedEntryType::SystemMessage => "system",
                _ => "other",
            })
            .collect();
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
    async fn native_session_history_pi_is_deterministic() {
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

    #[test]
    fn native_session_history_pi_detects_tree_features() {
        let branched = std::fs::read_to_string(fixture("branched_session.jsonl")).unwrap();
        let entries: Vec<Value> = branched
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(has_tree_features(&entries));

        let linear = std::fs::read_to_string(fixture("basic_session.jsonl")).unwrap();
        let entries: Vec<Value> = linear
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!has_tree_features(&entries));
    }

    #[tokio::test]
    async fn pi_native_history_helper_resolves_branched_session() {
        if !helper_available() {
            eprintln!(
                "SKIP pi_native_history_helper_resolves_branched_session: node or the installed @earendil-works/pi-coding-agent package is unavailable"
            );
            return;
        }
        let history = read_fixture("branched_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        let user_texts: Vec<&str> = history
            .entries
            .iter()
            .filter(|entry| matches!(entry.entry_type, NormalizedEntryType::UserMessage))
            .map(|entry| entry.content.as_str())
            .collect();
        // Active branch only: "方案 A" lives on the abandoned branch.
        assert_eq!(user_texts, vec!["整理 docs 目录", "换个思路"]);
        assert!(
            history
                .entries
                .iter()
                .all(|entry| !entry.content.contains("方案 A"))
        );
    }

    #[tokio::test]
    async fn pi_native_history_helper_resolves_compacted_session() {
        if !helper_available() {
            eprintln!(
                "SKIP pi_native_history_helper_resolves_compacted_session: node or the installed @earendil-works/pi-coding-agent package is unavailable"
            );
            return;
        }
        let history = read_fixture("compacted_session.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        assert!(history.entries.iter().any(|entry| matches!(
            entry.entry_type,
            NormalizedEntryType::SystemMessage
        ) && entry.content.contains("整理文档")));
    }

    #[tokio::test]
    #[ignore = "release-mode benchmark: cargo test -p executors --release pi_native_history_benchmark -- --ignored"]
    async fn pi_native_history_benchmark() {
        let dir = std::env::temp_dir().join(format!("vk-pi-bench-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bench.jsonl");
        let mut content = String::from(
            "{\"type\":\"session\",\"version\":3,\"id\":\"bench\",\"timestamp\":\"2026-08-12T00:00:00.000Z\",\"cwd\":\"/tmp/bench\"}\n",
        );
        let mut parent = "null".to_string();
        for i in 0..1500 {
            let user_id = format!("u{i:06}");
            let asst_id = format!("a{i:06}");
            content.push_str(&format!(
                "{{\"type\":\"message\",\"id\":\"{user_id}\",\"parentId\":{parent},\"timestamp\":\"2026-08-12T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"第 {i} 轮请求，请检查模块输出是否包含预期字段\",\"timestamp\":1780000000000}}}}\n"
            ));
            content.push_str(&format!(
                "{{\"type\":\"message\",\"id\":\"{asst_id}\",\"parentId\":\"{user_id}\",\"timestamp\":\"2026-08-12T00:00:02.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"分析第 {i} 轮：需要对比输入输出，确认字段完整性，并考虑边界情况。\"}},{{\"type\":\"text\",\"text\":\"第 {i} 轮完成：所有检查通过，结果已写入输出文件。\"}}],\"provider\":\"test\",\"model\":\"test\",\"stopReason\":\"stop\",\"timestamp\":1780000000001}}}}\n"
            ));
            parent = format!("\"{asst_id}\"");
        }
        std::fs::write(&path, content).unwrap();

        let start = std::time::Instant::now();
        let history = PiNativeSessionHistory
            .read(&path, TailPolicy::Completed)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(history.entries.len(), 4500);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "benchmark took {elapsed:?}"
        );
    }

    /// Shadow comparison (plan Phase 5): the same logical turn goes through
    /// the live RPC normalizer and through the native provider; tool calls,
    /// thinking, and the final assistant text must agree. The live normalizer
    /// keeps one streaming slot per message kind, so intermediate assistant
    /// texts within a turn collapse there, while the native file preserves
    /// every message — the native view is the authoritative review source.
    #[tokio::test]
    async fn pi_native_history_shadow_matches_live_normalizer() {
        use std::sync::Arc;

        use workspace_utils::{log_msg::LogMsg, msg_store::MsgStore};

        use super::super::normalize_logs::spawn_stdout_normalizer;
        use crate::logs::utils::{EntryIndexProvider, patch::extract_normalized_entry_from_patch};

        let frames = [
            r#"{"type":"vk_pi_session_start","sessionId":"s-shadow"}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"分析"}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"请求"}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"先看看"}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"日志"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"先看看日志"}]}}"#,
            r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"ls docs"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","result":"README.md\nplans/","isError":false}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"已完成"}}"#,
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"已完成：docs 已整理。"}]}}"#,
        ];

        let store = Arc::new(MsgStore::new());
        for frame in frames {
            store.push_stdout(format!("{frame}\n"));
        }
        store.push_finished();
        let entry_index = EntryIndexProvider::start_from(&store);
        let handle = spawn_stdout_normalizer(store.clone(), entry_index);
        let _ = handle.await;

        let mut by_index: std::collections::BTreeMap<usize, NormalizedEntry> = Default::default();
        for msg in store.get_history() {
            if let LogMsg::JsonPatch(patch) = msg
                && let Some((index, entry)) = extract_normalized_entry_from_patch(&patch)
            {
                by_index.insert(index, entry);
            }
        }
        let live: Vec<NormalizedEntry> = by_index.into_values().collect();

        // Same conversation as a native session file.
        let dir = std::env::temp_dir().join(format!("vk-pi-shadow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s-shadow\",\"timestamp\":\"2026-08-12T01:00:00.000Z\",\"cwd\":\"/tmp/demo\"}\n",
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-12T01:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"整理 docs 目录\",\"timestamp\":1780000000000}}\n",
                "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-08-12T01:00:02.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"分析请求\"},{\"type\":\"text\",\"text\":\"先看看日志\"},{\"type\":\"toolCall\",\"id\":\"call_1\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls docs\"}}],\"provider\":\"test\",\"model\":\"test\",\"stopReason\":\"toolUse\",\"timestamp\":1780000000001}}\n",
                "{\"type\":\"message\",\"id\":\"m3\",\"parentId\":\"m2\",\"timestamp\":\"2026-08-12T01:00:03.000Z\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call_1\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"README.md\\nplans/\"}],\"isError\":false,\"timestamp\":1780000000002}}\n",
                "{\"type\":\"message\",\"id\":\"m4\",\"parentId\":\"m3\",\"timestamp\":\"2026-08-12T01:00:04.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"已完成：docs 已整理。\"}],\"provider\":\"test\",\"model\":\"test\",\"stopReason\":\"stop\",\"timestamp\":1780000000003}}\n",
            ),
        )
        .unwrap();
        let native = PiNativeSessionHistory
            .read(&path, TailPolicy::Completed)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let kinds = |entries: &[NormalizedEntry]| {
            entries
                .iter()
                .map(|entry| match &entry.entry_type {
                    NormalizedEntryType::UserMessage => "user",
                    NormalizedEntryType::AssistantMessage => "assistant",
                    NormalizedEntryType::Thinking => "thinking",
                    NormalizedEntryType::ToolUse { .. } => "tool",
                    _ => "other",
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&live), vec!["thinking", "assistant", "tool"]);
        assert_eq!(
            kinds(&native.entries),
            vec!["user", "thinking", "assistant", "tool", "assistant"]
        );

        // Thinking and final assistant text agree.
        assert_eq!(live[0].content, "分析请求");
        assert_eq!(live[0].content, native.entries[1].content);
        assert_eq!(live[1].content, "已完成：docs 已整理。");
        assert_eq!(live[1].content, native.entries[4].content);
        // The native file keeps the intermediate assistant text the live slot
        // model collapses away.
        assert_eq!(native.entries[2].content, "先看看日志");

        // Tool call parity: name, success, output.
        let tool_output = |entry: &NormalizedEntry| match &entry.entry_type {
            NormalizedEntryType::ToolUse {
                tool_name,
                action_type: ActionType::CommandRun { command, result },
                status,
            } => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(command, "ls docs");
                assert!(matches!(status, ToolStatus::Success));
                result
                    .as_ref()
                    .and_then(|r| r.output.clone())
                    .unwrap_or_default()
            }
            other => panic!("expected command-run tool entry, got {other:?}"),
        };
        assert_eq!(tool_output(&live[2]), tool_output(&native.entries[3]));
    }

    #[tokio::test]
    #[ignore = "manual evidence: VK_PI_REAL_SESSION=/path/to/session.jsonl"]
    async fn pi_native_history_real_sample() {
        let Ok(path) = std::env::var("VK_PI_REAL_SESSION") else {
            eprintln!("SKIP pi_native_history_real_sample: set VK_PI_REAL_SESSION");
            return;
        };
        let history = PiNativeSessionHistory
            .read(std::path::Path::new(&path), TailPolicy::Completed)
            .await
            .unwrap();
        let count = |want: &str| {
            history
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        (&entry.entry_type, want),
                        (NormalizedEntryType::UserMessage, "user")
                            | (NormalizedEntryType::AssistantMessage, "assistant")
                            | (NormalizedEntryType::Thinking, "thinking")
                            | (NormalizedEntryType::ToolUse { .. }, "tool")
                    )
                })
                .count()
        };
        // Counts from the original problem session (jq-verified).
        assert_eq!(count("user"), 1);
        assert_eq!(count("tool"), 76);
        assert!(count("assistant") >= 1);
        assert!(count("thinking") >= 1);
    }

    #[tokio::test]
    async fn native_session_history_pi_tail_policies() {
        let history = read_fixture("trailing_partial.jsonl", TailPolicy::Running)
            .await
            .unwrap();
        assert!(history.partial_tail);
        assert!(!history.entries.is_empty());

        let err = read_fixture("trailing_partial.jsonl", TailPolicy::Completed)
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            crate::history::NativeHistoryErrorCode::NativeSessionCorrupt
        );
    }

    #[tokio::test]
    async fn native_session_history_pi_locate_requires_header_match() {
        let root = fixture("scan_root");
        let found = find_session_file(&root, "11111111-2222-3333-4444-555555555555", 4)
            .expect("fixture session should be locatable");
        assert!(
            found.ends_with("2026-08-12T01-00-00-000Z_11111111-2222-3333-4444-555555555555.jsonl")
        );

        // Filename mentions the id but the header belongs to another session:
        // never trust the filename alone.
        assert!(find_session_file(&root, "99999999-0000-0000-0000-000000000000", 4).is_none());
    }
}
