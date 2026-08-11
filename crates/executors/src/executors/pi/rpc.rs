use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::executors::ExecutorError;

const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RpcRequestId(pub u64);

impl std::fmt::Display for RpcRequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum PiRpcCommand {
    #[serde(rename = "prompt")]
    Prompt { message: String },
    #[serde(rename = "abort")]
    Abort,
    #[serde(rename = "get_state")]
    GetState,
    #[serde(rename = "get_session_stats")]
    GetSessionStats,
    #[serde(rename = "get_entries")]
    GetEntries,
    #[serde(rename = "get_available_models")]
    GetAvailableModels,
    #[serde(rename = "get_available_thinking_levels")]
    GetAvailableThinkingLevels,
    #[serde(rename = "get_commands")]
    GetCommands,
}

impl PiRpcCommand {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Abort => "abort",
            Self::GetState => "get_state",
            Self::GetSessionStats => "get_session_stats",
            Self::GetEntries => "get_entries",
            Self::GetAvailableModels => "get_available_models",
            Self::GetAvailableThinkingLevels => "get_available_thinking_levels",
            Self::GetCommands => "get_commands",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PiRpcRequest {
    pub id: RpcRequestId,
    #[serde(flatten)]
    pub command: PiRpcCommand,
}

impl PiRpcRequest {
    pub fn new(id: RpcRequestId, command: PiRpcCommand) -> Self {
        Self { id, command }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcResponse {
    pub id: RpcRequestId,
    #[serde(rename = "type")]
    pub type_: String,
    pub command: String,
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcSessionState {
    #[serde(default)]
    pub model: Option<Value>,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub is_compacting: bool,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default, rename = "sessionName")]
    pub session_name: Option<String>,
    #[serde(default, rename = "messageCount")]
    pub message_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcSessionStats {
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tokens: Option<Value>,
    #[serde(default, rename = "contextUsage")]
    pub context_usage: Option<PiRpcContextUsage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcContextUsage {
    #[serde(default)]
    pub tokens: Option<u32>,
    #[serde(default, rename = "contextWindow")]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcEntries {
    #[serde(default)]
    pub entries: Vec<Value>,
    #[serde(default, rename = "leafId")]
    pub leaf_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcAvailableModels {
    #[serde(default)]
    pub models: Vec<PiRpcModel>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub reasoning: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcThinkingLevels {
    #[serde(default)]
    pub levels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcCommands {
    #[serde(default)]
    pub commands: Vec<PiRpcCommandInfo>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PiRpcCommandInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum PiRpcEvent {
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "agent_settled")]
    AgentSettled,
    #[serde(rename = "agent_end")]
    AgentEnd {
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "turn_end")]
    TurnEnd {
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(rename = "message_update")]
    MessageUpdate {
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: PiRpcAssistantMessageEvent,
    },
    #[serde(rename = "message_start")]
    MessageStart { message: Value },
    #[serde(rename = "message_end")]
    MessageEnd { message: Value },
    #[serde(rename = "tool_execution_start")]
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default)]
        args: Option<Value>,
    },
    #[serde(rename = "tool_execution_update")]
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default)]
        args: Option<Value>,
        #[serde(default, rename = "partialResult")]
        partial_result: Option<Value>,
    },
    #[serde(rename = "tool_execution_end")]
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default, rename = "isError")]
        is_error: bool,
    },
    #[serde(rename = "extension_ui_request")]
    ExtensionUiRequest {
        id: String,
        method: String,
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(rename = "extension_error")]
    ExtensionError { error: String },
    #[serde(rename = "compaction_start")]
    CompactionStart { reason: Option<String> },
    #[serde(rename = "compaction_end")]
    CompactionEnd {
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(rename = "auto_retry_start")]
    AutoRetryStart {
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(rename = "auto_retry_end")]
    AutoRetryEnd {
        #[serde(default)]
        success: Option<bool>,
        #[serde(flatten)]
        extra: Value,
    },
    #[serde(untagged)]
    Unknown(Value),
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum PiRpcAssistantMessageEvent {
    #[serde(rename = "text_delta")]
    TextDelta { delta: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { delta: String },
    #[serde(untagged)]
    Other(Value),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename = "extension_ui_response")]
pub struct PiRpcExtensionUiResponse {
    pub id: String,
    #[serde(flatten)]
    pub outcome: PiRpcExtensionUiOutcome,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PiRpcExtensionUiOutcome {
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled { cancelled: bool },
}

impl PiRpcExtensionUiResponse {
    pub fn value(id: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            outcome: PiRpcExtensionUiOutcome::Value {
                value: value.into(),
            },
        }
    }

    pub fn confirmed(id: impl Into<String>, confirmed: bool) -> Self {
        Self {
            id: id.into(),
            outcome: PiRpcExtensionUiOutcome::Confirmed { confirmed },
        }
    }

