//! Codex native session history provider.
//!
//! Reads the rollout file Codex persists under
//! `$CODEX_HOME/sessions/<yyyy>/<mm>/<dd>/rollout-<ts>-<session-id>.jsonl`.
//! Loading uses the official `RolloutItem` serde types (the same schema
//! `RolloutRecorder` writes), so the mapping below only deals with final,
//! materialized items — never delta events.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use codex_protocol::{
    models::{ContentItem, FunctionCallOutputPayload, ResponseItem},
    protocol::{EventMsg, RolloutItem},
};
use serde_json::Value;

use super::session::{SessionError, SessionHandler};
use crate::{
    executors::BaseCodingAgent,
    history::{
        FileFingerprint, NativeHistoryError, NativeSessionHistory, NativeSessionHistoryProvider,
        TailPolicy, read_native_jsonl,
    },
    logs::{
        ActionType, CommandRunResult, NormalizedEntry, NormalizedEntryType, TokenUsageInfo,
        ToolResult, ToolStatus,
    },
};

#[derive(Debug)]
pub struct CodexNativeSessionHistory;

/// Verify the file actually belongs to `session_id` — `fork_rollout_file`
/// rewrites ids, and filename matching alone can pick the wrong rollout.
fn header_matches_session(path: &Path, session_id: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    if std::io::BufRead::read_line(&mut reader, &mut first_line).is_err() {
        return false;
    }
    let Ok(RolloutItem::SessionMeta(meta_line)) =
        serde_json::from_str::<RolloutItem>(first_line.trim())
    else {
        return false;
    };
    meta_line.meta.id.to_string() == session_id
}

#[async_trait]
impl NativeSessionHistoryProvider for CodexNativeSessionHistory {
    fn agent(&self) -> BaseCodingAgent {
        BaseCodingAgent::Codex
    }

    async fn locate(&self, session_id: &str, _cwd: &Path) -> Result<PathBuf, NativeHistoryError> {
        match SessionHandler::find_rollout_file_path(session_id) {
            Ok(path) if header_matches_session(&path, session_id) => Ok(path),
            Ok(path) => Err(NativeHistoryError::Corrupt {
                path,
                reason: "rollout filename matches but session_meta id differs".to_string(),
            }),
            Err(SessionError::NotFound(_)) => Err(NativeHistoryError::FileNotFound {
                session_id: session_id.to_string(),
                root: SessionHandler::sessions_root_for_error(),
            }),
            Err(err) => Err(NativeHistoryError::FormatUnsupported(err.to_string())),
        }
    }

    async fn read(
        &self,
        path: &Path,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError> {
        let content = read_native_jsonl(path, tail)?;
        let fingerprint = FileFingerprint::of(path)?;

        let mut items: Vec<RolloutItem> = Vec::with_capacity(content.lines.len());
        for (line_no, line) in content.lines.iter().enumerate() {
            let item = serde_json::from_str::<RolloutItem>(line).map_err(|e| {
                NativeHistoryError::Corrupt {
                    path: path.to_path_buf(),
                    reason: format!("line {} is not a valid rollout item: {e}", line_no + 1),
                }
            })?;
            items.push(item);
        }

        let session_id = items
            .iter()
            .find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id.to_string()),
                _ => None,
            })
            .ok_or_else(|| NativeHistoryError::Corrupt {
                path: path.to_path_buf(),
                reason: "rollout has no session_meta line".to_string(),
            })?;

        let entries = map_rollout_items(&items);

        Ok(NativeSessionHistory {
            agent: BaseCodingAgent::Codex,
            session_id,
            entries,
            fingerprint,
            partial_tail: content.partial_tail,
        })
    }
}

