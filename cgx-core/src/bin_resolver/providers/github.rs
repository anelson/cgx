use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use serde::Deserialize;
use snafu::ResultExt;
use tempfile::TempDir;

use super::Provider;
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    cratespec::Forge,
    downloader::DownloadedCrate,
    error,
    http::{
        ACCEPT, API_RESPONSE_LIMIT_BYTES, AUTHORIZATION, HeaderMap, HeaderValue, HttpClient,
        SMALL_DOWNLOAD_LIMIT_BYTES,
    },
    messages::PrebuiltBinaryMessage,
    target::TargetTriple,
};

pub(in crate::bin_resolver) struct GithubProvider {
    reporter: crate::messages::MessageReporter,
    staging_dir: PathBuf,
    verify_checksums: bool,
    http_client: HttpClient,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

impl GithubProvider {
    pub(in crate::bin_resolver) fn new(
        reporter: crate::messages::MessageReporter,
        staging_dir: &TempDir,
        verify_checksums: bool,
        http_client: HttpClient,
    ) -> Self {
        Self {
            reporter,
            staging_dir: staging_dir
                .path()
                .join(<&'static str>::from(BinaryProvider::GithubReleases)),
            verify_checksums,
            http_client,
        }
    }

    /// Get the GitHub repository URL for a crate.
    ///
    /// If the crate came from a GitHub forge, the forge URL is used directly (handles the fork
    /// scenario where Cargo.toml may still point to the upstream). For all other sources
    /// (including non-GitHub forges), falls back to the `[package].repository` field in
    /// Cargo.toml only when it is a `https://github.com/...` URL.
    fn get_repo_url(krate: &DownloadedCrate) -> Result<Option<String>> {
        match &krate.resolved.source {
            ResolvedSource::Forge {
                forge: forge @ Forge::GitHub { .. },
                ..
            } => Ok(Some(forge.repo_url())),
            ResolvedSource::Forge { .. }
            | ResolvedSource::CratesIo
            | ResolvedSource::Registry { .. }
            | ResolvedSource::Git { .. }
            | ResolvedSource::LocalDir { .. } => Ok(krate
                .repository_url()?
                .filter(|u| u.starts_with("https://github.com/"))),
        }
    }

    /// Parse owner and repo from a GitHub repository URL.
    ///
    /// Given `https://github.com/owner/repo` (or a custom GHE base), returns `("owner", "repo")`.
    fn parse_owner_repo(repo_url: &str) -> Option<(&str, &str)> {
        let path = repo_url.strip_prefix("https://")?.split_once('/')?.1;
        let (owner, rest) = path.split_once('/')?;
        let repo = rest.split('/').next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some((owner, repo))
    }

    /// Determine the API base URL for a given repository URL.
    ///
    /// For `github.com`, returns `https://api.github.com`.
    /// For GitHub Enterprise (`github.example.com`), returns `https://github.example.com/api/v3`.
    fn api_base(repo_url: &str) -> Option<String> {
        let host = repo_url.strip_prefix("https://")?.split('/').next()?;
        if host == "github.com" {
            Some("https://api.github.com".to_string())
        } else {
            Some(format!("https://{}/api/v3", host))
        }
    }

    /// List release assets for a given tag from the GitHub Releases API.
    ///
    /// Returns a vec of `(asset_name, download_url)` pairs. A 404 (no release for the tag) is a
    /// conclusive absence and maps to `Ok(empty)`. Any other failure  is returned as `Err`.
    fn list_release_assets(
        &self,
        api_base: &str,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Vec<(String, String)>> {
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            api_base,
            owner,
            repo,
            super::tag_url_path_segment(tag)
        );

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            if let Ok(auth_value) = HeaderValue::from_str(&format!("token {}", token)) {
                headers.insert(AUTHORIZATION, auth_value);
            }
        }

        let response =
            self.http_client
                .get_with_headers_retrying_status(&url, &headers, Self::should_retry_status)?;

        if !response.status().is_success() {
            if response.status() == StatusCode::NOT_FOUND {
                // The API call worked, and Github is telling us that there are not release assets
                // for this resource.  From the caller's perspective that isn't an error at all, it
                // is a conclusive, negative result.
                return Ok(Vec::new());
            }

            return Self::non_success_response_error(&url, response);
        }

        let text = HttpClient::response_body_to_string(response, &url, API_RESPONSE_LIMIT_BYTES)?;

        let release: ReleaseResponse = serde_json::from_str(&text).context(error::JsonSnafu)?;

        Ok(release
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect())
    }

