//! Git hosting provider detection from repository and pull request URLs.

use url::Url;

use super::types::ProviderKind;

fn extract_host(value: &str) -> Option<String> {
    if let Ok(url) = Url::parse(value)
        && let Some(host) = url.host_str()
    {
        return Some(host.to_ascii_lowercase());
    }

    // Git commonly uses SCP-like remotes such as `git@example.com:owner/repo.git`.
    let authority = value.split_once(':')?.0;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() || host.contains('/') || host.contains('\\') {
        return None;
    }

    Some(host.to_ascii_lowercase())
}

/// Detect the git hosting provider from a repository remote or pull request URL.
pub fn detect_provider_from_url(url: &str) -> ProviderKind {
    let Some(host) = extract_host(url) else {
        return ProviderKind::Unknown;
    };
    let path = Url::parse(url)
        .ok()
        .map(|parsed| parsed.path().to_ascii_lowercase())
        .or_else(|| {
            url.split_once(':')
                .map(|(_, path)| path.to_ascii_lowercase())
        })
        .unwrap_or_default();

    if host == "gitee.com" {
        return ProviderKind::Gitee;
    }

    if host == "dev.azure.com"
        || host == "ssh.dev.azure.com"
        || host.ends_with(".visualstudio.com")
        || path.contains("/_git/")
    {
        return ProviderKind::AzureDevOps;
    }

    if host == "github.com" || host.starts_with("github.") {
        return ProviderKind::GitHub;
    }

    ProviderKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_com_https() {
        assert_eq!(
            detect_provider_from_url("https://github.com/owner/repo"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("https://github.com/owner/repo.git"),
            ProviderKind::GitHub
        );
    }

    #[test]
    fn test_github_com_ssh() {
        assert_eq!(
            detect_provider_from_url("git@github.com:owner/repo.git"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("ssh://git@github.com/owner/repo.git"),
            ProviderKind::GitHub
        );
    }

    #[test]
    fn test_gitee_urls() {
        for url in [
            "https://gitee.com/owner/repo.git",
            "git@gitee.com:owner/repo.git",
            "ssh://git@gitee.com/owner/repo.git",
            "https://gitee.com/owner/repo/pulls/123",
        ] {
            assert_eq!(detect_provider_from_url(url), ProviderKind::Gitee);
        }
    }

    #[test]
    fn test_gitee_host_must_match_exactly() {
        for url in [
            "https://gitee.com.evil.example/owner/repo",
            "https://example.com/gitee.com/owner/repo",
            "git@gitee.com.evil.example:owner/repo.git",
        ] {
            assert_eq!(detect_provider_from_url(url), ProviderKind::Unknown);
        }
    }

    #[test]
    fn test_github_enterprise() {
        assert_eq!(
            detect_provider_from_url("https://github.company.com/owner/repo"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("https://github.acme.corp/team/project"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("git@github.internal.io:org/repo.git"),
            ProviderKind::GitHub
        );
    }

    #[test]
    fn test_azure_devops_https() {
        assert_eq!(
            detect_provider_from_url("https://dev.azure.com/org/project/_git/repo"),
            ProviderKind::AzureDevOps
        );
    }

    #[test]
    fn test_azure_devops_ssh() {
        assert_eq!(
            detect_provider_from_url("git@ssh.dev.azure.com:v3/org/project/repo"),
            ProviderKind::AzureDevOps
        );
    }

    #[test]
    fn test_azure_devops_legacy_visualstudio() {
        assert_eq!(
            detect_provider_from_url("https://org.visualstudio.com/project/_git/repo"),
            ProviderKind::AzureDevOps
        );
    }

    #[test]
    fn test_azure_devops_git_path() {
        // Any URL with /_git/ is Azure DevOps
        assert_eq!(
            detect_provider_from_url("https://custom.domain.com/org/project/_git/repo"),
            ProviderKind::AzureDevOps
        );
    }

    #[test]
    fn test_unknown_provider() {
        assert_eq!(
            detect_provider_from_url("https://gitlab.com/owner/repo"),
            ProviderKind::Unknown
        );
        assert_eq!(
            detect_provider_from_url("https://bitbucket.org/owner/repo"),
            ProviderKind::Unknown
        );
    }

    #[test]
    fn test_pr_urls() {
        assert_eq!(
            detect_provider_from_url("https://github.com/owner/repo/pull/123"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("https://github.company.com/owner/repo/pull/456"),
            ProviderKind::GitHub
        );
        assert_eq!(
            detect_provider_from_url("https://dev.azure.com/org/project/_git/repo/pullrequest/123"),
            ProviderKind::AzureDevOps
        );
        assert_eq!(
            detect_provider_from_url(
                "https://org.visualstudio.com/project/_git/repo/pullrequest/456"
            ),
            ProviderKind::AzureDevOps
        );
    }
}
