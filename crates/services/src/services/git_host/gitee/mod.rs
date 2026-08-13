//! Gitee hosting service backed by the community `ge` CLI.

mod cli;

use std::{path::Path, time::Duration};

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
pub use cli::GeCli;
use cli::{GeCliError, GiteeRepoInfo, MINIMUM_GE_VERSION};
use db::models::merge::PullRequestInfo;
use tokio::task;
use tracing::info;

use super::{
    GitHostProvider,
    types::{CreatePrRequest, GitHostError, OpenPrInfo, ProviderKind, UnifiedPrComment},
};

#[derive(Debug, Clone)]
pub struct GiteeProvider {
    ge_cli: GeCli,
}

impl GiteeProvider {
    pub fn new() -> Result<Self, GitHostError> {
        Ok(Self {
            ge_cli: GeCli::new(),
        })
    }

    fn repo_info(remote_url: &str) -> Result<GiteeRepoInfo, GitHostError> {
        GeCli::parse_repo_url(remote_url).map_err(Into::into)
    }

    fn retry_builder() -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_secs(1))
            .with_max_delay(Duration::from_secs(30))
            .with_max_times(3)
            .with_jitter()
    }
}

impl From<GeCliError> for GitHostError {
    fn from(error: GeCliError) -> Self {
        match &error {
            GeCliError::AuthFailed(message) => GitHostError::AuthFailed(message.clone()),
            GeCliError::NotAvailable => GitHostError::CliNotInstalled {
                provider: ProviderKind::Gitee,
            },
            GeCliError::UnsupportedVersion { found, .. } => GitHostError::CliVersionUnsupported {
                provider: ProviderKind::Gitee,
                found: found.clone(),
                minimum: MINIMUM_GE_VERSION.to_string(),
            },
            GeCliError::CommandFailed(message) => {
                let lower = message.to_ascii_lowercase();
                if lower.contains("403")
                    || lower.contains("forbidden")
                    || message.contains("无权限")
                {
                    GitHostError::InsufficientPermissions(message.clone())
                } else if lower.contains("404")
                    || lower.contains("not found")
                    || message.contains("不存在")
                {
                    GitHostError::RepoNotFoundOrNoAccess(message.clone())
                } else {
                    GitHostError::PullRequest(message.clone())
                }
            }
            GeCliError::UnexpectedOutput(message) => {
                GitHostError::UnexpectedOutput(message.clone())
            }
        }
    }
}

#[async_trait]
impl GitHostProvider for GiteeProvider {
    async fn create_pr(
        &self,
        repo_path: &Path,
        remote_url: &str,
        request: &CreatePrRequest,
    ) -> Result<PullRequestInfo, GitHostError> {
        let target_repo = Self::repo_info(remote_url)?;
        if let Some(head_url) = &request.head_repo_url {
            let head_repo = Self::repo_info(head_url)?;
            if head_repo != target_repo {
                return Err(GitHostError::PullRequest(
                    "Cross-fork pull requests are not supported for Gitee".to_string(),
                ));
            }
        }

        (|| async {
            let cli = self.ge_cli.clone();
            let request = request.clone();
            let head_branch = request.head_branch.clone();
            let target_repo = target_repo.clone();
            let repo_path = repo_path.to_path_buf();
            let result =
                task::spawn_blocking(move || cli.create_pr(&request, &target_repo, &repo_path))
                    .await
                    .map_err(|error| {
                        GitHostError::PullRequest(format!(
                            "Failed to execute Gitee CLI for PR creation: {error}"
                        ))
                    })?
                    .map_err(GitHostError::from)?;

            info!(
                "Created Gitee PR #{} for branch {}",
                result.number, head_branch
            );
            Ok(result)
        })
        .retry(Self::retry_builder())
        .when(|error: &GitHostError| error.should_retry())
        .notify(|error: &GitHostError, delay: Duration| {
            tracing::warn!(
                "Gitee CLI call failed, retrying after {:.2}s: {}",
                delay.as_secs_f64(),
                error
            );
        })
        .await
    }

    async fn get_pr_status(&self, pr_url: &str) -> Result<PullRequestInfo, GitHostError> {
        (|| async {
            let cli = self.ge_cli.clone();
            let url = pr_url.to_string();
            task::spawn_blocking(move || cli.view_pr(&url))
                .await
                .map_err(|error| {
                    GitHostError::PullRequest(format!(
                        "Failed to execute Gitee CLI for viewing PR: {error}"
                    ))
                })?
                .map_err(GitHostError::from)
        })
        .retry(Self::retry_builder())
        .when(|error: &GitHostError| error.should_retry())
        .await
    }

    async fn list_prs_for_branch(
        &self,
        _repo_path: &Path,
        remote_url: &str,
        branch_name: &str,
    ) -> Result<Vec<PullRequestInfo>, GitHostError> {
        let repo = Self::repo_info(remote_url)?;
        (|| async {
            let cli = self.ge_cli.clone();
            let repo = repo.clone();
            let branch = branch_name.to_string();
            task::spawn_blocking(move || cli.list_prs_for_branch(&repo, &branch))
                .await
                .map_err(|error| {
                    GitHostError::PullRequest(format!(
                        "Failed to execute Gitee CLI for listing PRs: {error}"
                    ))
                })?
                .map_err(GitHostError::from)
        })
        .retry(Self::retry_builder())
        .when(|error: &GitHostError| error.should_retry())
        .await
    }

    async fn get_pr_comments(
        &self,
        _repo_path: &Path,
        remote_url: &str,
        pr_number: i64,
    ) -> Result<Vec<UnifiedPrComment>, GitHostError> {
        let repo = Self::repo_info(remote_url)?;
        (|| async {
            let cli = self.ge_cli.clone();
            let repo = repo.clone();
            task::spawn_blocking(move || cli.get_pr_comments(&repo, pr_number))
                .await
                .map_err(|error| {
                    GitHostError::PullRequest(format!(
                        "Failed to execute Gitee CLI for fetching comments: {error}"
                    ))
                })?
                .map_err(GitHostError::from)
        })
        .retry(Self::retry_builder())
        .when(|error: &GitHostError| error.should_retry())
        .await
    }

    async fn list_open_prs(
        &self,
        _repo_path: &Path,
        remote_url: &str,
    ) -> Result<Vec<OpenPrInfo>, GitHostError> {
        let repo = Self::repo_info(remote_url)?;
        (|| async {
            let cli = self.ge_cli.clone();
            let repo = repo.clone();
            task::spawn_blocking(move || cli.list_open_prs(&repo))
                .await
                .map_err(|error| {
                    GitHostError::PullRequest(format!(
                        "Failed to execute Gitee CLI for listing open PRs: {error}"
                    ))
                })?
                .map_err(GitHostError::from)
        })
        .retry(Self::retry_builder())
        .when(|error: &GitHostError| error.should_retry())
        .await
    }

    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Gitee
    }
}