    /// Try each release tag form in order, returning the assets of the first tag whose release
    /// has any.
    ///
    /// A tag whose release is absent (404) or exists but has no assets falls through to the next
    /// form; an inconclusive [`Self::list_release_assets`] failure propagates as `Err`. Returns
    /// an empty vec when no tag form yields assets, which is a conclusive absence.
    fn find_release_assets_for_tags(
        &self,
        api_base: &str,
        owner: &str,
        repo: &str,
        tags: &[String],
    ) -> Result<Vec<(String, String)>> {
        for tag in tags {
            let assets = self.list_release_assets(api_base, owner, repo, tag)?;
            if !assets.is_empty() {
                return Ok(assets);
            }
        }
        Ok(Vec::new())
    }

    /// Download a release asset from the given URL.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if the server returned 404 (resource
    /// does not exist), or `Err` for any other failure (network errors, non-404 HTTP errors).
    fn try_download_to_file(&self, url: &str, path: &Path) -> Result<bool> {
        let response = self
            .http_client
            .get_retrying_status(url, Self::should_retry_status)?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            return Self::non_success_response_error(url, response);
        }

        HttpClient::response_body_to_file(response, url, path)?;

        Ok(true)
    }

    /// GitHub rate limiting can be reported as HTTP 403 or 429, and callers should not retry
    /// errors that are indicative of rate limiting. Let those statuses return to this provider so
    /// [`Self::non_success_response_error`] can decide whether they are throttle responses; keep
    /// ordinary server errors retryable through [`HttpClient`].
    fn should_retry_status(status: StatusCode) -> bool {
        status.is_server_error()
    }

    /// Classify a non-success GitHub API or release-asset response.
    ///
    /// GitHub uses HTTP 404 for an absent release tag or asset, which callers handle before this
    /// function. This helper checks the remaining GitHub-specific throttle signals on HTTP 403/429
    /// and otherwise preserves the response as a normal HTTP status error.
    fn non_success_response_error<T>(url: &str, response: reqwest::blocking::Response) -> Result<T> {
        let status = response.status();
        let headers = response.headers().clone();
        let body = if status == StatusCode::FORBIDDEN && !Self::is_throttled_response(status, &headers, None)
        {
            match HttpClient::response_body_to_string(response, url, SMALL_DOWNLOAD_LIMIT_BYTES) {
                Ok(text) => Some(text),
                Err(error::Error::HttpResponseTooLarge { .. }) => None,
                Err(source) => return Err(source),
            }
        } else {
            None
        };

        if Self::is_throttled_response(status, &headers, body.as_deref()) {
            return Err(Self::provider_throttled_error(url, status));
        }

        error::HttpStatusSnafu {
            url: url.to_string(),
            status: status.as_u16(),
        }
        .fail()
    }

    /// Return whether a GitHub HTTP response represents API throttling rather than an ordinary
    /// authorization or client error.
    ///
    /// GitHub can report rate limiting with HTTP 429 directly, or with HTTP 403 plus headers such
    /// as `retry-after` / `x-ratelimit-remaining: 0`, or with a rate-limit message in the response
    /// body.
    fn is_throttled_response(status: StatusCode, headers: &HeaderMap, body: Option<&str>) -> bool {
        if status == StatusCode::TOO_MANY_REQUESTS {
            return true;
        }

        status == StatusCode::FORBIDDEN
            && (Self::header_value(headers, "retry-after").is_some()
                || Self::header_value(headers, "x-ratelimit-remaining").as_deref() == Some("0")
                || body.is_some_and(Self::is_rate_limit_body))
    }

    /// Detect GitHub's textual rate-limit responses when the HTTP 403 headers alone are not enough
    /// to distinguish throttling from ordinary forbidden access.
    fn is_rate_limit_body(body: &str) -> bool {
        let body = body.to_ascii_lowercase();
        body.contains("rate limit") || body.contains("rate-limit") || body.contains("too many requests")
    }

