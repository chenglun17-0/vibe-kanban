use std::{collections::HashMap, path::Path, sync::Arc};

use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use workspace_utils::msg_store::MsgStore;

use crate::logs::{
    ActionType, CommandRunResult, NormalizedEntry, NormalizedEntryError, NormalizedEntryType,
    TokenUsageInfo, ToolResult, ToolStatus,
    stderr_processor::normalize_stderr_logs,
    utils::{ConversationPatch, EntryIndexProvider, patch::add_normalized_entry},
};

/// Normalize Pi RPC JSONL frames (raw stdout) plus stderr into conversation entries.
pub fn normalize_logs(msg_store: Arc<MsgStore>, _worktree_path: &Path) {
    let entry_index = EntryIndexProvider::start_from(&msg_store);
    normalize_stderr_logs(msg_store.clone(), entry_index.clone());
    let _handle = spawn_stdout_normalizer(msg_store, entry_index);
    // The handle is intentionally dropped: the task ends when the MsgStore
    // stream closes (push_finished), mirroring other executors.
}

pub(crate) fn spawn_stdout_normalizer(
    msg_store: Arc<MsgStore>,
    entry_index: EntryIndexProvider,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = LogState::new(entry_index, msg_store.clone());
        let mut stdout = msg_store.stdout_lines_stream();
        while let Some(Ok(line)) = stdout.next().await {
            state.handle_line(&line);
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PiFrame {
    #[serde(rename = "vk_pi_session_start")]
    SessionStart {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "vk_pi_token_usage")]
    TokenUsage {
        #[serde(default)]
        tokens: Option<u32>,
        #[serde(default, rename = "contextWindow")]
        context_window: Option<u32>,
    },
    #[serde(rename = "message_update")]
    MessageUpdate {
        #[serde(rename = "assistantMessageEvent")]
        event: AssistantEvent,
    },
    #[serde(rename = "message_end")]
    MessageEnd { message: Value },
    #[serde(rename = "tool_execution_start")]
    ToolStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default)]
        args: Option<Value>,
    },
    #[serde(rename = "tool_execution_update")]
    ToolUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default, rename = "partialResult")]
        partial_result: Option<Value>,
    },
    #[serde(rename = "tool_execution_end")]
    ToolEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default, rename = "isError")]
        is_error: bool,
    },
    #[serde(rename = "extension_error")]
    ExtensionError { error: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AssistantEvent {
    #[serde(rename = "text_delta")]
    TextDelta { delta: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { delta: String },
    #[serde(other)]
    Other,
}

struct StreamingEntry {
    index: usize,
    content: String,
}

struct LogState {
    entry_index: EntryIndexProvider,
    msg_store: Arc<MsgStore>,
    stored_session: bool,
    assistant: Option<StreamingEntry>,
    thinking: Option<StreamingEntry>,
    tool_indices: HashMap<String, usize>,
    tool_args: HashMap<String, (String, Option<Value>)>,
}

impl LogState {
    fn new(entry_index: EntryIndexProvider, msg_store: Arc<MsgStore>) -> Self {
        Self {
            entry_index,
            msg_store,
            stored_session: false,
            assistant: None,
            thinking: None,
            tool_indices: HashMap::new(),
            tool_args: HashMap::new(),
        }
    }

    fn handle_line(&mut self, line: &str) {
        let frame = match serde_json::from_str::<PiFrame>(line.trim()) {
            Ok(frame) => frame,
            Err(_) => return, // non-JSON stdout stays visible via raw logs only
        };
        match frame {
            PiFrame::SessionStart { session_id } => {
                if !self.stored_session {
                    self.msg_store.push_session_id(session_id);
                    self.stored_session = true;
                }
            }
            PiFrame::TokenUsage {
                tokens: Some(tokens),
                context_window: Some(window),
            } => {
                add_normalized_entry(
                    &self.msg_store,
                    &self.entry_index,
                    NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::TokenUsageInfo(TokenUsageInfo {
                            total_tokens: tokens,
                            model_context_window: window,
                        }),
                        content: format!("Tokens used: {tokens} / Context window: {window}"),
                        metadata: None,
                    },
                );
            }
            PiFrame::TokenUsage { .. } | PiFrame::Other => {}
            PiFrame::MessageUpdate { event } => match event {
                AssistantEvent::TextDelta { delta } => {
                    self.append_streaming(true, &delta, NormalizedEntryType::AssistantMessage)
                }
                AssistantEvent::ThinkingDelta { delta } => {
                    self.append_streaming(false, &delta, NormalizedEntryType::Thinking)
                }
                AssistantEvent::Other => {}
            },
            PiFrame::MessageEnd { message } => self.handle_message_end(message),
            PiFrame::ToolStart {
                tool_call_id,
                tool_name,
                args,
            } => self.handle_tool_start(tool_call_id, tool_name, args),
            PiFrame::ToolUpdate {
                tool_call_id,
                tool_name,
                partial_result,
            } => self.handle_tool_update(tool_call_id, tool_name, partial_result),
            PiFrame::ToolEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => self.handle_tool_end(tool_call_id, tool_name, result, is_error),
            PiFrame::ExtensionError { error } => {
                add_normalized_entry(
                    &self.msg_store,
                    &self.entry_index,
                    NormalizedEntry {
                        timestamp: None,
                        entry_type: NormalizedEntryType::ErrorMessage {
                            error_type: NormalizedEntryError::Other,
                        },
                        content: error,
                        metadata: None,
                    },
                );
            }
        }
    }

    fn append_streaming(&mut self, assistant: bool, delta: &str, entry_type: NormalizedEntryType) {
        let slot = if assistant {
            &mut self.assistant
        } else {
            &mut self.thinking
        };
        match slot {
            Some(stream) => {
                stream.content.push_str(delta);
                let entry = NormalizedEntry {
                    timestamp: None,
                    entry_type,
                    content: stream.content.clone(),
                    metadata: None,
                };
                self.msg_store
                    .push_patch(ConversationPatch::replace(stream.index, entry));
            }
            None => {
                let index = self.entry_index.next();
                *slot = Some(StreamingEntry {
                    index,
                    content: delta.to_string(),
                });
                let entry = NormalizedEntry {
                    timestamp: None,
                    entry_type,
                    content: delta.to_string(),
                    metadata: None,
                };
                self.msg_store
                    .push_patch(ConversationPatch::add_normalized_entry(index, entry));
            }
        }
    }

    /// `message_end` is authoritative: reconcile the streamed assistant text
    /// with the final message so missing deltas cannot corrupt the transcript.
    fn handle_message_end(&mut self, message: Value) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let text = extract_assistant_text(&message);
        if text.is_empty() {
            return;
        }
        let stream = self.assistant.get_or_insert_with(|| StreamingEntry {
            index: self.entry_index.next(),
            content: String::new(),
        });
        stream.content = text.clone();
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::AssistantMessage,
            content: text,
            metadata: None,
        };
        self.msg_store
            .push_patch(ConversationPatch::replace(stream.index, entry));
    }

    fn handle_tool_start(&mut self, tool_call_id: String, tool_name: String, args: Option<Value>) {
        let index = self.entry_index.next();
        self.tool_indices.insert(tool_call_id.clone(), index);
        self.tool_args
            .insert(tool_call_id, (tool_name.clone(), args.clone()));
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: display_tool_name(&tool_name),
                action_type: map_action(&tool_name, args, None),
                status: ToolStatus::Created,
            },
            content: tool_name,
            metadata: None,
        };
        self.msg_store
            .push_patch(ConversationPatch::add_normalized_entry(index, entry));
    }

    /// `partialResult` is a cumulative snapshot: always replace, never append.
    fn handle_tool_update(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        partial_result: Option<Value>,
    ) {
        let Some(index) = self.tool_indices.get(&tool_call_id).copied() else {
            return;
        };
        let args = self
            .tool_args
            .get(&tool_call_id)
            .and_then(|(_, args)| args.clone());
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: display_tool_name(&tool_name),
                action_type: map_action(&tool_name, args, partial_result),
                status: ToolStatus::Created,
            },
            content: tool_name,
            metadata: None,
        };
        self.msg_store
            .push_patch(ConversationPatch::replace(index, entry));
    }

    fn handle_tool_end(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        result: Option<Value>,
        is_error: bool,
    ) {
        let Some(index) = self.tool_indices.remove(&tool_call_id) else {
            return;
        };
        let args = self
            .tool_args
            .remove(&tool_call_id)
            .and_then(|(_, args)| args);
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: display_tool_name(&tool_name),
                action_type: map_action(&tool_name, args, result),
                status: if is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Success
                },
            },
            content: tool_name,
            metadata: None,
        };
        self.msg_store
            .push_patch(ConversationPatch::replace(index, entry));
    }
}

