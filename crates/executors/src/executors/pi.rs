use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use workspace_utils::{approvals::ApprovalStatus, msg_store::MsgStore};

use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorExitResult, SlashCommandDescription,
        SpawnedChild, StandardCodingAgentExecutor,
        pi::rpc::{
            PiRpcClient, PiRpcCommand, PiRpcCommands, PiRpcEvent, PiRpcExtensionUiResponse,
            PiRpcSessionState, PiRpcSessionStats,
        },
    },
    logs::utils::patch,
};

pub mod normalize_logs;
pub mod rpc;

#[cfg(test)]
pub(crate) mod fake_rpc;

const BRIDGE_SOURCE: &str = include_str!("pi/vibe-kanban-bridge.mjs");
const BRIDGE_ENVELOPE_PREFIX: &str = "vk-pi-approval:";

/// Pi tool-approval policy: AUTO executes tools directly; SUPERVISED routes
/// every mutating or unknown tool through the Vibe Kanban approval UI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PiPermissionPolicy {
    #[default]
    Auto,
    Supervised,
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Pi {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<PiPermissionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_project: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_user_extensions: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[schemars(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

/// Handle to a running `pi --mode rpc` child and its protocol client.
struct PiProcess {
    child: AsyncGroupChild,
    client: PiRpcClient,
    events: tokio::sync::mpsc::UnboundedReceiver<PiRpcEvent>,
    cancel: CancellationToken,
}

/// Machine-readable envelope emitted by the bundled permission bridge as the
/// `confirm` dialog title in RPC mode.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolApprovalEnvelope {
    tool_call_id: Option<String>,
    tool_name: String,
    summary: Option<String>,
}

impl Pi {
    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new("pi").extend_params(["--mode", "rpc"]);

        match self.trust_project.unwrap_or(false) {
            true => builder = builder.extend_params(["--approve"]),
            false => builder = builder.extend_params(["--no-approve"]),
        }

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }
        if let Some(thinking) = &self.thinking {
            builder = builder.extend_params(["--thinking", thinking.as_str()]);
        }

        if !self.load_user_extensions.unwrap_or(!self.is_supervised()) {
            builder = builder.extend_params(["--no-extensions"]);
        }

        apply_overrides(builder, &self.cmd)
    }

    fn permission_policy(&self) -> PiPermissionPolicy {
        self.permission_policy
            .clone()
            .unwrap_or(PiPermissionPolicy::Auto)
    }

    fn is_supervised(&self) -> bool {
        matches!(self.permission_policy(), PiPermissionPolicy::Supervised)
    }

    /// Write the bundled permission bridge to a managed temp file. The file is
    /// world-readable and contains no secrets; temp-dir cleanup is left to the OS.
    fn materialize_bridge() -> Result<PathBuf, ExecutorError> {
        let dir = std::env::temp_dir().join("vibe-kanban");
        std::fs::create_dir_all(&dir).map_err(ExecutorError::Io)?;
        let path = dir.join(format!("pi-bridge-{}.mjs", uuid::Uuid::new_v4()));
        let staging = dir.join(format!(".pi-bridge-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&staging, BRIDGE_SOURCE).map_err(ExecutorError::Io)?;
        std::fs::rename(&staging, &path).map_err(ExecutorError::Io)?;
        Ok(path)
    }

    async fn launch_rpc(
        &self,
        current_dir: &Path,
        resume_session: Option<&str>,
        env: Option<&ExecutionEnv>,
    ) -> Result<PiProcess, ExecutorError> {
        let mut builder = self.build_command_builder()?;
        if let Some(session_id) = resume_session {
            builder = builder.extend_params(["--session", session_id]);
        }
        if self.is_supervised() {
            let bridge = Self::materialize_bridge()?;
            builder = builder.extend_params(["--extension", bridge.to_string_lossy().as_ref()]);
        }
        let command_parts = builder.build_initial()?;
        let (program_path, args) = command_parts.into_resolved().await?;

        let mut process = tokio::process::Command::new(program_path);
        process
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .env("NO_COLOR", "1")
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .env(
                "VK_PI_PERMISSION_POLICY",
                if self.is_supervised() {
                    "SUPERVISED"
                } else {
                    "AUTO"
                },
            )
            .args(&args);

        if let Some(env) = env {
            env.clone()
                .with_profile(&self.cmd)
                .apply_to_command(&mut process);
        }

        let mut child = process.group_spawn()?;
        let stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Pi RPC process missing stdin"))
        })?;
        let stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Pi RPC process missing stdout"))
        })?;
        let raw_stdout_writer = crate::stdout_dup::create_stdout_pipe_writer(&mut child)?;
        let cancel = CancellationToken::new();
        let (event_tx, events) = tokio::sync::mpsc::unbounded_channel::<PiRpcEvent>();

        let client = PiRpcClient::spawn_for_child(
            stdin,
            stdout,
            event_tx,
            raw_stdout_writer,
            cancel.clone(),
        );

        Ok(PiProcess {
            child,
            client,
            events,
            cancel,
        })
    }

    async fn spawn_inner(
        &self,
        current_dir: &Path,
        prompt: &str,
        resume_session: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let process = self
            .launch_rpc(current_dir, resume_session, Some(env))
            .await?;
        let mut events = process.events;
        let cancel = process.cancel.clone();
        let client = process.client.clone();
        let approvals = self.approvals.clone();
        let prompt = self.append_prompt.combine_prompt(prompt);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ExecutorExitResult>();

        let client_for_task = client.clone();
        let cancel_for_task = cancel.clone();
        tokio::spawn(async move {
            let result = async {
                let state: PiRpcSessionState = client_for_task
                    .request(PiRpcCommand::GetState, cancel_for_task.clone())
                    .await?;
                let session_id = state.session_id.ok_or_else(|| {
                    ExecutorError::Io(std::io::Error::other("Pi RPC did not report a session ID"))
                })?;

                // Synthetic lifecycle marker: the normalizer publishes this as
                // Vibe's execution session ID.
                client_for_task.log_frame(
                    serde_json::json!({
                        "type": "vk_pi_session_start",
                        "sessionId": session_id,
                    })
                    .to_string(),
                );

                client_for_task
                    .prompt(prompt, cancel_for_task.clone())
                    .await?;

                let mut saw_settled = false;
                while let Some(event) = events.recv().await {
                    match event {
                        PiRpcEvent::AgentSettled => {
                            saw_settled = true;
                            break;
                        }
                        PiRpcEvent::ExtensionError { error } => {
                            tracing::warn!(error, "Pi extension error");
                        }
                        PiRpcEvent::AutoRetryEnd {
                            success: Some(false),
                            ..
                        } => {
                            return Err(ExecutorError::Io(std::io::Error::other(
                                "Pi automatic retry failed",
                            )));
                        }
                        PiRpcEvent::ExtensionUiRequest { id, extra, .. } => {
                            handle_extension_ui_request(
                                &client_for_task,
                                approvals.as_ref(),
                                &cancel_for_task,
                                id,
                                extra,
                            )
                            .await;
                        }
                        _ => {}
                    }
                }

                if !saw_settled {
                    // The event channel closed without agent_settled. If the
                    // server is still responsive and idle this was an
                    // immediately-handled command; otherwise it is a failure.
                    let state: PiRpcSessionState = client_for_task
                        .request(PiRpcCommand::GetState, CancellationToken::new())
                        .await?;
                    if state.is_streaming || state.is_compacting {
                        return Err(ExecutorError::Io(std::io::Error::other(
                            "Pi stdout closed before the run settled",
                        )));
                    }
                }

                // Best-effort usage reporting; stats unavailability is not fatal.
                match client_for_task
                    .request::<PiRpcSessionStats>(
                        PiRpcCommand::GetSessionStats,
                        CancellationToken::new(),
                    )
                    .await
                {
                    Ok(stats) => {
                        if let Some(usage) = stats.context_usage {
                            client_for_task.log_frame(
                                serde_json::json!({
                                    "type": "vk_pi_token_usage",
                                    "tokens": usage.tokens,
                                    "contextWindow": usage.context_window,
                                })
                                .to_string(),
                            );
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "Pi session stats unavailable");
                    }
                }

                Ok(())
            }
            .await;

            let exit = match result {
                Ok(()) => ExecutorExitResult::Success,
                Err(err) => {
                    if cancel_for_task.is_cancelled() {
                        ExecutorExitResult::Success
                    } else {
                        tracing::error!(error = %err, "Pi executor failed");
                        ExecutorExitResult::Failure
                    }
                }
            };
            let _ = exit_tx.send(exit);
        });

        // Graceful cancellation: ask Pi to abort; the container kills the
        // process group after its timeout if abort does not stop it.
        let cancel_for_abort = cancel.clone();
        let client_for_abort = client.clone();
        tokio::spawn(async move {
            cancel_for_abort.cancelled().await;
            let _ = client_for_abort.abort().await;
        });

        Ok(SpawnedChild {
            child: process.child,
            exit_signal: Some(exit_rx),
            cancel: Some(cancel),
        })
    }
}

