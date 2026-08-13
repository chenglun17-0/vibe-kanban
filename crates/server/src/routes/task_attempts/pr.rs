use std::path::PathBuf;

use axum::{
    Extension, Json,
    extract::{Query, State},
    response::Json as ResponseJson,
};
use db::models::{
    coding_agent_turn::CodingAgentTurn,
    execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
    execution_process_repo_state::ExecutionProcessRepoState,
    merge::{Merge, MergeStatus},
    project_repo::ProjectRepo,
    repo::{Repo, RepoError},
    session::{CreateSession, Session},
    task::{CreateTask, Task, TaskStatus},
    workspace::{CreateWorkspace, Workspace, WorkspaceError},
    workspace_repo::{CreateWorkspaceRepo, WorkspaceRepo},
};
use deployment::Deployment;
use executors::actions::{
    ExecutorAction, ExecutorActionType, coding_agent_follow_up::CodingAgentFollowUpRequest,
    coding_agent_initial::CodingAgentInitialRequest,
};
use git::{GitCliError, GitRemote, GitServiceError};
use serde::{Deserialize, Serialize};
use services::services::{
    container::ContainerService,
    git_host::{
        self, CreatePrRequest, GitHostError, GitHostProvider, ProviderKind, UnifiedPrComment,
        gitee::GeCli, github::GhCli,
    },
};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct CreatePrApiRequest {
    pub title: String,
    pub body: Option<String>,
    pub target_branch: Option<String>,
    pub draft: Option<bool>,
    pub repo_id: Uuid,
    pub source_head_sha: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct GeneratePrDescriptionRequest {
    pub repo_id: Uuid,
    pub target_branch: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct GeneratePrDescriptionResponse {
    pub execution_process_id: Uuid,
    pub source_head_sha: String,
}

#[derive(Debug, Deserialize)]
pub struct GetPrDescriptionQuery {
    pub execution_process_id: Uuid,
    pub repo_id: Uuid,
}

#[derive(Debug, Serialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum PrDescriptionStatus {
    Running,
    Failed {
        message: String,
    },
    Completed {
        title: String,
        body: String,
        source_head_sha: String,
    },
}

#[derive(Debug, Deserialize)]
struct GeneratedPrDescription {
    title: String,
    body: String,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum PrError {
    CliNotInstalled {
        provider: ProviderKind,
    },
    CliVersionUnsupported {
        provider: ProviderKind,
        found: String,
        minimum: String,
    },
    CliNotLoggedIn {
        provider: ProviderKind,
    },
    GitCliNotLoggedIn,
    GitCliNotInstalled,
    TargetBranchNotFound {
        branch: String,
    },
    SourceBranchChanged,
    UnsupportedProvider,
}

#[derive(Debug, Serialize, TS)]
pub struct AttachPrResponse {
    pub pr_attached: bool,
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_status: Option<MergeStatus>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
pub struct AttachExistingPrRequest {
    pub repo_id: Uuid,
}

#[derive(Debug, Serialize, TS)]
pub struct PrCommentsResponse {
    pub comments: Vec<UnifiedPrComment>,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum GetPrCommentsError {
    NoPrAttached,
    CliNotInstalled {
        provider: ProviderKind,
    },
    CliVersionUnsupported {
        provider: ProviderKind,
        found: String,
        minimum: String,
    },
    CliNotLoggedIn {
        provider: ProviderKind,
    },
}

#[derive(Debug, Deserialize, TS)]
pub struct GetPrCommentsQuery {
    pub repo_id: Uuid,
}

pub const DEFAULT_PR_DESCRIPTION_PROMPT: &str = r#"Analyze the committed changes in this branch relative to the target branch and generate a pull request title and description.

Return exactly one JSON object and no other text:
{"title":"concise descriptive title (Vibe Kanban)","body":"markdown description"}

The body must explain what changed, why it changed based on the task context, and important implementation or testing details. End with: This PR was written using [Vibe Kanban](https://vibekanban.com)

Do not create or update a pull request. Do not modify files, commit, or push. Your only task is to inspect the existing changes and return the JSON object."#;

fn parse_generated_pr_description(summary: &str) -> Result<GeneratedPrDescription, String> {
    let trimmed = summary.trim();
    let json = if trimmed.starts_with("```") {
        let first_newline = trimmed
            .find('\n')
            .ok_or_else(|| "AI response contains an invalid code fence".to_string())?;
        let without_opening = &trimmed[first_newline + 1..];
        let closing = without_opening
            .rfind("```")
            .ok_or_else(|| "AI response contains an unclosed code fence".to_string())?;
        without_opening[..closing].trim()
    } else {
        trimmed
    };

    let generated: GeneratedPrDescription = serde_json::from_str(json)
        .map_err(|error| format!("AI response is not valid PR description JSON: {error}"))?;
    if generated.title.trim().is_empty() || generated.body.trim().is_empty() {
        return Err("AI response must contain non-empty title and body".to_string());
    }
    if generated.title.chars().count() > 256 {
        return Err("AI-generated PR title exceeds 256 characters".to_string());
    }
    if generated.body.len() > 64 * 1024 {
        return Err("AI-generated PR body exceeds 64 KiB".to_string());
    }
    Ok(generated)
}

pub async fn generate_pr_description(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<GeneratePrDescriptionRequest>,
) -> Result<ResponseJson<ApiResponse<GeneratePrDescriptionResponse>>, ApiError> {
    let pool = &deployment.db().pool;
    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;
    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;
    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let worktree_path = PathBuf::from(container_ref).join(&repo.name);
    let git = deployment.git();
    let worktree_status = git.get_worktree_status(&worktree_path)?;
    if worktree_status.uncommitted_tracked > 0 || worktree_status.untracked > 0 {
        return Err(ApiError::Workspace(WorkspaceError::ValidationError(
            "Commit or discard working tree changes before generating a PR description".to_string(),
        )));
    }
    let source_head_sha = git.get_head_info(&worktree_path)?.oid;
    let target_branch = request
        .target_branch
        .unwrap_or_else(|| workspace_repo.target_branch.clone());

    let config = deployment.config().read().await;
    let configured_prompt = config
        .pr_auto_description_prompt
        .as_deref()
        .filter(|prompt| {
            !prompt.contains("{pr_number}")
                && !prompt.contains("{pr_url}")
                && !prompt.contains(" pr edit")
        });
    let prompt_template = configured_prompt.unwrap_or(DEFAULT_PR_DESCRIPTION_PROMPT);
    let prompt = format!(
        "{prompt_template}\n\nTarget branch: {target_branch}\nSource HEAD: {source_head_sha}\n\nRegardless of any earlier instruction, do not create or update a PR and do not modify the repository. Return only a JSON object with non-empty string fields `title` and `body`."
    );
    drop(config);

    let session = Session::find_latest_by_workspace_id(pool, workspace.id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "No coding agent session is available to generate the PR description".to_string(),
        )))?;
    let executor_profile_id =
        ExecutionProcess::latest_executor_profile_for_session(pool, session.id)
            .await?
            .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
                "No coding agent profile is available to generate the PR description".to_string(),
            )))?;
    let latest_session_info = CodingAgentTurn::find_latest_session_info(pool, session.id).await?;
    let working_dir = Some(repo.name.clone());
    let action_type = if let Some(info) = latest_session_info {
        ExecutorActionType::CodingAgentFollowUpRequest(CodingAgentFollowUpRequest {
            prompt,
            session_id: info.session_id,
            reset_to_message_id: None,
            executor_profile_id,
            working_dir,
        })
    } else {
        ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
            prompt,
            executor_profile_id,
            working_dir,
        })
    };
    let action = ExecutorAction::new(action_type, None).auxiliary();
    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &action,
            &ExecutionProcessRunReason::CodingAgent,
        )
        .await?;

    Ok(ResponseJson(ApiResponse::success(
        GeneratePrDescriptionResponse {
            execution_process_id: execution_process.id,
            source_head_sha,
        },
    )))
}

