//! Session-level conversation history from agents' native session files.
//!
//! Completed coding-agent history is read once per Vibe session from the
//! agent's own materialized session file (via `executors::history` providers).
//! Setup/cleanup script processes still render from raw logs. Raw stdout is
//! never re-normalized here — that path was lossy (broadcast lag) and
//! quadratic (per-delta patches). Plan: docs/exec-plan/agents/native-agent-session-history.md

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{LazyLock, RwLock},
};

use chrono::{DateTime, Utc};
use db::models::{
    coding_agent_turn::CodingAgentTurn,
    execution_process::{
        ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus,
    },
    execution_process_logs::ExecutionProcessLogs,
    session::Session,
    workspace::Workspace,
};
use executors::{
    actions::{ExecutorAction, ExecutorActionType},
    executors::BaseCodingAgent,
    history::{
        FileFingerprint, NativeHistoryError, TailPolicy, native_history_provider,
    },
    logs::{
        ActionType, CommandExitStatus, CommandRunResult, NormalizedEntry, NormalizedEntryType,
        ToolStatus,
    },
};
use sqlx::SqlitePool;
use utils::log_msg::LogMsg;
use uuid::Uuid;

/// executors::logs::utils::patch::PatchType is the frontend's entry wrapper.
use executors::logs::utils::patch::PatchType;

/// Cache native reads by file fingerprint; scripts are rebuilt per call
/// (cheap) and never cached.
static NATIVE_HISTORY_CACHE: LazyLock<RwLock<HashMap<CacheKey, Vec<NormalizedEntry>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    session_id: Uuid,
    agent_session_id: String,
    fingerprint: FileFingerprint,
}

/// Load the full completed-session conversation: native agent history merged
/// with setup/cleanup script tool cards, ordered by process creation time.
pub async fn get_conversation_history(
    pool: &SqlitePool,
    session_id: Uuid,
) -> Result<Vec<PatchType>, NativeHistoryError> {
    let session = Session::find_by_id(pool, session_id)
        .await
        .map_err(|e| NativeHistoryError::Corrupt {
            path: std::path::PathBuf::from("<db>"),
            reason: format!("failed to load session: {e}"),
        })?
        .ok_or(NativeHistoryError::SessionIdMissing)?;

    let processes = ExecutionProcess::find_by_session_id(pool, session_id, false)
        .await
        .map_err(|e| NativeHistoryError::Corrupt {
            path: std::path::PathBuf::from("<db>"),
            reason: format!("failed to list execution processes: {e}"),
        })?;

    let mut blocks: Vec<(DateTime<Utc>, Vec<PatchType>)> = Vec::new();

    // Script tool cards from raw logs (setup/cleanup/tool-install).
    for process in &processes {
        if matches!(process.status, ExecutionProcessStatus::Running) {
            continue;
        }
        let Some((context, script)) = script_request(process) else {
            continue;
        };
        let entries = build_script_entries(pool, process, context, &script).await?;
        if !entries.is_empty() {
            blocks.push((process.created_at, entries));
        }
    }

    let coding_processes: Vec<&ExecutionProcess> = processes
        .iter()
        .filter(|p| p.run_reason == ExecutionProcessRunReason::CodingAgent)
        .collect();
    let has_running_coding = coding_processes
        .iter()
        .any(|p| matches!(p.status, ExecutionProcessStatus::Running));
    let first_coding_at = coding_processes.iter().map(|p| p.created_at).min();

    if coding_processes
        .iter()
        .any(|p| !matches!(p.status, ExecutionProcessStatus::Running))
    {
        let native = load_native_entries(pool, &session, has_running_coding).await?;
        if !native.is_empty() {
            blocks.push((first_coding_at.unwrap_or_else(Utc::now), native));
        }
    }

    Ok(merge_blocks(blocks))
}

/// Visible for testing: deterministic, stable merge of entry blocks by their
/// sort key. Equal keys keep insertion order (scripts before native content).
pub fn merge_blocks(mut blocks: Vec<(DateTime<Utc>, Vec<PatchType>)>) -> Vec<PatchType> {
    blocks.sort_by_key(|(sort_key, _)| *sort_key);
    blocks
        .into_iter()
        .flat_map(|(_, entries)| entries)
        .collect()
}

fn script_request(process: &ExecutionProcess) -> Option<(ScriptContextKind, String)> {
    let action = &process.executor_action.0;
    let ExecutorAction { typ, .. } = action;
    let ExecutorActionType::ScriptRequest(request) = typ else {
        return None;
    };
    let kind = match request.context {
        executors::actions::script::ScriptContext::SetupScript => ScriptContextKind::Setup,
        executors::actions::script::ScriptContext::CleanupScript => ScriptContextKind::Cleanup,
        executors::actions::script::ScriptContext::ToolInstallScript => {
            ScriptContextKind::ToolInstall
        }
        executors::actions::script::ScriptContext::DevServer => return None,
    };
    Some((kind, request.script.clone()))
}

enum ScriptContextKind {
    Setup,
    Cleanup,
    ToolInstall,
}

impl ScriptContextKind {
    fn tool_name(&self) -> &'static str {
        match self {
            Self::Setup => "Setup Script",
            Self::Cleanup => "Cleanup Script",
            Self::ToolInstall => "Tool Install Script",
        }
    }
}

