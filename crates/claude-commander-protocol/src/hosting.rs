//! Provider-neutral code-host wire contracts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Hosted Git provider selected for Commander-wide repository and review operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum CodeHostProvider {
    #[default]
    Github,
    Gitlab,
}

impl CodeHostProvider {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Gitlab => "GitLab",
        }
    }

    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
        }
    }
}

/// Identity of the provider used for one listing or status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CodeHost {
    pub provider: CodeHostProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Visibility values shared by GitHub and GitLab repository listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum RepositoryVisibility {
    Public,
    Internal,
    Private,
}

/// A repository normalized across supported code-host APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct HostedRepository {
    pub full_name: String,
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub visibility: RepositoryVisibility,
    pub fork: bool,
    pub archived: bool,
    #[serde(default)]
    pub default_branch: Option<String>,
    pub clone_url: String,
    pub ssh_url: String,
    #[serde(default)]
    pub activity_at: Option<DateTime<Utc>>,
}

/// Provider-bearing repository list; the host remains observable for an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RepositoryListing {
    pub host: CodeHost,
    pub repositories: Vec<HostedRepository>,
}

impl From<crate::github::GithubRepo> for HostedRepository {
    fn from(repo: crate::github::GithubRepo) -> Self {
        Self {
            full_name: repo.full_name,
            namespace: repo.owner,
            name: repo.name,
            description: repo.description,
            visibility: if repo.private {
                RepositoryVisibility::Private
            } else {
                RepositoryVisibility::Public
            },
            fork: repo.fork,
            archived: repo.archived,
            default_branch: Some(repo.default_branch),
            clone_url: repo.clone_url,
            ssh_url: repo.ssh_url,
            activity_at: repo.pushed_at,
        }
    }
}

/// Where a hosted or ordinary URL clone should come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CloneSource {
    Github {
        full_name: String,
    },
    Gitlab {
        full_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
    },
    Url {
        url: String,
    },
}

/// Why a configured self-managed GitLab hostname was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidGitlabHostname;

impl fmt::Display for InvalidGitlabHostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "expected a hostname with an optional numeric port, without a scheme, path, userinfo, credentials, whitespace, or options"
        )
    }
}

impl std::error::Error for InvalidGitlabHostname {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHostedRepoSlug {
    provider: CodeHostProvider,
    slug: String,
}

impl fmt::Display for InvalidHostedRepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shape = match self.provider {
            CodeHostProvider::Github => "owner/name",
            CodeHostProvider::Gitlab => "namespace/path",
        };
        write!(f, "'{}' is not a {shape} repository slug", self.slug)
    }
}

impl std::error::Error for InvalidHostedRepoSlug {}

