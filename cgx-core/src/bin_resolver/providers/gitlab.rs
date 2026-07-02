#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use reqwest::StatusCode;
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
    http::HttpClient,
    messages::PrebuiltBinaryMessage,
    target::TargetTriple,
};

/// A generated GitLab release asset candidate and the metadata needed to consume it.
///
/// There's not a GitLab API endpoint to list release assets, unlike with GitHub, so to discover a
/// release asset heuristically we need to generate a lot of these candidates and then probbe them
/// one by one to see if any of them exist.
#[derive(Debug, Clone)]
struct GitlabReleaseAssetCandidate {
    /// Full GitLab release download URL for this candidate asset.
    url: String,

    /// File format inferred from the candidate asset filename.
    ///
    /// This determines whether the downloaded asset is copied directly, decompressed,
    /// or treated as an archive that must be searched for the binary.
    archive_format: ArchiveFormat,

    /// Release asset filename, without the surrounding URL path.
    ///
    /// Used for checksum sidecar matching and diagnostics that should mention the
    /// concrete asset name.
    asset_filename: String,

    /// Binary basename implied by the generated asset filename.
    ///
    /// Used as a fallback expected binary name when searching inside archives, since
    /// some projects publish assets under a name that differs from the crate's default
    /// binary name.
    binary_basename_hint: String,
}

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

    /// Generate GitLab release asset candidates to probe and download.
    ///
    /// Uses the came candidate filename generator in [`super::generate_candidate_filenames`], then
    /// adds some Gitlab-specific fields to produce a list of [`GitlabReleaseAssetCandidate`]s that
    /// can be probed for existence.
    fn generate_release_asset_candidates(
        repo_url: &str,
        crate_name: &str,
        extra_binary_names: &[&str],
        version: &str,
        target: &TargetTriple,
    ) -> Vec<GitlabReleaseAssetCandidate> {
        // Generate candidate filenames for the crate and platform
        let filename_candidates =
            super::generate_candidate_filenames(crate_name, extra_binary_names, version, target);

        // Make a separate Gitlab release asset candidate for each possible variant the release tag
        // might have (with or without a leading "v" prefix).
        let tags = [format!("v{}", version), version.to_string()];

        let mut asset_candidates = Vec::new();
        for tag in &tags {
            for CandidateFilename {
                filename,
                binary_basename,
                format,
            } in &filename_candidates
            {
                asset_candidates.push(GitlabReleaseAssetCandidate {
                    url: format!("{}/-/releases/{}/downloads/binaries/{}", repo_url, tag, filename),
                    archive_format: *format,
                    asset_filename: filename.clone(),
                    binary_basename_hint: binary_basename.clone(),
                });
            }
        }
        asset_candidates
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
    /// Returns `Ok(true)` on success, `Ok(false)` if the server returned 404 (resource
    /// does not exist), or `Err` for any other failure (network errors, non-404 HTTP errors).
    fn try_download_to_file(&self, url: &str, path: &Path) -> Result<bool> {
        self.http_client
            .try_download_to_file_retrying_status(url, path, Self::should_retry_status)
            .map_err(|source| Self::map_download_error(url, source))
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

    fn map_download_error(url: &str, source: error::Error) -> error::Error {
        match source {
            error::Error::HttpStatus { status, .. } if status == StatusCode::TOO_MANY_REQUESTS.as_u16() => {
                Self::provider_throttled_error(url, StatusCode::TOO_MANY_REQUESTS)
            }
            source => source,
        }
    }
}

impl Provider for GitlabProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::GitlabReleases
    }

    fn try_resolve(&self, krate: &DownloadedCrate, target: &TargetTriple) -> Result<ConclusiveResolution> {
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
        let candidates = Self::generate_release_asset_candidates(
            &repo_url,
            crate_name,
            &extra_binary_names,
            &krate.resolved.version.to_string(),
            target,
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
        for candidate in &candidates {
            match self.head_probe(&candidate.url) {
                Ok(true) => {
                    found = Some(candidate.clone());
                    break;
                }

                Err(e) => return Err(e),
                Ok(false) => continue,
            }
        }
        let Some(candidate) = found else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    "no matching release found",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        self.reporter.report(|| {
            PrebuiltBinaryMessage::downloading_binary(&candidate.url, BinaryProvider::GitlabReleases)
        });

        let binary_name = krate.default_binary_name()?;
        let expected_binary_names =
            super::expected_binary_names(&binary_name, Some(&candidate.binary_basename_hint), crate_name);

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir
            .path()
            .join(candidate.archive_format.canonical_filename());
        match self.try_download_to_file(&candidate.url, &archive_path) {
            Ok(true) => {}
            Ok(false) => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::provider_has_no_binary(
                        BinaryProvider::GitlabReleases,
                        format!("Release asset not found: {}", candidate.url),
                    )
                });
                return Ok(ConclusiveResolution::Nonexistent);
            }
            Err(e) => return Err(e),
        }

        if self.verify_checksums {
            let checksum_url = format!("{}.sha256", candidate.url);
            let checksum_path = temp_dir.path().join("checksum.sha256");
            let checksum_found =
                self.try_download_to_file(&checksum_url, &checksum_path)
                    .map_err(|source| {
                        super::provider_asset_preparation_failed(
                            BinaryProvider::GitlabReleases,
                            &candidate.url,
                            source,
                        )
                    })?;
            if checksum_found {
                super::checksum::verify_sha256_checksum(
                    &archive_path,
                    &checksum_path,
                    &candidate.asset_filename,
                    &self.reporter,
                )
                .map_err(|source| {
                    super::provider_asset_preparation_failed(
                        BinaryProvider::GitlabReleases,
                        &candidate.url,
                        source,
                    )
                })?;
            }
        }

        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = super::extract_binary_by_candidate_names(
            &archive_path,
            candidate.archive_format,
            &expected_binary_names,
            &extract_dir,
        )
        .map_err(|source| {
            super::provider_asset_preparation_failed(BinaryProvider::GitlabReleases, &candidate.url, source)
        })?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("gitlab")
            .join(&krate.resolved.name)
            .join(krate.resolved.version.to_string())
            .join(target.as_str());

        std::fs::create_dir_all(&final_dir).with_context(|_| error::IoSnafu {
            path: final_dir.clone(),
        })?;

        let final_path = final_dir.join(format!("{}{}", binary_name, target.binary_ext()));
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

    fn target_triple(target: &'static str) -> TargetTriple {
        TargetTriple::from_static(target).unwrap()
    }

    #[test]
    fn test_release_asset_candidate_generation_includes_version_patterns() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
        );

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.url.contains("/-/releases/v1.2.3/"))
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.url.contains("/-/releases/1.2.3/"))
        );
    }

    #[test]
    fn test_release_asset_candidate_generation_uses_gitlab_path() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
        );

        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.url.contains("/-/releases/"))
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.url.contains("/downloads/binaries/"))
        );
    }

    #[test]
    fn test_release_asset_candidate_generation_includes_archive_formats() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
        );

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.url.ends_with(".tar.gz"))
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.url.ends_with(".tar.xz"))
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.url.ends_with(".tar.zst"))
        );
        assert!(candidates.iter().any(|candidate| candidate.url.ends_with(".zip")));
    }

    #[test]
    fn test_release_asset_candidate_generation_carries_format() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
        );

        for candidate in &candidates {
            if candidate.url.ends_with(".tar.gz") || candidate.url.ends_with(".tgz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarGz);
            } else if candidate.url.ends_with(".tar.xz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarXz);
            } else if candidate.url.ends_with(".tar.zst") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarZst);
            } else if candidate.url.ends_with(".tar.bz2") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarBz2);
            } else if candidate.url.ends_with(".tar") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Tar);
            } else if candidate.url.ends_with(".zip") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Zip);
            } else if candidate.url.ends_with(".gz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Gz);
            } else if candidate.url.ends_with(".xz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Xz);
            } else if candidate.url.ends_with(".zst") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Zst);
            } else if candidate.url.ends_with(".bz2") {
                assert_eq!(candidate.archive_format, ArchiveFormat::Bz2);
            } else {
                assert_eq!(candidate.archive_format, ArchiveFormat::NakedBinary);
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
