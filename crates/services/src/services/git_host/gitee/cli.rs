//! Low-level helpers around the community Gitee CLI (`ge`).

use std::{
    ffi::{OsStr, OsString},
    io::Write,
    path::Path,
    process::Command,
};

use chrono::{DateTime, Utc};
use db::models::merge::{MergeStatus, PullRequestInfo};
use serde::Deserialize;
use tempfile::NamedTempFile;
use thiserror::Error;
use url::Url;
use utils::shell::resolve_executable_path_blocking;

use crate::services::git_host::types::{CreatePrRequest, OpenPrInfo, UnifiedPrComment};

pub const MINIMUM_GE_VERSION: &str = "5.21.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiteeRepoInfo {
    pub owner: String,
    pub repo_name: String,
}

#[derive(Debug, Deserialize)]
struct GeBranchRef {
    #[serde(default)]
    r#ref: String,
}

#[derive(Debug, Deserialize)]
struct GePrResponse {
    number: i64,
    html_url: String,
    #[serde(default)]
    state: String,
    merged_at: Option<String>,
    head: Option<GeBranchRef>,
    base: Option<GeBranchRef>,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Error)]
pub enum GeCliError {
    #[error("Gitee CLI (`ge`) executable not found or not runnable")]
    NotAvailable,
    #[error("Gitee CLI version {found} is unsupported; version {minimum} or newer is required")]
    UnsupportedVersion { found: String, minimum: String },
    #[error("Gitee CLI command failed: {0}")]
    CommandFailed(String),
    #[error("Gitee CLI authentication failed: {0}")]
    AuthFailed(String),
    #[error("Gitee CLI returned unexpected output: {0}")]
    UnexpectedOutput(String),
}

#[derive(Debug, Clone, Default)]
pub struct GeCli;

impl GeCli {
    pub fn new() -> Self {
        Self {}
    }

    fn executable(&self) -> Result<std::path::PathBuf, GeCliError> {
        resolve_executable_path_blocking("ge").ok_or(GeCliError::NotAvailable)
    }

    fn ensure_supported_version(&self, ge: &Path) -> Result<(), GeCliError> {
        let output = Command::new(ge)
            .arg("version")
            .output()
            .map_err(|err| GeCliError::CommandFailed(err.to_string()))?;
        if !output.status.success() {
            return Err(GeCliError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        let found = Self::parse_version(&raw).ok_or_else(|| {
            GeCliError::UnexpectedOutput(format!(
                "Could not parse `ge version` output: {}",
                raw.trim()
            ))
        })?;
        if !Self::version_at_least(&found, MINIMUM_GE_VERSION) {
            return Err(GeCliError::UnsupportedVersion {
                found,
                minimum: MINIMUM_GE_VERSION.to_string(),
            });
        }
        Ok(())
    }

    fn run<I, S>(&self, args: I, dir: Option<&Path>) -> Result<String, GeCliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let ge = self.executable()?;
        self.ensure_supported_version(&ge)?;

        let mut command = Command::new(&ge);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        command.args(args);
        tracing::debug!(executable = ?ge, "Running Gitee CLI command");

        let output = command
            .output()
            .map_err(|err| GeCliError::CommandFailed(err.to_string()))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("ge auth login")
            || lower.contains("not logged in")
            || lower.contains("unauthorized")
            || lower.contains("authentication")
            || stderr.contains("请先登录")
            || stderr.contains("认证无效")
        {
            return Err(GeCliError::AuthFailed(stderr));
        }
        Err(GeCliError::CommandFailed(stderr))
    }

    pub fn parse_repo_url(remote_url: &str) -> Result<GiteeRepoInfo, GeCliError> {
        let (host, path) = if let Ok(url) = Url::parse(remote_url) {
            (
                url.host_str().unwrap_or_default().to_ascii_lowercase(),
                url.path().trim_start_matches('/').to_string(),
            )
        } else {
            let (authority, path) = remote_url.split_once(':').ok_or_else(|| {
                GeCliError::UnexpectedOutput(format!("Invalid Gitee remote URL: {remote_url}"))
            })?;
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host)
                .to_ascii_lowercase();
            (host, path.to_string())
        };

        if host != "gitee.com" {
            return Err(GeCliError::UnexpectedOutput(format!(
                "Expected a gitee.com URL, got: {remote_url}"
            )));
        }

        let mut segments = path.trim_end_matches('/').split('/');
        let owner = segments.next().filter(|value| !value.is_empty());
        let repo = segments.next().filter(|value| !value.is_empty());
        match (owner, repo) {
            (Some(owner), Some(repo)) => Ok(GiteeRepoInfo {
                owner: owner.to_string(),
                repo_name: repo.trim_end_matches(".git").to_string(),
            }),
            _ => Err(GeCliError::UnexpectedOutput(format!(
                "Could not extract owner/repository from Gitee URL: {remote_url}"
            ))),
        }
    }