fn message_text(content: &[ContentItem], want_input: bool) -> String {
    content
        .iter()
        .filter_map(|item| match (want_input, item) {
            (true, ContentItem::InputText { text }) => Some(text.as_str()),
            (false, ContentItem::OutputText { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn output_payload_text(payload: &FunctionCallOutputPayload) -> String {
    let mut parts = vec![payload.content.as_str()];
    if let Some(items) = &payload.content_items {
        parts.extend(items.iter().filter_map(|item| match item {
            codex_protocol::models::FunctionCallOutputContentItem::InputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        }));
    }
    parts.join("\n")
}

fn shell_command(arguments: &str) -> String {
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_string();
    };
    for key in ["cmd", "command"] {
        match args.get(key) {
            Some(Value::String(command)) => return command.clone(),
            Some(Value::Array(parts)) => {
                return parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            _ => {}
        }
    }
    arguments.to_string()
}

fn tool_action(name: &str, arguments: &str) -> ActionType {
    match name {
        "exec_command" | "shell" | "local_shell_exec" | "container.exec" => {
            ActionType::CommandRun {
                command: shell_command(arguments),
                result: None,
            }
        }
        _ => ActionType::Tool {
            tool_name: name.to_string(),
            arguments: serde_json::from_str::<Value>(arguments).ok(),
            result: None,
        },
    }
}

fn map_rollout_items(items: &[RolloutItem]) -> Vec<NormalizedEntry> {
    // Dedupe rule (locked by fixture): `response_item` messages are durable and
    // authoritative; `event_msg` user/agent messages that repeat their text are
    // skipped, anything not present in response items is still surfaced.
    let mut response_message_texts: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for item in items {
        if let RolloutItem::ResponseItem(ResponseItem::Message { content, .. }) = item {
            for text in [message_text(content, true), message_text(content, false)] {
                if !text.is_empty() {
                    response_message_texts.insert(text);
                }
            }
        }
    }

    let mut out: Vec<NormalizedEntry> = Vec::new();
    // call_id -> position in `out`
    let mut tool_slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for item in items {
        match item {
            RolloutItem::ResponseItem(response) => match response {
                ResponseItem::Message { role, content, .. } => {
                    let (entry_type, text) = match role.as_str() {
                        "user" => (
                            NormalizedEntryType::UserMessage,
                            message_text(content, true),
                        ),
                        "assistant" => (
                            NormalizedEntryType::AssistantMessage,
                            message_text(content, false),
                        ),
                        // system/developer items are prompt scaffolding, not conversation
                        _ => continue,
                    };
                    if !text.is_empty() {
                        out.push(NormalizedEntry {
                            timestamp: None,
                            entry_type,
                            content: text,
                            metadata: None,
                        });
                    }
                }
                ResponseItem::Reasoning {
                    summary, content, ..
                } => {
                    let text = content
                        .as_ref()
                        .map(|blocks| {
                            blocks
                                .iter()
                                .map(|block| match block {
                                    codex_protocol::models::ReasoningItemContent::ReasoningText {
                                        text,
                                    }
                                    | codex_protocol::models::ReasoningItemContent::Text {
                                        text,
                                    } => text.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| {
                            summary
                                .iter()
                                .map(|s| match s {
                                    codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                                        text,
                                    } => text.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        });
                    if !text.is_empty() {
                        out.push(NormalizedEntry {
                            timestamp: None,
                            entry_type: NormalizedEntryType::Thinking,
                            content: text,
                            metadata: None,
                        });
                    }
                }
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    let action_type = tool_action(name, arguments);
                    let content = match &action_type {
                        ActionType::CommandRun { command, .. } => command.clone(),
                        _ => name.clone(),
                    };
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::ToolUse {
                            tool_name: name.clone(),
                            action_type,
                            status: ToolStatus::Created,
                        },
                        content,
                        metadata: None,
                    });
                    tool_slots.insert(call_id.clone(), out.len() - 1);
                }
                ResponseItem::CustomToolCall {
                    name,
                    input,
                    call_id,
                    ..
                } => {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::ToolUse {
                            tool_name: name.clone(),
                            action_type: ActionType::Tool {
                                tool_name: name.clone(),
                                arguments: Some(Value::String(input.clone())),
                                result: None,
                            },
                            status: ToolStatus::Created,
                        },
                        content: name.clone(),
                        metadata: None,
                    });
                    tool_slots.insert(call_id.clone(), out.len() - 1);
                }
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    apply_tool_result(
                        &mut out,
                        &mut tool_slots,
                        call_id,
                        output_payload_text(output),
                        output.success.unwrap_or(true),
                    );
                }
                ResponseItem::CustomToolCallOutput { call_id, output } => {
                    apply_tool_result(&mut out, &mut tool_slots, call_id, output.clone(), true);
                }
                _ => {}
            },
            RolloutItem::EventMsg(EventMsg::AgentMessage(event)) => {
                if !response_message_texts.contains(&event.message) && !event.message.is_empty() {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::AssistantMessage,
                        content: event.message.clone(),
                        metadata: None,
                    });
                }
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) => {
                if !response_message_texts.contains(&event.message) && !event.message.is_empty() {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::UserMessage,
                        content: event.message.clone(),
                        metadata: None,
                    });
                }
            }
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                if let Some(info) = &event.info {
                    out.push(NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::TokenUsageInfo(TokenUsageInfo {
                            total_tokens: info.total_token_usage.total_tokens.max(0) as u32,
                            model_context_window: info.model_context_window.unwrap_or(0).max(0)
                                as u32,
                        }),
                        content: format!(
                            "Tokens used: {} / Context window: {}",
                            info.total_token_usage.total_tokens,
                            info.model_context_window.unwrap_or(0)
                        ),
                        metadata: None,
                    });
                }
            }
            RolloutItem::Compacted(compacted) => {
                out.push(NormalizedEntry {
                    timestamp: None,
                    entry_type: NormalizedEntryType::SystemMessage,
                    content: compacted.message.clone(),
                    metadata: None,
                });
            }
            _ => {}
        }
    }

    // Tool calls without outputs never finished; mark them failed so history
    // does not show a perpetual "created" state.
    for position in tool_slots.values() {
        if let Some(entry) = out.get_mut(*position)
            && let Some(updated) = entry.with_tool_status(ToolStatus::Failed)
        {
            *entry = updated;
        }
    }

    out
}