    pub fn cancelled(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            outcome: PiRpcExtensionUiOutcome::Cancelled { cancelled: true },
        }
    }
}

#[derive(Debug)]
enum PendingResponse {
    Response(PiRpcResponse),
    Shutdown(String),
}

#[derive(Debug, Default)]
struct RpcState {
    pending: HashMap<RpcRequestId, oneshot::Sender<PendingResponse>>,
    shutdown_reason: Option<String>,
}

#[derive(Debug)]
enum ClientOutput {
    Response(PiRpcResponse),
    Event(PiRpcEvent),
    MalformedFrame { reason: String, frame: String },
}

#[derive(Debug, Clone)]
pub struct PiRpcClient {
    next_id: Arc<AtomicU64>,
    command_tx: mpsc::UnboundedSender<String>,
    log_frame_tx: mpsc::UnboundedSender<String>,
    state: Arc<StdMutex<RpcState>>,
}

impl PiRpcClient {
    pub fn spawn_for_child<S>(
        stdin: ChildStdin,
        stdout: ChildStdout,
        event_tx: mpsc::UnboundedSender<PiRpcEvent>,
        raw_stdout_writer: S,
        cancel: CancellationToken,
    ) -> Self
    where
        S: AsyncWrite + Unpin + Send + 'static,
    {
        Self::spawn(stdin, stdout, event_tx, raw_stdout_writer, cancel)
    }

