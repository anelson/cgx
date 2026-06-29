#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use snafu::ResultExt;

use super::{ArchiveFormat, CandidateFilename, Provider};
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    cratespec::Forge,
    downloader::DownloadedCrate,
    error,
    http::{Bytes, HttpClient},
    messages::PrebuiltBinaryMessage,
};

pub(in crate::bin_resolver) struct GitlabProvider {
    reporter: crate::messages::MessageReporter,
    cache_dir: PathBuf,
    verify_checksums: bool,
    http_client: HttpClient,
}

impl GitlabProvider {
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

    /// Get the GitLab repository URL for a crate.
    ///
    /// If the crate came from a GitLab forge, the forge URL is used directly (handles the fork
    /// scenario where Cargo.toml may still point to the upstream). For all other sources
    /// (including non-GitLab forges), falls back to the `[package].repository` field in
    /// Cargo.toml only when it is a `https://gitlab.com/...` URL.
    fn get_repo_url(krate: &DownloadedCrate) -> Result<Option<String>> {
        match &krate.resolved.source {
            ResolvedSource::Forge {
                forge: forge @ Forge::GitLab { .. },
                ..
            } => Ok(Some(forge.repo_url())),
            ResolvedSource::Forge { .. }
            | ResolvedSource::CratesIo
            | ResolvedSource::Registry { .. }
            | ResolvedSource::Git { .. }
            | ResolvedSource::LocalDir { .. } => Ok(krate
                .repository_url()?
                .filter(|u| u.starts_with("https://gitlab.com/"))),
        }
    }

    /// Generate candidate URLs for GitLab releases, each paired with its [`ArchiveFormat`].
    ///
    /// Uses the shared filename generator and constructs full GitLab release download URLs
    /// for both `v{version}` and `{version}` tags.
    fn generate_urls(
        repo_url: &str,
        crate_name: &str,
        extra_binary_names: &[&str],
        version: &str,
        platform: &str,
    ) -> Vec<(String, ArchiveFormat)> {
        let candidates =
            super::generate_candidate_filenames(crate_name, extra_binary_names, version, platform);
        let tags = [format!("v{}", version), version.to_string()];

        let mut urls = Vec::new();
        for tag in &tags {
            for CandidateFilename { filename, format } in &candidates {
                urls.push((
                    format!("{}/-/releases/{}/downloads/binaries/{}", repo_url, tag, filename),
                    *format,
                ));
            }
        }
        urls
    }