pub async fn get_pr_description(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<GetPrDescriptionQuery>,
) -> Result<ResponseJson<ApiResponse<PrDescriptionStatus>>, ApiError> {
    let pool = &deployment.db().pool;
    let process = ExecutionProcess::find_by_id(pool, query.execution_process_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "PR description generation process not found".to_string(),
        )))?;
    let session =
        Session::find_by_id(pool, process.session_id)
            .await?
            .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
                "PR description generation session not found".to_string(),
            )))?;
    if session.workspace_id != workspace.id || process.executor_action().affects_task_status {
        return Err(ApiError::Workspace(WorkspaceError::ValidationError(
            "Execution process is not a PR description generation process".to_string(),
        )));
    }

    let status = match process.status {
        ExecutionProcessStatus::Running => PrDescriptionStatus::Running,
        ExecutionProcessStatus::Failed | ExecutionProcessStatus::Killed => {
            PrDescriptionStatus::Failed {
                message: "AI agent failed to generate the PR description".to_string(),
            }
        }
        ExecutionProcessStatus::Completed => {
            let turn = CodingAgentTurn::find_by_execution_process_id(pool, process.id)
                .await?
                .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
                    "PR description generation result not found".to_string(),
                )))?;
            let Some(summary) = turn.summary else {
                return Ok(ResponseJson(ApiResponse::success(
                    PrDescriptionStatus::Running,
                )));
            };
            let generated = parse_generated_pr_description(&summary)
                .map_err(|message| ApiError::Workspace(WorkspaceError::ValidationError(message)))?;
            let workspace_repo =
                WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, query.repo_id)
                    .await?
                    .ok_or(RepoError::NotFound)?;
            let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
                .await?
                .ok_or(RepoError::NotFound)?;
            let container_ref = deployment
                .container()
                .ensure_container_exists(&workspace)
                .await?;
            let worktree_path = PathBuf::from(container_ref).join(repo.name);
            let worktree_status = deployment.git().get_worktree_status(&worktree_path)?;
            if worktree_status.uncommitted_tracked > 0 || worktree_status.untracked > 0 {
                return Ok(ResponseJson(ApiResponse::success(
                    PrDescriptionStatus::Failed {
                        message: "The working tree changed while the PR description was generated"
                            .to_string(),
                    },
                )));
            }
            let source_head_sha =
                ExecutionProcessRepoState::find_by_execution_process_id(pool, process.id)
                    .await?
                    .into_iter()
                    .find(|state| state.repo_id == query.repo_id)
                    .and_then(|state| state.before_head_commit)
                    .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
                        "Source HEAD for PR description generation is unavailable".to_string(),
                    )))?;
            PrDescriptionStatus::Completed {
                title: generated.title,
                body: generated.body,
                source_head_sha,
            }
        }
    };
    Ok(ResponseJson(ApiResponse::success(status)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_pr_description_with_or_without_fence() {
        for summary in [
            r###"{"title":"Fix status","body":"## Summary\nDone"}"###,
            "```json\n{\"title\":\"Fix status\",\"body\":\"## Summary\\nDone\"}\n```",
        ] {
            let generated = parse_generated_pr_description(summary).unwrap();
            assert_eq!(generated.title, "Fix status");
            assert_eq!(generated.body, "## Summary\nDone");
        }
    }

    #[test]
    fn rejects_empty_or_explanatory_pr_description() {
        assert!(parse_generated_pr_description(r#"{"title":"","body":"body"}"#).is_err());
        assert!(parse_generated_pr_description("Here is the JSON: {} ").is_err());
    }
}

pub async fn create_pr(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<CreatePrApiRequest>,
) -> Result<ResponseJson<ApiResponse<String, PrError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let repo_path = repo.path.clone();
    let target_branch = if let Some(branch) = request.target_branch {
        branch
    } else {
        workspace_repo.target_branch.clone()
    };

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let workspace_path = PathBuf::from(&container_ref);
    let worktree_path = workspace_path.join(&repo.name);

    let git = deployment.git();
    if let Some(expected_head) = request.source_head_sha.as_deref() {
        let current_head = git.get_head_info(&worktree_path)?.oid;
        let worktree_status = git.get_worktree_status(&worktree_path)?;
        if current_head != expected_head
            || worktree_status.uncommitted_tracked > 0
            || worktree_status.untracked > 0
        {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::SourceBranchChanged,
            )));
        }
    }
    let push_remote = git.resolve_remote_for_branch(&repo_path, &workspace.branch)?;

    // Try to get the remote from the branch name (works for remote-tracking branches like "upstream/main").
    // Fall back to push_remote if the branch doesn't exist locally or isn't a remote-tracking branch.
    let (target_remote, base_branch) =
        match git.get_remote_from_branch_name(&repo_path, &target_branch) {
            Ok(remote) => {
                let branch = target_branch
                    .strip_prefix(&format!("{}/", remote.name))
                    .unwrap_or(&target_branch);
                (remote, branch.to_string())
            }
            Err(_) => (push_remote.clone(), target_branch.clone()),
        };

    match git.check_remote_branch_exists(&repo_path, &target_remote.url, &base_branch) {
        Ok(false) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::TargetBranchNotFound {
                    branch: target_branch.clone(),
                },
            )));
        }
        Err(GitServiceError::GitCLI(GitCliError::AuthFailed(_))) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::GitCliNotLoggedIn,
            )));
        }
        Err(GitServiceError::GitCLI(GitCliError::NotAvailable)) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::GitCliNotInstalled,
            )));
        }
        Err(e) => return Err(ApiError::GitService(e)),
        Ok(true) => {}
    }

    if let Err(e) = git.push_to_remote(&worktree_path, &workspace.branch, false) {
        tracing::error!("Failed to push branch to remote: {}", e);
        match e {
            GitServiceError::GitCLI(GitCliError::AuthFailed(_)) => {
                return Ok(ResponseJson(ApiResponse::error_with_data(
                    PrError::GitCliNotLoggedIn,
                )));
            }
            GitServiceError::GitCLI(GitCliError::NotAvailable) => {
                return Ok(ResponseJson(ApiResponse::error_with_data(
                    PrError::GitCliNotInstalled,
                )));
            }
            _ => return Err(ApiError::GitService(e)),
        }
    }

    let git_host = match git_host::GitHostService::from_url(&target_remote.url) {
        Ok(host) => host,
        Err(GitHostError::UnsupportedProvider) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::UnsupportedProvider,
            )));
        }
        Err(GitHostError::CliNotInstalled { provider }) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::CliNotInstalled { provider },
            )));
        }
        Err(e) => return Err(ApiError::GitHost(e)),
    };

    let provider = git_host.provider_kind();

    // Create the PR
    let pr_request = CreatePrRequest {
        title: request.title.clone(),
        body: request.body.clone(),
        head_branch: workspace.branch.clone(),
        base_branch: base_branch.clone(),
        draft: request.draft,
        head_repo_url: Some(push_remote.url.clone()),
    };

    match git_host
        .create_pr(&repo_path, &target_remote.url, &pr_request)
        .await
    {
        Ok(pr_info) => {
            // Update the workspace with PR information
            if let Err(e) = Merge::create_pr(
                pool,
                workspace.id,
                workspace_repo.repo_id,
                &base_branch,
                pr_info.number,
                &pr_info.url,
            )
            .await
            {
                tracing::error!("Failed to update workspace PR status: {}", e);
            }

            // Auto-open PR in browser
            if let Err(e) = utils::browser::open_browser(&pr_info.url).await {
                tracing::warn!("Failed to open PR in browser: {}", e);
            }

            deployment
                .track_if_analytics_allowed(
                    "pr_created",
                    serde_json::json!({
                        "workspace_id": workspace.id.to_string(),
                        "provider": format!("{:?}", provider),
                    }),
                )
                .await;

            Ok(ResponseJson(ApiResponse::success(pr_info.url)))
        }
        Err(e) => {
            tracing::error!(
                "Failed to create PR for attempt {} using {:?}: {}",
                workspace.id,
                provider,
                e
            );
            match &e {
                GitHostError::CliNotInstalled { provider } => Ok(ResponseJson(
                    ApiResponse::error_with_data(PrError::CliNotInstalled {
                        provider: *provider,
                    }),
                )),
                GitHostError::CliVersionUnsupported {
                    provider,
                    found,
                    minimum,
                } => Ok(ResponseJson(ApiResponse::error_with_data(
                    PrError::CliVersionUnsupported {
                        provider: *provider,
                        found: found.clone(),
                        minimum: minimum.clone(),
                    },
                ))),
                GitHostError::AuthFailed(_) => Ok(ResponseJson(ApiResponse::error_with_data(
                    PrError::CliNotLoggedIn { provider },
                ))),
                _ => Err(ApiError::GitHost(e)),
            }
        }
    }
}