    pub fn parse_pr_url(pr_url: &str) -> Result<(GiteeRepoInfo, i64), GeCliError> {
        let url = Url::parse(pr_url)
            .map_err(|_| GeCliError::UnexpectedOutput(format!("Invalid Gitee PR URL: {pr_url}")))?;
        if url.host_str() != Some("gitee.com") {
            return Err(GeCliError::UnexpectedOutput(format!(
                "Expected a gitee.com PR URL, got: {pr_url}"
            )));
        }
        let segments: Vec<_> = url
            .path_segments()
            .map(|segments| segments.collect())
            .unwrap_or_default();
        if segments.len() != 4 || segments[2] != "pulls" {
            return Err(GeCliError::UnexpectedOutput(format!(
                "Unexpected Gitee PR URL format: {pr_url}"
            )));
        }
        let number = segments[3].parse::<i64>().map_err(|err| {
            GeCliError::UnexpectedOutput(format!(
                "Invalid pull request number in '{pr_url}': {err}"
            ))
        })?;
        Ok((
            GiteeRepoInfo {
                owner: segments[0].to_string(),
                repo_name: segments[1].to_string(),
            },
            number,
        ))
    }

    pub fn create_pr(
        &self,
        request: &CreatePrRequest,
        repo: &GiteeRepoInfo,
        repo_path: &Path,
    ) -> Result<PullRequestInfo, GeCliError> {
        let mut body_file =
            NamedTempFile::new().map_err(|err| GeCliError::CommandFailed(err.to_string()))?;
        body_file
            .write_all(request.body.as_deref().unwrap_or("").as_bytes())
            .map_err(|err| GeCliError::CommandFailed(err.to_string()))?;

        let mut args: Vec<OsString> = vec![
            "pr".into(),
            "create".into(),
            "--repo".into(),
            format!("{}/{}", repo.owner, repo.repo_name).into(),
            "--head".into(),
            request.head_branch.clone().into(),
            "--base".into(),
            request.base_branch.clone().into(),
            "--title".into(),
            request.title.clone().into(),
            "--body-file".into(),
            body_file.path().as_os_str().to_os_string(),
        ];
        if request.draft.unwrap_or(false) {
            args.push("--draft".into());
        }

        let raw = self.run(args, Some(repo_path))?;
        Self::parse_create_output(&raw)
    }

    pub fn view_pr(&self, pr_url: &str) -> Result<PullRequestInfo, GeCliError> {
        let (repo, number) = Self::parse_pr_url(pr_url)?;
        let raw = self.run(
            [
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                &format!("{}/{}", repo.owner, repo.repo_name),
                "--json",
                "number,html_url,state,merged_at",
            ],
            None,
        )?;
        Self::parse_pr_view(&raw)
    }

    pub fn list_prs_for_branch(
        &self,
        repo: &GiteeRepoInfo,
        branch: &str,
    ) -> Result<Vec<PullRequestInfo>, GeCliError> {
        let raw = self.run(
            [
                "pr",
                "list",
                "--repo",
                &format!("{}/{}", repo.owner, repo.repo_name),
                "--state",
                "all",
                "--head",
                branch,
                "--json",
                "number,html_url,state,merged_at",
            ],
            None,
        )?;
        Self::parse_pr_list(&raw)
    }

    pub fn list_open_prs(&self, repo: &GiteeRepoInfo) -> Result<Vec<OpenPrInfo>, GeCliError> {
        let raw = self.run(
            [
                "pr",
                "list",
                "--repo",
                &format!("{}/{}", repo.owner, repo.repo_name),
                "--state",
                "open",
                "--json",
                "number,html_url,title,head,base",
            ],
            None,
        )?;
        Self::parse_open_pr_list(&raw)
    }

    pub fn checkout_pr(
        &self,
        repo_path: &Path,
        repo: &GiteeRepoInfo,
        pr_number: i64,
    ) -> Result<(), GeCliError> {
        self.run(
            [
                "pr",
                "checkout",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", repo.owner, repo.repo_name),
                "--force",
            ],
            Some(repo_path),
        )?;
        Ok(())
    }

    pub fn get_pr_comments(
        &self,
        repo: &GiteeRepoInfo,
        pr_number: i64,
    ) -> Result<Vec<UnifiedPrComment>, GeCliError> {
        let raw = self.run(
            [
                "pr",
                "comment",
                "list",
                &pr_number.to_string(),
                "--repo",
                &format!("{}/{}", repo.owner, repo.repo_name),
                "--limit",
                "100",
            ],
            None,
        )?;
        Self::parse_comments(&raw)
    }