    /// Probe a GitLab release download URL with a HEAD request to check if the asset exists.
    ///
    /// GitLab release asset URLs return success for existing assets. HTTP 404 and other ordinary
    /// non-success statuses are treated as "this candidate is not present"; HTTP 429 is GitLab's
    /// rate-limit signal and is returned as a provider throttle error.
    fn head_probe(&self, url: &str) -> Result<bool> {
        let response = self
            .http_client
            .head_retrying_status(url, Self::should_retry_status)?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(Self::provider_throttled_error(url, response.status()));
        }
        Ok(response.status().is_success())
    }

    /// Download a GitLab release asset from the given URL.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404 (resource
    /// does not exist), or `Err` for any other failure (network errors, non-404 HTTP errors).
    fn try_download(&self, url: &str) -> Result<Option<Bytes>> {
        let response = self
            .http_client
            .get_retrying_status(url, Self::should_retry_status)?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(Self::provider_throttled_error(url, response.status()));
        }

        if !response.status().is_success() {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: response.status().as_u16(),
            }
            .fail();
        }

        let bytes = response
            .bytes()
            .with_context(|_| error::HttpRequestSnafu { url: url.to_string() })?;

        Ok(Some(bytes))
    }

    fn should_retry_status(status: StatusCode) -> bool {
        // GitLab rate limiting is reported as HTTP 429, so that should not be retried.
        // Ordinary server errors still use the standard HTTP retry path.
        status.is_server_error()
    }

    fn provider_throttled_error(url: &str, status: StatusCode) -> error::Error {
        // Build the provider-throttle error after GitLab-specific response classification has already
        // identified HTTP 429 as rate limiting.
        error::ProviderThrottledSnafu {
            url: url.to_string(),
            status: status.as_u16(),
        }
        .build()
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

impl Provider for GitlabProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::GitlabReleases
    }

    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<ConclusiveResolution> {
        let repo_url = if let Some(url) = Self::get_repo_url(krate)? {
            url
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    "no repository URL available",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let crate_name = krate.resolved.name.as_str();
        let binary_names = krate.binary_names()?;
        let extra_binary_names: Vec<&str> = binary_names
            .iter()
            .map(String::as_str)
            .filter(|n| *n != crate_name)
            .collect();
        let urls = Self::generate_urls(
            &repo_url,
            crate_name,
            &extra_binary_names,
            &krate.resolved.version.to_string(),
            platform,
        );

        // Probe sequentially with HEAD requests
        //
        // If we hit a connection/timeout error, bail immediately rather than continuing to probe
        // every candidate URL against a broken/unresponsive server. The candidate set is the full
        // cross-product of names (crate + binary), platform aliases, archive formats, and tag
        // variants, so the worst-case (no asset exists) is on the order of ~1,400 sequential
        // HEADs. That only happens for a crate actually hosted on gitlab.com whose release lacks
        // any matching asset (a slow fallback to a source build); `get_repo_url` short-circuits to
        // `None` for every non-GitLab crate, which is the overwhelming majority and does zero
        // probes.
        let mut found = None;
        for (url, format) in &urls {
            match self.head_probe(url) {
                Ok(true) => {
                    found = Some((url.clone(), *format));
                    break;
                }

                Err(e) => return Err(e),
                Ok(false) => continue,
            }
        }
        let Some((url, format)) = found else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    "no matching release found",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        self.reporter
            .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::GitlabReleases));

        let data = match self.try_download(&url) {
            Ok(Some(data)) => data,
            Ok(None) => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::provider_has_no_binary(
                        BinaryProvider::GitlabReleases,
                        format!("Release asset not found: {}", url),
                    )
                });
                return Ok(ConclusiveResolution::Nonexistent);
            }
            Err(e) => return Err(e),
        };

        if self.verify_checksums {
            self.verify_checksum(&data, &url)?;
        }

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir.path().join(format.canonical_filename());
        std::fs::write(&archive_path, &data).with_context(|_| error::IoSnafu {
            path: archive_path.clone(),
        })?;

        let binary_name = krate.default_binary_name()?;
        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = super::extract_binary(&archive_path, format, &binary_name, &extract_dir)?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("gitlab")
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

        Ok(ConclusiveResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::GitlabReleases,
            path: final_path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use assert_matches::assert_matches;
    use httpmock::{Method::HEAD, prelude::*};
    use semver::Version;
    use url::Url;

    use super::*;
    use crate::{
        config::HttpConfig, crate_resolver::ResolvedSource, cratespec::Forge, error::Error,
        messages::MessageReporter,
    };

    fn fast_retry_config() -> HttpConfig {
        HttpConfig {
            retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(10),
            ..Default::default()
        }
    }

    fn test_provider() -> (GitlabProvider, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let http_client = HttpClient::new(&fast_retry_config()).unwrap();
        (
            GitlabProvider::new(
                MessageReporter::null(),
                temp_dir.path().to_path_buf(),
                false,
                http_client,
            ),
            temp_dir,
        )
    }

    #[test]
    fn test_url_generation_includes_version_patterns() {
        let urls = GitlabProvider::generate_urls(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            "x86_64-unknown-linux-gnu",
        );

        assert!(urls.iter().any(|(url, _)| url.contains("/-/releases/v1.2.3/")));
        assert!(urls.iter().any(|(url, _)| url.contains("/-/releases/1.2.3/")));
    }

    #[test]
    fn test_url_generation_uses_gitlab_path() {
        let urls = GitlabProvider::generate_urls(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            "x86_64-unknown-linux-gnu",
        );

        assert!(urls.iter().all(|(url, _)| url.contains("/-/releases/")));
        assert!(urls.iter().all(|(url, _)| url.contains("/downloads/binaries/")));
    }

    #[test]
    fn test_url_generation_includes_archive_formats() {
        let urls = GitlabProvider::generate_urls(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            "x86_64-unknown-linux-gnu",
        );

        assert!(urls.iter().any(|(url, _)| url.ends_with(".tar.gz")));
        assert!(urls.iter().any(|(url, _)| url.ends_with(".tar.xz")));
        assert!(urls.iter().any(|(url, _)| url.ends_with(".tar.zst")));
        assert!(urls.iter().any(|(url, _)| url.ends_with(".zip")));
    }

    #[test]
    fn test_url_generation_carries_format() {
        let urls = GitlabProvider::generate_urls(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            "x86_64-unknown-linux-gnu",
        );

        for (url, format) in &urls {
            if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
                assert_eq!(*format, ArchiveFormat::TarGz);
            } else if url.ends_with(".tar.xz") {
                assert_eq!(*format, ArchiveFormat::TarXz);
            } else if url.ends_with(".tar.zst") {
                assert_eq!(*format, ArchiveFormat::TarZst);
            } else if url.ends_with(".tar.bz2") {
                assert_eq!(*format, ArchiveFormat::TarBz2);
            } else if url.ends_with(".tar") {
                assert_eq!(*format, ArchiveFormat::Tar);
            } else if url.ends_with(".zip") {
                assert_eq!(*format, ArchiveFormat::Zip);
            } else if url.ends_with(".gz") {
                assert_eq!(*format, ArchiveFormat::Gz);
            } else if url.ends_with(".xz") {
                assert_eq!(*format, ArchiveFormat::Xz);
            } else if url.ends_with(".zst") {
                assert_eq!(*format, ArchiveFormat::Zst);
            } else if url.ends_with(".bz2") {
                assert_eq!(*format, ArchiveFormat::Bz2);
            } else {
                assert_eq!(*format, ArchiveFormat::NakedBinary);
            }
        }
    }

    #[test]
    fn test_get_repo_url_gitlab_forge() {
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
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://gitlab.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn test_get_repo_url_gitlab_forge_custom_url() {
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitLab {
                        custom_url: Some(Url::parse("https://gitlab.company.com").unwrap()),
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://gitlab.company.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn test_get_repo_url_github_forge_no_gitlab_repo_returns_none() {
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
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, None);
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

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://gitlab.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn test_get_repo_url_crates_io_non_gitlab_returns_none() {
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

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, None);
    }

    #[test]
    fn test_get_repo_url_github_forge_falls_back_to_cargo_toml() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://gitlab.com/upstream/mytool"
"#,
        )
        .unwrap();

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
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = GitlabProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://gitlab.com/upstream/mytool".to_string()));
    }

    #[test]
    fn head_probe_429_is_not_retried_and_becomes_provider_throttled() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/asset");
            then.status(429).header("retry-after", "30");
        });
        let (provider, _temp_dir) = test_provider();

        let result = provider.head_probe(&server.url("/asset"));

        assert_matches!(result, Err(Error::ProviderThrottled { status: 429, .. }));
        mock.assert_calls(1);
    }

    #[test]
    fn head_probe_5xx_still_retries() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/asset");
            then.status(500);
        });
        let (provider, _temp_dir) = test_provider();

        let result = provider.head_probe(&server.url("/asset"));

        assert_matches!(result, Err(Error::HttpStatus { status: 500, .. }));
        mock.assert_calls(3);
    }
}