/// Validate the provider-specific repository path passed as one CLI argv element.
pub fn validate_hosted_repo_slug(
    provider: CodeHostProvider,
    slug: &str,
) -> Result<(), InvalidHostedRepoSlug> {
    let invalid = || InvalidHostedRepoSlug {
        provider,
        slug: crate::github::redact_credentials(slug)
            .chars()
            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
            .collect(),
    };
    let segments = slug.split('/').collect::<Vec<_>>();
    let count_is_valid = match provider {
        CodeHostProvider::Github => segments.len() == 2,
        CodeHostProvider::Gitlab => segments.len() >= 2,
    };
    if !count_is_valid {
        return Err(invalid());
    }
    for segment in segments {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.starts_with('-')
            || segment.chars().any(char::is_control)
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Validate a `glab --hostname`/`GITLAB_HOST` value without accepting URL syntax.
///
/// The returned value is the caller's exact string so a validated hostname can
/// be frozen into a clone request without silently changing its meaning.
pub fn validate_gitlab_hostname(hostname: &str) -> Result<&str, InvalidGitlabHostname> {
    if hostname.is_empty()
        || hostname.starts_with('-')
        || hostname
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || hostname.contains(['/', '\\', '@'])
        || hostname.contains("://")
    {
        return Err(InvalidGitlabHostname);
    }

    let (host, port) = match hostname.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err(InvalidGitlabHostname);
            }
            (host, Some(port))
        }
        None => (hostname, None),
    };

    if host.is_empty()
        || host.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return Err(InvalidGitlabHostname);
    }

    if let Some(port) = port {
        let Ok(port) = port.parse::<u16>() else {
            return Err(InvalidGitlabHostname);
        };
        if port == 0 {
            return Err(InvalidGitlabHostname);
        }
    }

    Ok(hostname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_to_github_and_uses_lowercase_wire_values() {
        assert_eq!(CodeHostProvider::default(), CodeHostProvider::Github);
        assert_eq!(
            serde_json::to_string(&CodeHostProvider::Github).unwrap(),
            r#""github""#
        );
        assert_eq!(
            serde_json::to_string(&CodeHostProvider::Gitlab).unwrap(),
            r#""gitlab""#
        );
        assert_eq!(
            serde_json::from_str::<CodeHostProvider>(r#""gitlab""#).unwrap(),
            CodeHostProvider::Gitlab
        );
    }

    #[test]
    fn empty_listing_still_identifies_the_selected_host() {
        let listing = RepositoryListing {
            host: CodeHost {
                provider: CodeHostProvider::Gitlab,
                hostname: Some("gitlab.example.com".into()),
            },
            repositories: Vec::new(),
        };
        let wire = serde_json::to_value(&listing).unwrap();
        assert_eq!(wire["host"]["provider"], "gitlab");
        assert_eq!(wire["host"]["hostname"], "gitlab.example.com");
        assert_eq!(wire["repositories"], serde_json::json!([]));
    }

    #[test]
    fn repository_visibility_preserves_gitlabs_internal_value() {
        assert_eq!(
            serde_json::from_str::<RepositoryVisibility>(r#""internal""#).unwrap(),
            RepositoryVisibility::Internal
        );
    }

    #[test]
    fn gitlab_clone_source_round_trips_with_a_frozen_hostname() {
        let source = CloneSource::Gitlab {
            full_name: "group/subgroup/project".into(),
            hostname: Some("gitlab.example.com".into()),
        };
        let wire = serde_json::to_string(&source).unwrap();
        assert_eq!(
            wire,
            r#"{"kind":"gitlab","full_name":"group/subgroup/project","hostname":"gitlab.example.com"}"#
        );
        assert_eq!(serde_json::from_str::<CloneSource>(&wire).unwrap(), source);
    }

    #[test]
    fn github_clone_source_keeps_its_existing_wire_shape() {
        let source = CloneSource::Github {
            full_name: "owner/repo".into(),
        };
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            r#"{"kind":"github","full_name":"owner/repo"}"#
        );
    }

    #[test]
    fn provider_display_and_cli_names_are_fixed_values() {
        assert_eq!(CodeHostProvider::Github.display_name(), "GitHub");
        assert_eq!(CodeHostProvider::Github.cli_name(), "gh");
        assert_eq!(CodeHostProvider::Gitlab.display_name(), "GitLab");
        assert_eq!(CodeHostProvider::Gitlab.cli_name(), "glab");
    }

    #[test]
    fn accepts_gitlab_hostnames_and_optional_ports() {
        for hostname in [
            "gitlab.com",
            "gitlab.example.com",
            "localhost",
            "gitlab.example.com:8443",
        ] {
            assert_eq!(validate_gitlab_hostname(hostname).unwrap(), hostname);
        }
    }

    #[test]
    fn rejects_gitlab_hostname_values_that_are_not_hosts() {
        for hostname in [
            "",
            "--hostname",
            "https://gitlab.example.com",
            "gitlab.example.com/group",
            "user@gitlab.example.com",
            "user:token@gitlab.example.com",
            "gitlab.example.com:abc",
            "gitlab.example.com:70000",
            "gitlab.example.com\n--paginate",
        ] {
            assert!(
                validate_gitlab_hostname(hostname).is_err(),
                "accepted invalid hostname {hostname:?}"
            );
        }
    }

    #[test]
    fn provider_specific_slug_depth_is_enforced() {
        assert!(validate_hosted_repo_slug(CodeHostProvider::Github, "owner/repo").is_ok());
        assert!(validate_hosted_repo_slug(CodeHostProvider::Github, "group/sub/repo").is_err());
        assert!(validate_hosted_repo_slug(CodeHostProvider::Gitlab, "group/repo").is_ok());
        assert!(validate_hosted_repo_slug(CodeHostProvider::Gitlab, "group/sub/repo").is_ok());
        assert!(validate_hosted_repo_slug(CodeHostProvider::Gitlab, "repo").is_err());
    }

    #[test]
    fn slug_rejections_redact_credentials_at_construction() {
        let error = validate_hosted_repo_slug(
            CodeHostProvider::Gitlab,
            "https://user:secret@gitlab.example.com/group/repo",
        )
        .unwrap_err();
        assert!(!format!("{error:?}").contains("secret"));
        assert!(!error.to_string().contains("secret"));

        let control =
            validate_hosted_repo_slug(CodeHostProvider::Gitlab, "group/repo\nforged").unwrap_err();
        assert!(!control.to_string().contains('\n'));
    }
}
