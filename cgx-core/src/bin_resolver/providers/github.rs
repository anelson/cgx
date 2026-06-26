#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use snafu::ResultExt;

use super::Provider;
use crate::{
    Result,
    bin_resolver::{BinaryResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    cratespec::Forge,
    downloader::DownloadedCrate,
    error,
    http::{ACCEPT, AUTHORIZATION, Bytes, HeaderMap, HeaderValue, HttpClient},
    messages::PrebuiltBinaryMessage,
};

pub(in crate::bin_resolver) struct GithubProvider {
    reporter: crate::messages::MessageReporter,
    cache_dir: PathBuf,
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
        cache_dir: PathBuf,
        verify_checksums: bool,
        http_client: HttpClient,
    ) -> Self {
        Self {
            reporter,
            cache_dir,
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
        let url = format!("{}/repos/{}/{}/releases/tags/{}", api_base, owner, repo, tag);

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            if let Ok(auth_value) = HeaderValue::from_str(&format!("token {}", token)) {
                headers.insert(AUTHORIZATION, auth_value);
            }
        }

        let response = self.http_client.get_with_headers(&url, &headers)?;

        if !response.status().is_success() {
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                // The API call worked, and Github is telling us that there are not release assets
                // for this resource.  From the caller's perspective that isn't an error at all, it
                // is a conclusive, negative result.
                return Ok(Vec::new());
            }

            return error::HttpStatusSnafu {
                url: url.clone(),
                status: response.status().as_u16(),
            }
            .fail();
        }

        let text = response
            .text()
            .with_context(|_| error::HttpRequestSnafu { url: url.clone() })?;

        let release: ReleaseResponse = serde_json::from_str(&text).context(error::JsonSnafu)?;

        Ok(release
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect())
    }

    /// Download a file from the given URL.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404 (resource
    /// does not exist), or `Err` for any other failure (network errors, non-404 HTTP errors).
    fn try_download(&self, url: &str) -> Result<Option<Bytes>> {
        self.http_client.try_download(url)
    }

    fn verify_checksum(&self, data: &[u8], url: &str) -> Result<()> {
        let checksum_url = format!("{}.sha256", url);

        let checksum_data = match self.try_download(&checksum_url)? {
            Some(data) => data,
            None => return Ok(()),
        };

        let checksum_str = String::from_utf8_lossy(&checksum_data);
        let expected_hash = checksum_str.split_whitespace().next().ok_or_else(|| {
            error::ChecksumMismatchSnafu {
                expected: checksum_str.to_string(),
                actual: "invalid checksum format".to_string(),
            }
            .build()
        })?;

        self.reporter
            .report(|| PrebuiltBinaryMessage::verifying_checksum(expected_hash));

        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_hash = crate::helpers::format_hex_lower(hasher.finalize());

        if expected_hash != actual_hash {
            return error::ChecksumMismatchSnafu {
                expected: expected_hash.to_string(),
                actual: actual_hash,
            }
            .fail();
        }

        self.reporter.report(PrebuiltBinaryMessage::checksum_verified);

        Ok(())
    }
}

impl Provider for GithubProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::GithubReleases
    }

    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<BinaryResolution> {
        let repo_url = if let Some(url) = Self::get_repo_url(krate)? {
            url
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    "no repository URL available",
                )
            });
            return Ok(BinaryResolution::Nonexistent);
        };

        let Some((owner, repo)) = Self::parse_owner_repo(&repo_url) else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    format!("could not parse owner/repo from URL: {}", repo_url),
                )
            });
            return Ok(BinaryResolution::Nonexistent);
        };

        let Some(api_base) = Self::api_base(&repo_url) else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    format!("could not determine API base for URL: {}", repo_url),
                )
            });
            return Ok(BinaryResolution::Nonexistent);
        };

        let version = krate.resolved.version.to_string();

        // Try both v{version} and {version} tags; stop at the first that returns assets. A transient
        // failure (rate limit, network) on a tag is remembered: if no tag yields assets and at least
        // one lookup failed transiently, the result is inconclusive.  To produce a definitive
        // negative result, all lookups must succeed and return no assets.
        let tags = [format!("v{}", version), version.clone()];
        let mut assets = Vec::new();
        let mut transient: Option<Box<error::Error>> = None;
        for tag in &tags {
            match self.list_release_assets(&api_base, owner, repo, tag) {
                Ok(a) if !a.is_empty() => {
                    assets = a;
                    break;
                }
                Ok(_) => {}
                Err(e) if e.is_transient_http_error() => {
                    if transient.is_none() {
                        transient = Some(Box::new(e));
                    }
                }
                Err(e) => return Err(e),
            }
        }

        if assets.is_empty() {
            if let Some(source) = transient {
                tracing::debug!(error = %source,
                    "GitHub release lookup failed transiently; result is inconclusive");
                return Ok(BinaryResolution::Inconclusive { source });
            }
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GithubReleases,
                    "no release found for any tag variant",
                )
            });
            return Ok(BinaryResolution::Nonexistent);
        }

        let crate_name = krate.resolved.name.as_str();
        let binary_names = krate.binary_names()?;
        let extra_binary_names: Vec<&str> = binary_names
            .iter()
            .map(String::as_str)
            .filter(|n| *n != crate_name)
            .collect();
        let candidates =
            super::generate_candidate_filenames(crate_name, &extra_binary_names, &version, platform);

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
            return Ok(BinaryResolution::Nonexistent);
        };

        self.reporter.report(|| {
            PrebuiltBinaryMessage::downloading_binary(download_url, BinaryProvider::GithubReleases)
        });

        let data = match self.try_download(download_url) {
            Ok(Some(data)) => data,
            Ok(None) => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::provider_has_no_binary(
                        BinaryProvider::GithubReleases,
                        format!("Release artifact not found: {}", download_url),
                    )
                });
                return Ok(BinaryResolution::Nonexistent);
            }
            Err(e) if e.is_transient_http_error() => {
                return Ok(BinaryResolution::Inconclusive { source: Box::new(e) });
            }
            Err(e) => return Err(e),
        };

        if self.verify_checksums {
            self.verify_checksum(&data, download_url)?;
        }

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir.path().join(candidate.format.canonical_filename());
        std::fs::write(&archive_path, &data).with_context(|_| error::IoSnafu {
            path: archive_path.clone(),
        })?;

        let binary_name = krate.default_binary_name()?;
        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = super::extract_binary(&archive_path, candidate.format, &binary_name, &extract_dir)?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("github")
            .join(&krate.resolved.name)
            .join(krate.resolved.version.to_string())
            .join(platform);

        std::fs::create_dir_all(&final_dir).with_context(|_| error::IoSnafu {
            path: final_dir.clone(),
        })?;

        let final_path = final_dir.join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));
        std::fs::copy(&binary_path, &final_path).with_context(|_| error::IoSnafu {
            path: final_path.clone(),
        })?;

        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&final_path)
                .with_context(|_| error::IoSnafu {
                    path: final_path.clone(),
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&final_path, perms).with_context(|_| error::IoSnafu {
                path: final_path.clone(),
            })?;
        }

        Ok(BinaryResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::GithubReleases,
            path: final_path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use semver::Version;
    use url::Url;

    use super::*;
    use crate::{crate_resolver::ResolvedSource, cratespec::Forge};

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
}