async fn handle_extension_ui_request(
    client: &PiRpcClient,
    approvals: Option<&Arc<dyn ExecutorApprovalService>>,
    cancel: &CancellationToken,
    id: String,
    extra: serde_json::Value,
) {
    let title = extra.get("title").and_then(serde_json::Value::as_str);

    if let Some(envelope_json) = title.and_then(|t| t.strip_prefix(BRIDGE_ENVELOPE_PREFIX)) {
        match serde_json::from_str::<ToolApprovalEnvelope>(envelope_json) {
            Ok(envelope) => {
                let approved = match approvals {
                    Some(service) => {
                        let tool_input = envelope
                            .summary
                            .as_ref()
                            .map(|s| serde_json::Value::String(s.clone()))
                            .unwrap_or(serde_json::Value::Null);
                        match service
                            .request_tool_approval(
                                &envelope.tool_name,
                                tool_input,
                                envelope.tool_call_id.as_deref().unwrap_or_default(),
                                cancel.clone(),
                            )
                            .await
                        {
                            Ok(status) => matches!(status, ApprovalStatus::Approved),
                            Err(err) => {
                                tracing::warn!(error = %err, "failed to create Pi tool approval");
                                false
                            }
                        }
                    }
                    // Supervised without an approval backend must fail closed.
                    None => false,
                };
                client
                    .send_extension_ui_response(PiRpcExtensionUiResponse::confirmed(id, approved));
            }
            Err(err) => {
                tracing::warn!(error = %err, "malformed Pi approval envelope");
                client.send_extension_ui_response(PiRpcExtensionUiResponse::confirmed(id, false));
            }
        }
        return;
    }

    // Third-party extension UI requests (select/input/editor/notify/status/
    // widget/title/editor text) have no Vibe equivalent yet. Answer dialog
    // methods with a cancellation so the run cannot hang indefinitely.
    client.send_extension_ui_response(PiRpcExtensionUiResponse::cancelled(id));
}