pub async fn attach_existing_pr(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<AttachExistingPrRequest>,
) -> Result<ResponseJson<ApiResponse<AttachPrResponse, PrError>>, ApiError> {
    let pool = &deployment.db().pool;

    let task = workspace
        .parent_task(pool)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::TaskNotFound))?;

    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    // Check if PR already attached for this repo
    let merges = Merge::find_by_workspace_and_repo_id(pool, workspace.id, request.repo_id).await?;
    if let Some(Merge::Pr(pr_merge)) = merges.into_iter().next() {
        return Ok(ResponseJson(ApiResponse::success(AttachPrResponse {
            pr_attached: true,
            pr_url: Some(pr_merge.pr_info.url.clone()),
            pr_number: Some(pr_merge.pr_info.number),
            pr_status: Some(pr_merge.pr_info.status.clone()),
        })));
    }

    let git = deployment.git();
    let remote = git.resolve_remote_for_branch(&repo.path, &workspace_repo.target_branch)?;

    let git_host = match git_host::GitHostService::from_url(&remote.url) {
        Ok(host) => host,
        Err(GitHostError::UnsupportedProvider) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::UnsupportedProvider,
            )));
        }
        Err(GitHostError::CliNotInstalled { provider }) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::CliNotInstalled { provider },
            )));
        }
        Err(e) => return Err(ApiError::GitHost(e)),
    };

    let provider = git_host.provider_kind();

    // List all PRs for branch (open, closed, and merged)
    let prs = match git_host
        .list_prs_for_branch(&repo.path, &remote.url, &workspace.branch)
        .await
    {
        Ok(prs) => prs,
        Err(GitHostError::CliNotInstalled { provider }) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::CliNotInstalled { provider },
            )));
        }
        Err(GitHostError::AuthFailed(_)) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                PrError::CliNotLoggedIn { provider },
            )));
        }
        Err(e) => return Err(ApiError::GitHost(e)),
    };

    // Take the first PR (prefer open, but also accept merged/closed)
    if let Some(pr_info) = prs.into_iter().next() {
        // Save PR info to database
        let merge = Merge::create_pr(
            pool,
            workspace.id,
            workspace_repo.repo_id,
            &workspace_repo.target_branch,
            pr_info.number,
            &pr_info.url,
        )
        .await?;

        // Update status if not open
        if !matches!(pr_info.status, MergeStatus::Open) {
            Merge::update_status(
                pool,
                merge.id,
                pr_info.status.clone(),
                pr_info.merge_commit_sha.clone(),
            )
            .await?;
        }

        // If PR is merged, mark task as done and archive workspace
        if matches!(pr_info.status, MergeStatus::Merged) {
            Task::update_status(pool, task.id, TaskStatus::Done).await?;
            if !workspace.pinned {
                Workspace::set_archived(pool, workspace.id, true).await?;
            }
        }

        Ok(ResponseJson(ApiResponse::success(AttachPrResponse {
            pr_attached: true,
            pr_url: Some(pr_info.url),
            pr_number: Some(pr_info.number),
            pr_status: Some(pr_info.status),
        })))
    } else {
        Ok(ResponseJson(ApiResponse::success(AttachPrResponse {
            pr_attached: false,
            pr_url: None,
            pr_number: None,
            pr_status: None,
        })))
    }
}