    pub(crate) fn spawn<S, W, R>(
        stdin: W,
        stdout: R,
        event_tx: mpsc::UnboundedSender<PiRpcEvent>,
        raw_stdout_writer: S,
        cancel: CancellationToken,
    ) -> Self
    where
        S: AsyncWrite + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        R: AsyncRead + Unpin + Send + 'static,
    {
        let state = Arc::new(StdMutex::new(RpcState::default()));
        let (command_tx, command_rx) = mpsc::unbounded_channel::<String>();
        let (log_frame_tx, log_frame_rx) = mpsc::unbounded_channel::<String>();
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<FrameReadResult>();

        spawn_stdout_reader(stdout, frame_tx, cancel.clone());
        spawn_frame_dispatcher(
            frame_rx,
            log_frame_rx,
            raw_stdout_writer,
            event_tx,
            state.clone(),
            cancel.clone(),
        );
        spawn_stdin_writer(stdin, command_rx, state.clone());

        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            command_tx,
            log_frame_tx,
            state,
        }
    }

    /// Write a synthetic frame into the raw log stream so the normalizer can
    /// observe lifecycle facts the server never prints (e.g. session ID).
    pub fn log_frame(&self, frame: impl Into<String>) {
        let _ = self.log_frame_tx.send(frame.into());
    }

    /// Answer an `extension_ui_request`. A response for an unknown or stale
    /// request ID is ignored by Pi, so best-effort sending is sufficient.
    pub fn send_extension_ui_response(&self, response: PiRpcExtensionUiResponse) {
        match serde_json::to_string(&response) {
            Ok(payload) => {
                let _ = self.command_tx.send(payload);
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to serialize extension UI response");
            }
        }
    }

    pub fn next_request_id(&self) -> RpcRequestId {
        RpcRequestId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    pub async fn request<T>(
        &self,
        command: PiRpcCommand,
        cancel: CancellationToken,
    ) -> Result<T, ExecutorError>
    where
        T: DeserializeOwned,
    {
        self.request_with_timeout(command, cancel, Some(Duration::from_secs(10)))
            .await
    }

    pub async fn prompt(
        &self,
        message: impl Into<String>,
        cancel: CancellationToken,
    ) -> Result<(), ExecutorError> {
        self.request_with_timeout::<Value>(
            PiRpcCommand::Prompt {
                message: message.into(),
            },
            cancel,
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn abort(&self) -> Result<(), ExecutorError> {
        self.request_with_timeout::<Value>(
            PiRpcCommand::Abort,
            CancellationToken::new(),
            Some(Duration::from_secs(2)),
        )
        .await
        .map(|_| ())
    }

    pub async fn request_with_timeout<T>(
        &self,
        command: PiRpcCommand,
        cancel: CancellationToken,
        request_timeout: Option<Duration>,
    ) -> Result<T, ExecutorError>
    where
        T: DeserializeOwned,
    {
        let label = command.label();
        let id = self.next_request_id();
        let request = PiRpcRequest::new(id, command);
        let (tx, rx) = oneshot::channel();

        {
            let mut state = lock_state(&self.state)?;
            if let Some(reason) = &state.shutdown_reason {
                return Err(protocol_error(format!(
                    "cannot send {label}; RPC client is shut down: {reason}"
                )));
            }
            state.pending.insert(id, tx);
        }

        let send_result = serde_json::to_string(&request)
            .map_err(|err| protocol_error(err.to_string()))
            .and_then(|payload| {
                self.command_tx
                    .send(payload)
                    .map_err(|_| protocol_error(format!("failed to queue {label} request")))
            });

        if let Err(err) = send_result {
            remove_pending(&self.state, id);
            return Err(err);
        }

        let response = if let Some(duration) = request_timeout {
            match timeout(duration, rx).await {
                Ok(result) => result,
                Err(_) => {
                    remove_pending(&self.state, id);
                    return Err(protocol_error(format!(
                        "timed out waiting for {label} response"
                    )));
                }
            }
        } else {
            tokio::select! {
                _ = cancel.cancelled() => {
                    remove_pending(&self.state, id);
                    return Err(protocol_error(format!("{label} request cancelled")));
                }
                result = rx => result,
            }
        };

        match response {
            Ok(PendingResponse::Response(response)) => {
                if response.id != id {
                    return Err(protocol_error(format!(
                        "response ID mismatch for {label}: expected {id}, got {}",
                        response.id
                    )));
                }
                if !response.success {
                    return Err(protocol_error(response.error.unwrap_or_else(|| {
                        format!("{label} request failed without an error message")
                    })));
                }
                let data = match response.data {
                    Some(data) => data,
                    None => serde_json::to_value(()).map_err(|err| {
                        protocol_error(format!("failed to encode empty response: {err}"))
                    })?,
                };
                serde_json::from_value(data).map_err(|err| {
                    protocol_error(format!("failed to decode {label} response: {err}"))
                })
            }
            Ok(PendingResponse::Shutdown(reason)) => Err(protocol_error(format!(
                "RPC client shut down while waiting for {label}: {reason}"
            ))),
            Err(_) => Err(protocol_error(format!(
                "RPC response channel dropped while waiting for {label}"
            ))),
        }
    }
}

#[derive(Debug)]
enum FrameReadResult {
    Frame(String),
    Malformed { reason: String },
    Eof,
    Error(String),
}

fn spawn_stdout_reader<R>(
    mut stdout: R,
    frame_tx: mpsc::UnboundedSender<FrameReadResult>,
    cancel: CancellationToken,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = Vec::with_capacity(8192);
        let mut chunk = [0u8; 8192];
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                read_result = stdout.read(&mut chunk) => {
                    match read_result {
                        Ok(0) => {
                            if !buffer.is_empty() {
                                let _ = frame_tx.send(FrameReadResult::Malformed {
                                    reason: "EOF before LF frame terminator".to_string(),
                                });
                            }
                            let _ = frame_tx.send(FrameReadResult::Eof);
                            break;
                        }
                        Ok(n) => {
                            buffer.extend_from_slice(&chunk[..n]);
                            let mut start = 0;
                            for (index, byte) in buffer.iter().enumerate() {
                                if *byte == b'\n' {
                                    let frame = &buffer[start..index];
                                    start = index + 1;
                                    if frame.len() > MAX_FRAME_BYTES {
                                        let _ = frame_tx.send(FrameReadResult::Malformed {
                                            reason: format!("RPC frame exceeded {MAX_FRAME_BYTES} bytes"),
                                        });
                                        continue;
                                    }
                                    let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
                                    match std::str::from_utf8(frame) {
                                        Ok(frame) => {
                                            let _ = frame_tx.send(FrameReadResult::Frame(frame.to_string()));
                                        }
                                        Err(err) => {
                                            let _ = frame_tx.send(FrameReadResult::Malformed {
                                                reason: format!("RPC frame is not UTF-8: {err}"),
                                            });
                                        }
                                    }
                                }
                            }
                            if start > 0 {
                                buffer.drain(..start);
                            }
                            if buffer.len() > MAX_FRAME_BYTES {
                                buffer.clear();
                                let _ = frame_tx.send(FrameReadResult::Malformed {
                                    reason: format!("RPC frame exceeded {MAX_FRAME_BYTES} bytes before LF"),
                                });
                            }
                        }
                        Err(err) => {
                            let _ = frame_tx.send(FrameReadResult::Error(err.to_string()));
                            break;
                        }
                    }
                }
            }
        }
    });
}

