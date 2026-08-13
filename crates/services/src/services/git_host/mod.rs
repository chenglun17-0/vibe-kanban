mod detection;
mod types;

pub mod azure;
pub mod gitee;
pub mod github;

use std::path::Path;

use async_trait::async_trait;
use db::models::merge::PullRequestInfo;
use detection::detect_provider_from_url;
use enum_dispatch::enum_dispatch;
pub use types::{
    CreatePrRequest, GitHostError, OpenPrInfo, PrComment, PrCommentAuthor, PrReviewComment,
    ProviderKind, ReviewCommentUser, UnifiedPrComment,
};

use self::{azure::AzureDevOpsProvider, gitee::GiteeProvider, github::GitHubProvider};

#[async_trait]
#[enum_dispatch(GitHostService)]
pub trait GitHostProvider: Send + Sync {
    async fn create_pr(
        &self,
        repo_path: &Path,
        remote_url: &str,
        request: &CreatePrRequest,
    ) -> Result<PullRequestInfo, GitHostError>;

    async fn get_pr_status(&self, pr_url: &str) -> Result<PullRequestInfo, GitHostError>;

    async fn list_prs_for_branch(
        &self,
        repo_path: &Path,
        remote_url: &str,
        branch_name: &str,
    ) -> Result<Vec<PullRequestInfo>, GitHostError>;

    async fn get_pr_comments(
        &self,
        repo_path: &Path,
        remote_url: &str,
        pr_number: i64,
    ) -> Result<Vec<UnifiedPrComment>, GitHostError>;

    async fn list_open_prs(
        &self,
        repo_path: &Path,
        remote_url: &str,
    ) -> Result<Vec<OpenPrInfo>, GitHostError>;

    fn provider_kind(&self) -> ProviderKind;
}

#[enum_dispatch]
pub enum GitHostService {
    GitHub(GitHubProvider),
    Gitee(GiteeProvider),
    AzureDevOps(AzureDevOpsProvider),
}

impl GitHostService {
    pub fn from_url(url: &str) -> Result<Self, GitHostError> {
        match detect_provider_from_url(url) {
            ProviderKind::GitHub => Ok(Self::GitHub(GitHubProvider::new()?)),
            ProviderKind::Gitee => Ok(Self::Gitee(GiteeProvider::new()?)),
            ProviderKind::AzureDevOps => Ok(Self::AzureDevOps(AzureDevOpsProvider::new()?)),
            ProviderKind::Unknown => Err(GitHostError::UnsupportedProvider),
        }
    }
}