fn apply_tool_result(
    out: &mut [NormalizedEntry],
    tool_slots: &mut std::collections::HashMap<String, usize>,
    call_id: &str,
    text: String,
    success: bool,
) {
    let Some(position) = tool_slots.remove(call_id) else {
        return;
    };
    let status = if success {
        ToolStatus::Success
    } else {
        ToolStatus::Failed
    };
    let Some(entry) = out.get_mut(position) else {
        return;
    };
    let NormalizedEntryType::ToolUse {
        tool_name,
        action_type,
        ..
    } = &mut entry.entry_type
    else {
        return;
    };
    match action_type {
        ActionType::CommandRun { result, .. } => {
            *result = Some(CommandRunResult {
                exit_status: None,
                output: Some(text),
            });
        }
        ActionType::Tool { result, .. } => {
            *result = Some(ToolResult::markdown(text));
        }
        _ => {}
    }
    let _ = tool_name;
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
            .join("tests/fixtures/native_history/codex")
            .join(name)
    }

    async fn read_fixture(
        name: &str,
        tail: TailPolicy,
    ) -> Result<NativeSessionHistory, NativeHistoryError> {
        CodexNativeSessionHistory.read(&fixture(name), tail).await
    }

    #[tokio::test]
    async fn native_session_history_codex_maps_rollout_items() {
        let history = read_fixture("basic_rollout.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        assert_eq!(history.session_id, "22222222-3333-4444-5555-666666666666");

        let kinds: Vec<&'static str> = history
            .entries
            .iter()
            .map(|entry| match &entry.entry_type {
                NormalizedEntryType::UserMessage => "user",
                NormalizedEntryType::AssistantMessage => "assistant",
                NormalizedEntryType::Thinking => "thinking",
                NormalizedEntryType::ToolUse { .. } => "tool",
                NormalizedEntryType::TokenUsageInfo(_) => "tokens",
                NormalizedEntryType::SystemMessage => "system",
                _ => "other",
            })
            .collect();
        // No delta, no duplicate agent_message event, system prompt skipped.
        assert_eq!(
            kinds,
            vec!["user", "thinking", "tool", "assistant", "tokens"]
        );

        let NormalizedEntryType::ToolUse {
            action_type,
            status,
            ..
        } = &history.entries[2].entry_type
        else {
            panic!("expected tool use entry");
        };
        assert!(matches!(status, ToolStatus::Success));
        let ActionType::CommandRun { command, result } = action_type else {
            panic!("expected command run");
        };
        assert_eq!(command, "ls -la");
        assert!(
            result
                .as_ref()
                .and_then(|r| r.output.as_deref())
                .is_some_and(|output| output.contains("total 42"))
        );
    }

    #[tokio::test]
    async fn native_session_history_codex_is_deterministic() {
        let a = read_fixture("basic_rollout.jsonl", TailPolicy::Completed)
            .await
            .unwrap();
        let b = read_fixture("basic_rollout.jsonl", TailPolicy::Completed)
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
    async fn native_session_history_codex_tail_policies() {
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