    fn parse_version(raw: &str) -> Option<String> {
        raw.split_whitespace()
            .find(|part| {
                let candidate = part.trim_start_matches('v');
                candidate.split('.').count() == 3
                    && candidate
                        .split('.')
                        .all(|segment| segment.parse::<u64>().is_ok())
            })
            .map(|part| part.trim_start_matches('v').to_string())
    }

    fn version_at_least(found: &str, minimum: &str) -> bool {
        let parse = |value: &str| -> Option<[u64; 3]> {
            let mut parts = value.split('.').map(|part| part.parse::<u64>().ok());
            Some([parts.next()??, parts.next()??, parts.next()??])
        };
        match (parse(found), parse(minimum)) {
            (Some(found), Some(minimum)) => found >= minimum,
            _ => false,
        }
    }

    fn parse_create_output(raw: &str) -> Result<PullRequestInfo, GeCliError> {
        let url = raw
            .split_whitespace()
            .find(|part| part.starts_with("https://gitee.com/") && part.contains("/pulls/"))
            .map(|part| part.trim_end_matches(['.', ',', ';']).to_string())
            .ok_or_else(|| {
                GeCliError::UnexpectedOutput(format!(
                    "`ge pr create` did not return a pull request URL; raw: {raw}"
                ))
            })?;
        let (_, number) = Self::parse_pr_url(&url)?;
        Ok(PullRequestInfo {
            number,
            url,
            status: MergeStatus::Open,
            merged_at: None,
            merge_commit_sha: None,
        })
    }

    fn parse_pr_view(raw: &str) -> Result<PullRequestInfo, GeCliError> {
        let pr: GePrResponse = serde_json::from_str(raw.trim()).map_err(|err| {
            GeCliError::UnexpectedOutput(format!("Failed to parse PR JSON: {err}; raw: {raw}"))
        })?;
        Ok(Self::to_pr_info(pr))
    }

    fn parse_pr_list(raw: &str) -> Result<Vec<PullRequestInfo>, GeCliError> {
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let prs: Vec<GePrResponse> = serde_json::from_str(raw.trim()).map_err(|err| {
            GeCliError::UnexpectedOutput(format!("Failed to parse PR list JSON: {err}; raw: {raw}"))
        })?;
        Ok(prs.into_iter().map(Self::to_pr_info).collect())
    }