fn extract_assistant_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub(crate) fn display_tool_name(tool_name: &str) -> String {
    match tool_name {
        "read" => "Read".to_string(),
        "write" | "edit" => "Edit".to_string(),
        "bash" => "Bash".to_string(),
        "grep" | "find" | "ls" => "Search".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn map_action(
    tool_name: &str,
    args: Option<Value>,
    result: Option<Value>,
) -> ActionType {
    let path = args
        .as_ref()
        .and_then(|a| a.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    match tool_name {
        "read" => ActionType::FileRead {
            path: path.unwrap_or_default(),
        },
        "write" | "edit" => ActionType::FileEdit {
            path: path.unwrap_or_default(),
            changes: vec![],
        },
        "bash" => {
            let command = args
                .as_ref()
                .and_then(|a| a.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ActionType::CommandRun {
                command,
                result: result.as_ref().map(|r| CommandRunResult {
                    exit_status: None,
                    output: Some(truncate_for_display(&summarize_value(r))),
                }),
            }
        }
        "grep" | "find" | "ls" => {
            let query = args
                .as_ref()
                .and_then(|a| {
                    a.get("pattern")
                        .or_else(|| a.get("query"))
                        .or_else(|| a.get("path"))
                })
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ActionType::Search { query }
        }
        other => ActionType::Tool {
            tool_name: other.to_string(),
            arguments: args
                .map(|a| serde_json::Value::String(truncate_for_display(&summarize_value(&a)))),
            result: result
                .map(|r| ToolResult::markdown(truncate_for_display(&summarize_value(&r)))),
        },
    }
}

pub(crate) fn summarize_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

const MAX_DISPLAY_CHARS: usize = 50_000;

pub(crate) fn truncate_for_display(text: &str) -> String {
    if text.chars().count() <= MAX_DISPLAY_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_DISPLAY_CHARS).collect();
    format!("{truncated}\n… [truncated; full output retained in raw logs]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::utils::patch::extract_normalized_entry_from_patch;

    fn collect(store: &Arc<MsgStore>) -> Vec<(usize, NormalizedEntry)> {
        store
            .get_history()
            .into_iter()
            .filter_map(|msg| match msg {
                workspace_utils::log_msg::LogMsg::JsonPatch(patch) => {
                    extract_normalized_entry_from_patch(&patch)
                }
                _ => None,
            })
            .collect()
    }

    async fn run_frames(frames: &[&str]) -> Vec<NormalizedEntry> {
        let msg_store = Arc::new(MsgStore::new());
        for frame in frames {
            // The RPC dispatcher persists each raw frame with a trailing LF; the
            // line stream only yields newline-terminated content.
            msg_store.push_stdout(format!("{frame}\n"));
        }
        msg_store.push_finished();
        let entry_index = EntryIndexProvider::start_from(&msg_store);
        let handle = spawn_stdout_normalizer(msg_store.clone(), entry_index);
        let _ = handle.await;
        collect(&msg_store)
            .into_iter()
            .map(|(_, entry)| entry)
            .collect()
    }

    #[tokio::test]
    async fn publishes_session_id_and_reconciles_assistant_text() {
        let store = Arc::new(MsgStore::new());
        store.push_stdout(format!(
            "{}\n",
            r#"{"type":"vk_pi_session_start","sessionId":"s-1"}"#
        ));
        store.push_stdout(format!(
            "{}\n",
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}"#
        ));
        store.push_stdout(format!(
            "{}\n",
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":" world"}}"#
        ));
        store.push_stdout(format!(
            "{}\n",
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hello world!"}]}}"#
        ));
        store.push_finished();

        let entry_index = EntryIndexProvider::start_from(&store);
        let handle = spawn_stdout_normalizer(store.clone(), entry_index);
        let _ = handle.await;

        let mut session_id = None;
        let mut assistant_texts = Vec::new();
        for msg in store.get_history() {
            match msg {
                workspace_utils::log_msg::LogMsg::SessionId(id) => session_id = Some(id),
                workspace_utils::log_msg::LogMsg::JsonPatch(patch) => {
                    if let Some((_, entry)) = extract_normalized_entry_from_patch(&patch)
                        && matches!(entry.entry_type, NormalizedEntryType::AssistantMessage)
                    {
                        assistant_texts.push(entry.content);
                    }
                }
                _ => {}
            }
        }

        assert_eq!(session_id.as_deref(), Some("s-1"));
        // The final message_end content is authoritative and must not be a
        // duplicated append of the streamed deltas.
        assert_eq!(
            assistant_texts.last().map(String::as_str),
            Some("Hello world!")
        );
    }

    #[tokio::test]
    async fn tool_partial_result_replaces_and_end_marks_success() {
        let frames = [
            r#"{"type":"tool_execution_start","toolCallId":"t1","toolName":"bash","args":{"command":"ls"}}"#,
            r#"{"type":"tool_execution_update","toolCallId":"t1","toolName":"bash","partialResult":{"output":"a"}}"#,
            r#"{"type":"tool_execution_update","toolCallId":"t1","toolName":"bash","partialResult":{"output":"ab"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"t1","toolName":"bash","result":{"output":"ab"},"isError":false}"#,
        ];
        let entries = run_frames(&frames).await;
        let tool_entries: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e.entry_type, NormalizedEntryType::ToolUse { .. }))
            .collect();
        assert!(!tool_entries.is_empty());
        let final_entry = tool_entries.last().unwrap();
        match &final_entry.entry_type {
            NormalizedEntryType::ToolUse {
                action_type,
                status,
                ..
            } => {
                assert!(matches!(status, ToolStatus::Success));
                match action_type {
                    ActionType::CommandRun {
                        result, command, ..
                    } => {
                        assert_eq!(command, "ls");
                        let output = result
                            .as_ref()
                            .and_then(|r| r.output.as_deref())
                            .unwrap_or_default();
                        // Partial results replace rather than append: "ab", never "aab".
                        assert!(output.contains("\"ab\""), "unexpected output: {output}");
                        assert!(!output.contains("aab"), "duplicated delta: {output}");
                    }
                    other => panic!("expected CommandRun, got {other:?}"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn unknown_frames_are_ignored_without_crashing() {
        let frames = [
            r#"{"type":"future_event","payload":{}}"#,
            "not json at all",
            r#"{"type":"vk_pi_token_usage","tokens":10,"contextWindow":100}"#,
        ];
        let entries = run_frames(&frames).await;
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].entry_type,
            NormalizedEntryType::TokenUsageInfo(_)
        ));
    }
}