async fn build_script_entries(
    pool: &SqlitePool,
    process: &ExecutionProcess,
    context: ScriptContextKind,
    script: &str,
) -> Result<Vec<PatchType>, NativeHistoryError> {
    let records = ExecutionProcessLogs::find_by_execution_id(pool, process.id)
        .await
        .map_err(|e| NativeHistoryError::Corrupt {
            path: std::path::PathBuf::from("<db>"),
            reason: format!("failed to load script logs: {e}"),
        })?;
    if records.is_empty() {
        return Ok(vec![]);
    }
    let messages = ExecutionProcessLogs::parse_logs(&records).map_err(|e| {
        NativeHistoryError::Corrupt {
            path: std::path::PathBuf::from("<db>"),
            reason: format!("failed to parse script logs: {e}"),
        }
    })?;
    let output = messages
        .iter()
        .filter_map(|msg| match msg {
            LogMsg::Stdout(text) | LogMsg::Stderr(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let exit_code = process.exit_code.map(|code| code as i32);
    let status = if exit_code == Some(0) {
        ToolStatus::Success
    } else {
        ToolStatus::Failed
    };
    let tool_name = context.tool_name();
    Ok(vec![PatchType::NormalizedEntry(NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::ToolUse {
            tool_name: tool_name.to_string(),
            action_type: ActionType::CommandRun {
                command: script.to_string(),
                result: Some(CommandRunResult {
                    exit_status: exit_code.map(|code| CommandExitStatus::ExitCode { code }),
                    output: Some(output),
                }),
            },
            status,
        },
        content: tool_name.to_string(),
        metadata: None,
    })])
}

async fn load_native_entries(
    pool: &SqlitePool,
    session: &Session,
    has_running_coding: bool,
) -> Result<Vec<PatchType>, NativeHistoryError> {
    let agent_session_id = CodingAgentTurn::find_latest_session_info(pool, session.id)
        .await
        .map_err(|e| NativeHistoryError::Corrupt {
            path: std::path::PathBuf::from("<db>"),
            reason: format!("failed to load coding agent turns: {e}"),
        })?
        .map(|info| info.session_id)
        .ok_or(NativeHistoryError::SessionIdMissing)?;

    let executor = session
        .executor
        .as_deref()
        .ok_or(NativeHistoryError::SessionIdMissing)?;
    let agent = BaseCodingAgent::from_str(executor).map_err(|_| {
        NativeHistoryError::FormatUnsupported(format!("executor '{executor}' is not supported"))
    })?;
    let provider = native_history_provider(agent);

    // Providers locate by session id alone; cwd is only a scan hint.
    let cwd = Workspace::find_by_id(pool, session.workspace_id)
        .await
        .ok()
        .flatten()
        .and_then(|workspace| workspace.container_ref)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let path = provider.locate(&agent_session_id, &cwd).await?;
    let fingerprint = provider.fingerprint(&path)?;

    let key = CacheKey {
        session_id: session.id,
        agent_session_id: agent_session_id.clone(),
        fingerprint: fingerprint.clone(),
    };
    if let Some(cached) = NATIVE_HISTORY_CACHE.read().unwrap().get(&key) {
        return Ok(cached
            .clone()
            .into_iter()
            .map(PatchType::NormalizedEntry)
            .collect());
    }

    let tail = if has_running_coding {
        TailPolicy::Running
    } else {
        TailPolicy::Completed
    };
    let history = provider.read(&path, tail).await?;

    let mut cache = NATIVE_HISTORY_CACHE.write().unwrap();
    if cache.len() >= MAX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(key, history.entries.clone());
    drop(cache);
    Ok(history
        .entries
        .into_iter()
        .map(PatchType::NormalizedEntry)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str) -> PatchType {
        PatchType::NormalizedEntry(NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::SystemMessage,
            content: content.to_string(),
            metadata: None,
        })
    }

    fn content_of(patch: &PatchType) -> String {
        let PatchType::NormalizedEntry(entry) = patch else {
            panic!("expected normalized entry");
        };
        entry.content.clone()
    }

    /// The merge contract: one block per source, stable-sorted by process
    /// creation time, so setup scripts precede and cleanup scripts follow the
    /// native conversation without interleaving duplicates.
    #[test]
    fn native_history_merge_blocks_orders_by_time_and_preserves_content() {
        let t0 = DateTime::parse_from_rfc3339("2026-08-12T01:00:00Z")
            .unwrap()
            .to_utc();
        let t1 = DateTime::parse_from_rfc3339("2026-08-12T01:05:00Z")
            .unwrap()
            .to_utc();
        let t2 = DateTime::parse_from_rfc3339("2026-08-12T01:10:00Z")
            .unwrap()
            .to_utc();

        let merged = merge_blocks(vec![
            (t2, vec![entry("cleanup")]),
            (t1, vec![entry("native-1"), entry("native-2")]),
            (t0, vec![entry("setup")]),
        ]);
        let texts: Vec<String> = merged.iter().map(content_of).collect();
        assert_eq!(texts, vec!["setup", "native-1", "native-2", "cleanup"]);
    }
}