#[async_trait]
impl StandardCodingAgentExecutor for Pi {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn available_slash_commands(
        &self,
        workdir: &Path,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        // Probe a short-lived RPC process for the commands this Pi install
        // actually loads. Discovery failures degrade to an empty list.
        let commands = async {
            let mut process = self.launch_rpc(workdir, None, None).await?;
            let cancel = process.cancel.clone();
            let result = process
                .client
                .request::<PiRpcCommands>(PiRpcCommand::GetCommands, cancel.clone())
                .await;
            cancel.cancel();
            let _ = process.child.inner().kill().await;
            result
        }
        .await
        .map(|commands| {
            let mut slash_commands: Vec<SlashCommandDescription> = commands
                .commands
                .into_iter()
                .map(|command| SlashCommandDescription {
                    name: command.name.trim_start_matches('/').to_string(),
                    description: command.description,
                })
                .collect();
            if !slash_commands
                .iter()
                .any(|command| command.name == "compact")
            {
                slash_commands.insert(
                    0,
                    SlashCommandDescription {
                        name: "compact".to_string(),
                        description: Some(
                            "Summarize conversation to prevent hitting the context limit"
                                .to_string(),
                        ),
                    },
                );
            }
            slash_commands
        })
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "Pi slash command discovery failed");
            Vec::new()
        });

        Ok(Box::pin(futures::stream::once(async move {
            patch::slash_commands(commands, false, None)
        })))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_inner(current_dir, prompt, None, env).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_inner(current_dir, prompt, Some(session_id), env)
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        normalize_logs::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        None
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let found_in_path = std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths).any(|dir| {
                    let candidate = dir.join(if cfg!(windows) { "pi.exe" } else { "pi" });
                    candidate.is_file()
                })
            })
            .unwrap_or(false);
        if found_in_path {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi_with_policy(policy: PiPermissionPolicy) -> Pi {
        Pi {
            append_prompt: AppendPrompt::default(),
            model: None,
            thinking: None,
            permission_policy: Some(policy),
            trust_project: None,
            load_user_extensions: None,
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }

    #[test]
    fn command_builder_uses_safe_defaults() {
        let builder = pi_with_policy(PiPermissionPolicy::Supervised)
            .build_command_builder()
            .unwrap();
        let args = builder.params.unwrap_or_default();
        assert!(args.windows(2).any(|pair| pair == ["--mode", "rpc"]));
        assert!(args.contains(&"--no-approve".to_string()));
        assert!(args.contains(&"--no-extensions".to_string()));
    }

    #[test]
    fn auto_mode_loads_user_extensions_by_default() {
        let builder = pi_with_policy(PiPermissionPolicy::Auto)
            .build_command_builder()
            .unwrap();
        let params = builder.params.unwrap_or_default();
        assert!(!params.contains(&"--no-extensions".to_string()));
    }

    #[test]
    fn bridge_envelope_parses() {
        let envelope: ToolApprovalEnvelope = serde_json::from_str(
            r#"{"v":1,"kind":"vk_tool_approval","toolCallId":"t1","toolName":"bash","summary":"ls"}"#,
        )
        .unwrap();
        assert_eq!(envelope.tool_name, "bash");
    }
}