fn spawn_frame_dispatcher<S>(
    mut frame_rx: mpsc::UnboundedReceiver<FrameReadResult>,
    mut log_frame_rx: mpsc::UnboundedReceiver<String>,
    mut raw_stdout_writer: S,
    event_tx: mpsc::UnboundedSender<PiRpcEvent>,
    state: Arc<StdMutex<RpcState>>,
    _cancel: CancellationToken,
) where
    S: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            // Synthetic harness frames must interleave with server frames but
            // never wait for them: poll the log channel without blocking the
            // stdout reader queue.
            while let Ok(frame) = log_frame_rx.try_recv() {
                let _ = raw_stdout_writer.write_all(frame.as_bytes()).await;
                let _ = raw_stdout_writer.write_all(b"\n").await;
                let _ = raw_stdout_writer.flush().await;
            }
            let Some(frame) = frame_rx.recv().await else {
                break;
            };
            match frame {
                FrameReadResult::Frame(raw) => {
                    let _ = raw_stdout_writer.write_all(raw.as_bytes()).await;
                    let _ = raw_stdout_writer.write_all(b"\n").await;
                    let _ = raw_stdout_writer.flush().await;
                    if raw.trim().is_empty() {
                        continue;
                    }
                    match parse_output(&raw) {
                        Ok(ClientOutput::Response(response)) => {
                            resolve_pending(
                                &state,
                                response.id,
                                PendingResponse::Response(response),
                            );
                        }
                        Ok(ClientOutput::Event(event)) => {
                            let _ = event_tx.send(event);
                        }
                        Ok(ClientOutput::MalformedFrame { reason, frame }) => {
                            tracing::warn!(reason, frame, "malformed Pi RPC frame");
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, frame = %raw, "failed to parse Pi RPC frame");
                        }
                    }
                }
                FrameReadResult::Malformed { reason } => {
                    tracing::warn!(reason, "malformed Pi RPC stream frame");
                }
                FrameReadResult::Error(reason) => {
                    shutdown_pending(&state, reason);
                    break;
                }
                FrameReadResult::Eof => {
                    shutdown_pending(&state, "stdout EOF".to_string());
                    break;
                }
            }
        }
    });
}

fn spawn_stdin_writer<W>(
    mut stdin: W,
    mut command_rx: mpsc::UnboundedReceiver<String>,
    state: Arc<StdMutex<RpcState>>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(payload) = command_rx.recv().await {
            let result = async {
                stdin.write_all(payload.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            }
            .await;
            if let Err(err) = result {
                shutdown_pending(&state, format!("failed writing RPC request: {err}"));
                break;
            }
        }
    });
}

fn parse_output(raw: &str) -> Result<ClientOutput, ExecutorError> {
    let value: Value = serde_json::from_str(raw).map_err(|err| protocol_error(err.to_string()))?;
    let type_ = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match type_ {
        "response" => serde_json::from_value(value)
            .map(ClientOutput::Response)
            .map_err(|err| protocol_error(err.to_string())),
        _ => match serde_json::from_value::<PiRpcEvent>(value.clone()) {
            Ok(event) => Ok(ClientOutput::Event(event)),
            Err(err) => Ok(ClientOutput::MalformedFrame {
                reason: err.to_string(),
                frame: raw.chars().take(240).collect(),
            }),
        },
    }
}

fn resolve_pending(state: &Arc<StdMutex<RpcState>>, id: RpcRequestId, response: PendingResponse) {
    let sender = match state.lock() {
        Ok(mut state) => state.pending.remove(&id),
        Err(_) => None,
    };
    if let Some(sender) = sender {
        let _ = sender.send(response);
    }
}

fn remove_pending(state: &Arc<StdMutex<RpcState>>, id: RpcRequestId) {
    if let Ok(mut state) = state.lock() {
        state.pending.remove(&id);
    }
}

fn shutdown_pending(state: &Arc<StdMutex<RpcState>>, reason: String) {
    let pending = match state.lock() {
        Ok(mut state) => {
            if state.shutdown_reason.is_none() {
                state.shutdown_reason = Some(reason.clone());
            }
            state.pending.drain().collect::<Vec<_>>()
        }
        Err(_) => Vec::new(),
    };
    for (_, sender) in pending {
        let _ = sender.send(PendingResponse::Shutdown(reason.clone()));
    }
}

fn lock_state(
    state: &Arc<StdMutex<RpcState>>,
) -> Result<std::sync::MutexGuard<'_, RpcState>, ExecutorError> {
    state
        .lock()
        .map_err(|_| protocol_error("RPC state lock is poisoned"))
}

fn protocol_error(message: impl Into<String>) -> ExecutorError {
    ExecutorError::Io(io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_serialization_includes_request_id() {
        let request = PiRpcRequest::new(
            RpcRequestId(7),
            PiRpcCommand::Prompt {
                message: "hello".to_string(),
            },
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["type"], "prompt");
        assert_eq!(value["message"], "hello");
    }

    #[test]
    fn event_unknown_variant_is_forward_compatible() {
        let event: PiRpcEvent = serde_json::from_str(r#"{"type":"future_event","x":1}"#).unwrap();
        assert!(matches!(event, PiRpcEvent::Unknown(_)));
    }
}