    fn parse_open_pr_list(raw: &str) -> Result<Vec<OpenPrInfo>, GeCliError> {
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let prs: Vec<GePrResponse> = serde_json::from_str(raw.trim()).map_err(|err| {
            GeCliError::UnexpectedOutput(format!("Failed to parse open PR JSON: {err}; raw: {raw}"))
        })?;
        Ok(prs
            .into_iter()
            .map(|pr| OpenPrInfo {
                number: pr.number,
                url: pr.html_url,
                title: pr.title,
                head_branch: pr.head.map(|branch| branch.r#ref).unwrap_or_default(),
                base_branch: pr.base.map(|branch| branch.r#ref).unwrap_or_default(),
            })
            .collect())
    }

    fn parse_comments(raw: &str) -> Result<Vec<UnifiedPrComment>, GeCliError> {
        if raw.trim().is_empty() || raw.contains("No comments found") {
            return Ok(Vec::new());
        }

        let mut comments = Vec::new();
        for block in raw.split("  Comment #").skip(1) {
            let mut lines = block.lines();
            let id = lines
                .next()
                .and_then(|line| line.trim().parse::<i64>().ok())
                .ok_or_else(|| {
                    GeCliError::UnexpectedOutput(format!("Invalid comment block: {block}"))
                })?;
            let author = lines
                .next()
                .and_then(|line| line.trim().strip_prefix("Author: "))
                .ok_or_else(|| {
                    GeCliError::UnexpectedOutput(format!("Missing comment author: {block}"))
                })?
                .to_string();
            let created_at = lines
                .next()
                .and_then(|line| line.trim().strip_prefix("Created: "))
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
                .ok_or_else(|| {
                    GeCliError::UnexpectedOutput(format!("Invalid comment timestamp: {block}"))
                })?;
            let body_first = lines
                .next()
                .and_then(|line| line.trim_start().strip_prefix("Body: "))
                .ok_or_else(|| {
                    GeCliError::UnexpectedOutput(format!("Missing comment body: {block}"))
                })?;
            let body = std::iter::once(body_first)
                .chain(lines)
                .collect::<Vec<_>>()
                .join("\n")
                .trim_end()
                .to_string();

            comments.push(UnifiedPrComment::General {
                id: id.to_string(),
                author,
                author_association: None,
                body,
                created_at,
                url: None,
            });
        }
        if comments.is_empty() {
            return Err(GeCliError::UnexpectedOutput(format!(
                "Failed to parse `ge pr comment list` output: {raw}"
            )));
        }
        comments.sort_by_key(|comment| comment.created_at());
        Ok(comments)
    }

    fn to_pr_info(pr: GePrResponse) -> PullRequestInfo {
        // Gitee serializes an unset merge timestamp as year 1 instead of null.
        let merged_at = pr
            .merged_at
            .filter(|value| !value.starts_with("0001-01-01"))
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc));
        let status = if merged_at.is_some() {
            MergeStatus::Merged
        } else {
            match pr.state.to_ascii_lowercase().as_str() {
                "open" => MergeStatus::Open,
                "merged" => MergeStatus::Merged,
                "closed" => MergeStatus::Closed,
                _ => MergeStatus::Unknown,
            }
        };
        PullRequestInfo {
            number: pr.number,
            url: pr.html_url,
            status,
            merged_at,
            merge_commit_sha: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_gitee_urls() {
        for url in [
            "https://gitee.com/acme/widgets.git",
            "git@gitee.com:acme/widgets.git",
            "ssh://git@gitee.com/acme/widgets.git",
        ] {
            assert_eq!(
                GeCli::parse_repo_url(url).unwrap(),
                GiteeRepoInfo {
                    owner: "acme".into(),
                    repo_name: "widgets".into(),
                }
            );
        }
        let (repo, number) =
            GeCli::parse_pr_url("https://gitee.com/acme/widgets/pulls/42").unwrap();
        assert_eq!(repo.owner, "acme");
        assert_eq!(number, 42);
    }

    #[test]
    fn rejects_non_gitee_and_malformed_urls() {
        assert!(GeCli::parse_repo_url("https://gitee.com.evil.test/acme/widgets").is_err());
        assert!(GeCli::parse_pr_url("https://gitee.com/acme/widgets/issues/42").is_err());
    }

    #[test]
    fn parses_version_and_enforces_minimum() {
        assert_eq!(
            GeCli::parse_version("ge version 5.21.0 (2026-07-09)"),
            Some("5.21.0".into())
        );
        assert!(GeCli::version_at_least("5.21.0", MINIMUM_GE_VERSION));
        assert!(GeCli::version_at_least("5.22.1", MINIMUM_GE_VERSION));
        assert!(!GeCli::version_at_least("5.20.9", MINIMUM_GE_VERSION));
    }

    #[test]
    fn parses_create_and_pr_json() {
        let created = GeCli::parse_create_output(
            "Pull request created successfully: https://gitee.com/acme/widgets/pulls/17\n",
        )
        .unwrap();
        assert_eq!(created.number, 17);
        assert!(matches!(created.status, MergeStatus::Open));

        let viewed = GeCli::parse_pr_view(
            r#"{"number":17,"html_url":"https://gitee.com/acme/widgets/pulls/17","state":"merged","merged_at":"2026-08-13T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(matches!(viewed.status, MergeStatus::Merged));
        assert!(viewed.merged_at.is_some());

        let open = GeCli::parse_pr_view(
            r#"{"number":18,"html_url":"https://gitee.com/acme/widgets/pulls/18","state":"open","merged_at":"0001-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        assert!(matches!(open.status, MergeStatus::Open));
        assert!(open.merged_at.is_none());
    }

    #[test]
    fn parses_open_prs_and_empty_lists() {
        let raw = r#"[{"number":7,"html_url":"https://gitee.com/acme/widgets/pulls/7","title":"Fix","head":{"ref":"feature"},"base":{"ref":"main"}}]"#;
        let prs = GeCli::parse_open_pr_list(raw).unwrap();
        assert_eq!(prs[0].head_branch, "feature");
        assert_eq!(prs[0].base_branch, "main");
        assert!(GeCli::parse_pr_list("").unwrap().is_empty());
    }

    #[test]
    fn parses_multiline_comments() {
        let raw = "Comments for acme/widgets#7:\n\n  Comment #12\n    Author: alice\n    Created: 2026-08-13T10:00:00Z\n    Body: first line\nsecond line\n\n  Comment #13\n    Author: bob\n    Created: 2026-08-13T11:00:00Z\n    Body: looks good\n";
        let comments = GeCli::parse_comments(raw).unwrap();
        assert_eq!(comments.len(), 2);
        match &comments[0] {
            UnifiedPrComment::General { body, author, .. } => {
                assert_eq!(author, "alice");
                assert_eq!(body, "first line\nsecond line");
            }
            _ => panic!("expected a general comment"),
        }
    }
}