    /// Build the provider-throttle error after GitHub-specific response classification has already
    /// decided that the status represents rate limiting.
    fn provider_throttled_error(url: &str, status: StatusCode) -> error::Error {
        error::ProviderThrottledSnafu {
            url: url.to_string(),
            status: status.as_u16(),
        }
        .build()
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }
}

impl Provider for GithubProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::GithubReleases
    }

    fn try_resolve(&self, krate: &DownloadedCrate, target: &TargetTriple) -> Result<ConclusiveResolution> {
        let repo_url = if let Some(url) = Self::get_repo_url(krate)? {
            url
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    "no repository URL available",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let Some((owner, repo)) = Self::parse_owner_repo(&repo_url) else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    format!("could not parse owner/repo from URL: {}", repo_url),
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let Some(api_base) = Self::api_base(&repo_url) else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    format!("could not determine API base for URL: {}", repo_url),
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let version = krate.resolved.version.to_string();
        let crate_name = krate.resolved.name.as_str();

        // Try each tag form the release might use (see [`super::generate_candidate_tags`]);
        // stop at the first that returns assets.
        let tags = super::generate_candidate_tags(crate_name, &version);
        let assets = self.find_release_assets_for_tags(&api_base, owner, repo, &tags)?;

        if assets.is_empty() {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    "no release found for any tag variant",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        }

        let binary_names = krate.binary_names()?;
        let extra_binary_names: Vec<&str> = binary_names
            .iter()
            .map(String::as_str)
            .filter(|n| *n != crate_name)
            .collect();
        let candidates =
            super::generate_candidate_filenames(crate_name, &extra_binary_names, &version, target);

        let asset_map: std::collections::HashMap<&str, &str> = assets
            .iter()
            .map(|(name, url)| (name.as_str(), url.as_str()))
            .collect();

        let matched = candidates
            .iter()
            .find_map(|c| asset_map.get(c.filename.as_str()).map(|url| (c, *url)));

        let (candidate, download_url) = if let Some(m) = matched {
            m
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    "no matching asset found in release",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        self.reporter.report(|| {
            PrebuiltBinaryMessage::downloading_binary(download_url, BinaryProvider::GithubReleases)
        });

        let binary_name = krate.default_binary_name()?;
        let expected_binary_names =
            super::expected_binary_names(&binary_name, Some(&candidate.binary_basename), crate_name);

        let work_dir = super::recreate_staging_work_dir(&self.staging_dir, &krate.resolved)?;

        let archive_path = work_dir.join(candidate.format.canonical_filename());
        match self.try_download_to_file(download_url, &archive_path) {
            Ok(true) => {}
            Ok(false) => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::provider_has_no_binary(
                        BinaryProvider::GithubReleases,
                        format!("Release artifact not found: {}", download_url),
                    )
                });
                return Ok(ConclusiveResolution::Nonexistent);
            }
            Err(e) => return Err(e),
        }

        if self.verify_checksums {
            let checksum_url = format!("{}.sha256", download_url);
            let checksum_path = work_dir.join("checksum.sha256");
            let checksum_found =
                self.try_download_to_file(&checksum_url, &checksum_path)
                    .map_err(|source| {
                        super::provider_asset_preparation_failed(
                            BinaryProvider::GithubReleases,
                            download_url,
                            source,
                        )
                    })?;
            if checksum_found {
                super::checksum::verify_sha256_checksum(
                    &archive_path,
                    &checksum_path,
                    &candidate.filename,
                    &self.reporter,
                )
                .map_err(|source| {
                    super::provider_asset_preparation_failed(
                        BinaryProvider::GithubReleases,
                        download_url,
                        source,
                    )
                })?;
            }
        }

        let extract_dir = work_dir.join("extracted");
        let binary_path = super::extract_binary_by_candidate_names(
            &archive_path,
            candidate.format,
            &expected_binary_names,
            &extract_dir,
        )
        .map_err(|source| {
            super::provider_asset_preparation_failed(BinaryProvider::GithubReleases, download_url, source)
        })?;

        let staged_path = super::stage_extracted_binary(&work_dir, &binary_name, target, &binary_path)?;
        Ok(ConclusiveResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::GithubReleases,
            path: staged_path,
            target: candidate.target.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use assert_matches::assert_matches;
    use httpmock::prelude::*;
    use semver::Version;
    use url::Url;

    use super::*;
    use crate::{
        bin_resolver::providers::generate_candidate_tags, config::HttpConfig, crate_resolver::ResolvedSource,
        cratespec::Forge, error::Error, messages::MessageReporter,
    };

    fn fast_retry_config() -> HttpConfig {
        HttpConfig {
            retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(10),
            ..Default::default()
        }
    }

    fn test_provider() -> (GithubProvider, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let http_client = HttpClient::new(&fast_retry_config()).unwrap();
        (
            GithubProvider::new(MessageReporter::null(), &temp_dir, false, http_client),
            temp_dir,
        )
    }

    #[test]
    fn test_parse_owner_repo_standard() {
        let (owner, repo) = GithubProvider::parse_owner_repo("https://github.com/eza-community/eza").unwrap();
        assert_eq!(owner, "eza-community");
        assert_eq!(repo, "eza");
    }

    #[test]
    fn test_parse_owner_repo_enterprise() {
        let (owner, repo) =
            GithubProvider::parse_owner_repo("https://github.enterprise.com/myorg/myrepo").unwrap();
        assert_eq!(owner, "myorg");
        assert_eq!(repo, "myrepo");
    }

    #[test]
    fn test_parse_owner_repo_invalid() {
        assert!(GithubProvider::parse_owner_repo("https://github.com/").is_none());
        assert!(GithubProvider::parse_owner_repo("not-a-url").is_none());
    }

    #[test]
    fn test_api_base_github_com() {
        assert_eq!(
            GithubProvider::api_base("https://github.com/owner/repo"),
            Some("https://api.github.com".to_string())
        );
    }

    #[test]
    fn test_api_base_enterprise() {
        assert_eq!(
            GithubProvider::api_base("https://github.enterprise.com/owner/repo"),
            Some("https://github.enterprise.com/api/v3".to_string())
        );
    }

    #[test]
    fn test_get_repo_url_github_forge() {
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn test_get_repo_url_github_forge_custom_url() {
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: Some(Url::parse("https://github.enterprise.com").unwrap()),
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(
            url,
            Some("https://github.enterprise.com/myowner/myrepo".to_string())
        );
    }

    #[test]
    fn test_get_repo_url_crates_io_queries_cargo_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://github.com/myowner/myrepo"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::CratesIo,
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn test_get_repo_url_crates_io_non_github_returns_none() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://gitlab.com/myowner/myrepo"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::CratesIo,
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, None);
    }

    #[test]
    fn test_get_repo_url_gitlab_forge_falls_back_to_cargo_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://github.com/upstream/mytool"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitLab {
                        custom_url: None,
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/upstream/mytool".to_string()));
    }

    #[test]
    fn test_get_repo_url_git_source_falls_back_to_cargo_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://github.com/owner/mytool"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Git {
                    repo: "https://some-git.example.com/owner/mytool.git".to_string(),
                    commit: "abc123".to_string(),
                },
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GithubProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/owner/mytool".to_string()));
    }

    #[test]
    fn list_release_assets_429_is_not_retried_and_becomes_provider_throttled() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/repos/owner/repo/releases/tags/v1.0.0");
            then.status(429).header("retry-after", "60");
        });
        let (provider, _temp_dir) = test_provider();

        let result = provider.list_release_assets(&server.base_url(), "owner", "repo", "v1.0.0");

        assert_matches!(result, Err(Error::ProviderThrottled { status: 429, .. }));
        mock.assert_calls(1);
    }

    #[test]
    fn list_release_assets_403_with_zero_remaining_becomes_provider_throttled() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/repos/owner/repo/releases/tags/v1.0.0");
            then.status(403)
                .header("x-ratelimit-remaining", "0")
                .header("x-ratelimit-reset", "1710000000");
        });
        let (provider, _temp_dir) = test_provider();

        let result = provider.list_release_assets(&server.base_url(), "owner", "repo", "v1.0.0");

        assert_matches!(result, Err(Error::ProviderThrottled { status: 403, .. }));
        mock.assert_calls(1);
    }

    #[test]
    fn list_release_assets_404_returns_empty_assets() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/repos/owner/repo/releases/tags/v1.0.0");
            then.status(404);
        });
        let (provider, _temp_dir) = test_provider();

        let assets = provider
            .list_release_assets(&server.base_url(), "owner", "repo", "v1.0.0")
            .unwrap();

        assert!(assets.is_empty());
        mock.assert_calls(1);
    }

    /// JSON body for a release whose only asset is `name`, for mocking the releases API.
    fn release_json(name: &str) -> String {
        format!(
            r#"{{"assets": [{{"name": "{name}", "browser_download_url": "https://example.com/{name}"}}]}}"#
        )
    }

    /// Mock a 404 for the given release tag path segment, for tags that must be probed and missed.
    fn mock_tag_404<'a>(server: &'a MockServer, tag_segment: &str) -> httpmock::Mock<'a> {
        let path = format!("/repos/owner/repo/releases/tags/{}", tag_segment);
        server.mock(|when, then| {
            when.method(GET).path(path);
            then.status(404);
        })
    }

    #[test]
    fn find_release_assets_stops_at_first_tag_with_assets() {
        let server = MockServer::start();
        let first = server.mock(|when, then| {
            when.method(GET).path("/repos/owner/repo/releases/tags/v1.0.0");
            then.status(200)
                .body(release_json("mytool-1.0.0-x86_64-unknown-linux-gnu.tar.gz"));
        });
        let later_tags = [
            "1.0.0",
            "mytool-v1.0.0",
            "mytool-1.0.0",
            "mytool%2Fv1.0.0",
            "mytool%2F1.0.0",
        ];
        let later_mocks: Vec<_> = later_tags.iter().map(|t| mock_tag_404(&server, t)).collect();
        let (provider, _temp_dir) = test_provider();

        let tags = generate_candidate_tags("mytool", "1.0.0");
        let assets = provider
            .find_release_assets_for_tags(&server.base_url(), "owner", "repo", &tags)
            .unwrap();

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].0, "mytool-1.0.0-x86_64-unknown-linux-gnu.tar.gz");
        first.assert_calls(1);
        for mock in &later_mocks {
            mock.assert_calls(0);
        }
    }

    /// The cargo-nextest shape: the first three tag forms 404 and the `{name}-{version}` form is
    /// the release's actual tag. The two slash forms after it must never be probed.
    #[test]
    fn find_release_assets_falls_through_404s_to_name_prefixed_tag() {
        let server = MockServer::start();
        let missed_mocks: Vec<_> = ["v1.0.0", "1.0.0", "mytool-v1.0.0"]
            .iter()
            .map(|t| mock_tag_404(&server, t))
            .collect();
        let hit = server.mock(|when, then| {
            when.method(GET)
                .path("/repos/owner/repo/releases/tags/mytool-1.0.0");
            then.status(200)
                .body(release_json("mytool-1.0.0-x86_64-unknown-linux-gnu.tar.gz"));
        });
        let slash_mocks: Vec<_> = ["mytool%2Fv1.0.0", "mytool%2F1.0.0"]
            .iter()
            .map(|t| mock_tag_404(&server, t))
            .collect();
        let (provider, _temp_dir) = test_provider();

        let tags = generate_candidate_tags("mytool", "1.0.0");
        let assets = provider
            .find_release_assets_for_tags(&server.base_url(), "owner", "repo", &tags)
            .unwrap();

        assert_eq!(assets[0].0, "mytool-1.0.0-x86_64-unknown-linux-gnu.tar.gz");
        hit.assert_calls(1);
        for mock in &missed_mocks {
            mock.assert_calls(1);
        }
        for mock in &slash_mocks {
            mock.assert_calls(0);
        }
    }

    /// Slash-form subcrate tags must reach the API as a single percent-encoded path segment.
    #[test]
    fn list_release_assets_percent_encodes_slash_tags() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/repos/owner/repo/releases/tags/mytool%2Fv1.0.0");
            then.status(200).body(release_json("mytool-v1.0.0.tar.gz"));
        });
        let (provider, _temp_dir) = test_provider();

        let assets = provider
            .list_release_assets(&server.base_url(), "owner", "repo", "mytool/v1.0.0")
            .unwrap();

        assert_eq!(assets[0].0, "mytool-v1.0.0.tar.gz");
        mock.assert_calls(1);
    }
}