pub async fn get_pr_comments(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<GetPrCommentsQuery>,
) -> Result<ResponseJson<ApiResponse<PrCommentsResponse, GetPrCommentsError>>, ApiError> {
    let pool = &deployment.db().pool;

    // Look up the specific repo using the multi-repo pattern
    let workspace_repo =
        WorkspaceRepo::find_by_workspace_and_repo_id(pool, workspace.id, query.repo_id)
            .await?
            .ok_or(RepoError::NotFound)?;

    let repo = Repo::find_by_id(pool, workspace_repo.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    // Find the merge/PR for this specific repo
    let merges = Merge::find_by_workspace_and_repo_id(pool, workspace.id, query.repo_id).await?;

    // Ensure there's an attached PR for this repo
    let pr_info = match merges.into_iter().next() {
        Some(Merge::Pr(pr_merge)) => pr_merge.pr_info,
        _ => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                GetPrCommentsError::NoPrAttached,
            )));
        }
    };

    let git = deployment.git();
    let remote = git.resolve_remote_for_branch(&repo.path, &workspace_repo.target_branch)?;

    let git_host = match git_host::GitHostService::from_url(&remote.url) {
        Ok(host) => host,
        Err(GitHostError::CliNotInstalled { provider }) => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                GetPrCommentsError::CliNotInstalled { provider },
            )));
        }
        Err(e) => return Err(ApiError::GitHost(e)),
    };

    let provider = git_host.provider_kind();

    match git_host
        .get_pr_comments(&repo.path, &remote.url, pr_info.number)
        .await
    {
        Ok(comments) => Ok(ResponseJson(ApiResponse::success(PrCommentsResponse {
            comments,
        }))),
        Err(e) => {
            tracing::error!(
                "Failed to fetch PR comments for attempt {}, PR #{}: {}",
                workspace.id,
                pr_info.number,
                e
            );
            match &e {
                GitHostError::CliNotInstalled { provider } => Ok(ResponseJson(
                    ApiResponse::error_with_data(GetPrCommentsError::CliNotInstalled {
                        provider: *provider,
                    }),
                )),
                GitHostError::CliVersionUnsupported {
                    provider,
                    found,
                    minimum,
                } => Ok(ResponseJson(ApiResponse::error_with_data(
                    GetPrCommentsError::CliVersionUnsupported {
                        provider: *provider,
                        found: found.clone(),
                        minimum: minimum.clone(),
                    },
                ))),
                GitHostError::AuthFailed(_) => Ok(ResponseJson(ApiResponse::error_with_data(
                    GetPrCommentsError::CliNotLoggedIn { provider },
                ))),
                _ => Err(ApiError::GitHost(e)),
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, TS)]
pub struct CreateWorkspaceFromPrBody {
    pub repo_id: Uuid,
    pub pr_number: i64,
    pub pr_title: String,
    pub pr_url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub run_setup: bool,
    pub remote_name: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct CreateWorkspaceFromPrResponse {
    pub workspace: Workspace,
    pub task: Task,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum CreateFromPrError {
    PrNotFound,
    BranchFetchFailed {
        message: String,
    },
    CliNotInstalled {
        provider: ProviderKind,
    },
    CliVersionUnsupported {
        provider: ProviderKind,
        found: String,
        minimum: String,
    },
    AuthFailed {
        message: String,
    },
    UnsupportedProvider,
    RepoNotInProject,
}

#[axum::debug_handler]
pub async fn create_workspace_from_pr(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<CreateWorkspaceFromPrBody>,
) -> Result<ResponseJson<ApiResponse<CreateWorkspaceFromPrResponse, CreateFromPrError>>, ApiError> {
    let pool = &deployment.db().pool;

    let repo = Repo::find_by_id(pool, payload.repo_id)
        .await?
        .ok_or(RepoError::NotFound)?;

    let project_repos = ProjectRepo::find_by_repo_id(pool, payload.repo_id).await?;
    let project_id = match project_repos.first() {
        Some(project_repo) => project_repo.project_id,
        None => {
            tracing::error!(
                "Repo {} is not associated with any project",
                payload.repo_id
            );
            return Ok(ResponseJson(ApiResponse::error_with_data(
                CreateFromPrError::RepoNotInProject,
            )));
        }
    };

    let remote = match payload.remote_name {
        Some(ref name) => GitRemote {
            url: deployment.git().get_remote_url(&repo.path, name)?,
            name: name.clone(),
        },
        None => deployment.git().get_default_remote(&repo.path)?,
    };

    // Use target branch initially - we'll switch to PR branch via gh pr checkout
    let target_branch_ref = format!("{}/{}", remote.name, payload.base_branch);

    let task_id = Uuid::new_v4();
    let create_task = CreateTask {
        project_id,
        title: payload.pr_title.clone(),
        description: Some(format!(
            "Created from PR #{}: {}",
            payload.pr_number, payload.pr_url
        )),
        status: Some(TaskStatus::InProgress),
        parent_workspace_id: None,
        plan_path: None,
        image_ids: None,
    };
    let task = Task::create(pool, &create_task, task_id).await?;

    let agent_working_dir = Some(repo.name.clone());

    // Create workspace with target branch initially
    let workspace_id = Uuid::new_v4();
    let mut workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: target_branch_ref.clone(),
            agent_working_dir,
        },
        workspace_id,
        task.id,
    )
    .await?;

    WorkspaceRepo::create_many(
        pool,
        workspace.id,
        &[CreateWorkspaceRepo {
            repo_id: payload.repo_id,
            target_branch: target_branch_ref.clone(),
        }],
    )
    .await?;

    let container_ref = deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    // Update workspace with container_ref so start_execution can find it
    workspace.container_ref = Some(container_ref.clone());

    let worktree_path = PathBuf::from(&container_ref).join(&repo.name);
    let checkout_result = match git_host::GitHostService::from_url(&remote.url) {
        Ok(host) if host.provider_kind() == ProviderKind::GitHub => GhCli::new()
            .get_repo_info(&remote.url, &worktree_path)
            .and_then(|repo_info| {
                GhCli::new().pr_checkout(
                    &worktree_path,
                    &repo_info.owner,
                    &repo_info.repo_name,
                    payload.pr_number,
                )
            })
            .map_err(|error| error.to_string()),
        Ok(host) if host.provider_kind() == ProviderKind::Gitee => {
            GeCli::parse_repo_url(&remote.url)
                .and_then(|repo_info| {
                    GeCli::new().checkout_pr(&worktree_path, &repo_info, payload.pr_number)
                })
                .map_err(|error| error.to_string())
        }
        Ok(host) => Err(format!(
            "Checking out pull requests from {} is not supported",
            host.provider_kind()
        )),
        Err(error) => Err(error.to_string()),
    };

    if let Err(message) = checkout_result {
        tracing::error!("Failed to checkout PR branch: {message}");
        return Ok(ResponseJson(ApiResponse::error_with_data(
            CreateFromPrError::BranchFetchFailed { message },
        )));
    }

    Workspace::update_branch_name(pool, workspace.id, &payload.head_branch).await?;
    workspace.branch = payload.head_branch.clone();

    Merge::create_pr(
        pool,
        workspace.id,
        payload.repo_id,
        &format!("{}/{}", remote.name, payload.base_branch),
        payload.pr_number,
        &payload.pr_url,
    )
    .await?;

    if payload.run_setup {
        let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
        if let Some(setup_action) = deployment.container().setup_actions_for_repos(&repos) {
            let session = Session::create(
                pool,
                &CreateSession { executor: None },
                Uuid::new_v4(),
                workspace.id,
            )
            .await?;

            if let Err(e) = deployment
                .container()
                .start_execution(
                    &workspace,
                    &session,
                    &setup_action,
                    &ExecutionProcessRunReason::SetupScript,
                )
                .await
            {
                tracing::error!("Failed to run setup script: {}", e);
            }
        }
    }

    deployment
        .track_if_analytics_allowed(
            "workspace_created_from_pr",
            serde_json::json!({
                "task_id": task.id.to_string(),
                "workspace_id": workspace.id.to_string(),
                "project_id": project_id.to_string(),
                "pr_number": payload.pr_number,
                "run_setup": payload.run_setup,
            }),
        )
        .await;

    tracing::info!(
        "Created workspace {} from PR #{} for task {}",
        workspace.id,
        payload.pr_number,
        task.id
    );

    let workspace = Workspace::find_by_id(pool, workspace.id)
        .await?
        .ok_or(WorkspaceError::TaskNotFound)?;

    Ok(ResponseJson(ApiResponse::success(
        CreateWorkspaceFromPrResponse { workspace, task },
    )))
}
