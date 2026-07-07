use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use tempfile::TempDir;

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

    /// The compatible target whose platform token produced this candidate's filename.
    target: TargetTriple,
}

pub(in crate::bin_resolver) struct GitlabProvider {
    reporter: crate::messages::MessageReporter,
    staging_dir: PathBuf,
    verify_checksums: bool,
    http_client: HttpClient,
}

impl GitlabProvider {
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
                .join(<&'static str>::from(BinaryProvider::GitlabReleases)),
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
    /// Uses the same candidate filename generator in [`super::generate_candidate_filenames`], then
    /// combines each filename with each of the caller-supplied release `tags` to produce a list of
    /// [`GitlabReleaseAssetCandidate`]s that can be probed for existence.  It is assumed that the
    /// caller has already confirmed that the `tags` exist, so the cross-product of tags and
    /// filenames is limited at least to those tags that are known to exist.
    ///
    /// Tag-major ordering preserves candidate priority within each tag.
    fn generate_release_asset_candidates(
        repo_url: &str,
        crate_name: &str,
        extra_binary_names: &[&str],
        version: &str,
        target: &TargetTriple,
        tags: &[String],
    ) -> Vec<GitlabReleaseAssetCandidate> {
        // Generate candidate filenames for the crate and platform
        let filename_candidates =
            super::generate_candidate_filenames(crate_name, extra_binary_names, version, target);

        // Make a separate Gitlab release asset candidate for each release tag supplied by the
        // caller (which we assume the caller already verified exist).
        let mut asset_candidates = Vec::new();
        for tag in tags {
            let tag_segment = super::tag_url_path_segment(tag);
            for CandidateFilename {
                filename,
                binary_basename,
                format,
                target,
            } in &filename_candidates
            {
                asset_candidates.push(GitlabReleaseAssetCandidate {
                    url: format!(
                        "{}/-/releases/{}/downloads/binaries/{}",
                        repo_url, tag_segment, filename
                    ),
                    archive_format: *format,
                    asset_filename: filename.clone(),
                    binary_basename_hint: binary_basename.clone(),
                    target: target.clone(),
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

    /// HEAD-probe `{repo_url}/-/releases/{tag}` to check whether a release exists for `tag`.
    ///
    /// This is the cheap pre-filter that keeps the per-tag filename cross-product bounded: a tag
    /// whose release page 404s cannot have any `downloads/binaries/` assets, so it is skipped
    /// entirely. 404 → `Ok(false)`; any 2xx → `Ok(true)`; 429 → provider-throttled error. Any
    /// other cleanly delivered status → `Ok(true)`: fail open and keep the tag in play, so a
    /// GitLab instance whose release pages misbehave (auth walls, odd proxies) degrades to
    /// exhaustive filename probing rather than wrongly concluding
    /// [`ConclusiveResolution::Nonexistent`]. 5xx statuses are retried by the HTTP client and
    /// surface as `Err` after exhaustion, which callers propagate as an inconclusive resolution.
    ///
    /// Not implemented in terms of [`Self::head_probe`], which maps every non-2xx status to
    /// `false` — the wrong semantics here, where only a 404 may prune the tag.
    fn release_tag_exists(&self, repo_url: &str, tag: &str) -> Result<bool> {
        let url = format!("{}/-/releases/{}", repo_url, super::tag_url_path_segment(tag));
        let response = self
            .http_client
            .head_retrying_status(&url, Self::should_retry_status)?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Self::provider_throttled_error(&url, status));
        }
        Ok(status != StatusCode::NOT_FOUND)
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
        let version = krate.resolved.version.to_string();

        // Pre-filter the candidate tag forms by probing each tag's release page, so the much
        // larger filename cross-product below only runs for tags that actually have a release.
        // The common no-release case stops here after at most one cheap HEAD per tag form.
        let mut live_tags = Vec::new();
        for tag in super::generate_candidate_tags(crate_name, &version) {
            if self.release_tag_exists(&repo_url, &tag)? {
                live_tags.push(tag);
            }
        }
        if live_tags.is_empty() {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
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
        let candidates = Self::generate_release_asset_candidates(
            &repo_url,
            crate_name,
            &extra_binary_names,
            &version,
            target,
            &live_tags,
        );

        // Probe sequentially with HEAD requests
        //
        // If we hit a connection/timeout error, bail immediately rather than continuing to probe
        // every candidate URL against a broken/unresponsive server. The candidate set is the
        // cross-product of names (crate + binary), platform aliases, and archive formats — on the
        // order of ~1,000 sequential HEADs per surviving tag when no asset exists (a slow
        // fallback to a source build). The release-page pre-filter above bounds that: a repo with
        // no release for any tag form does at most one HEAD per tag form, and repos rarely have
        // releases for more than one form. `get_repo_url` short-circuits to `None` for every
        // non-GitLab crate, which is the overwhelming majority and does zero probes.
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
                    "no matching asset found in release",
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

        let work_dir = super::recreate_staging_work_dir(&self.staging_dir, &krate.resolved)?;

        let archive_path = work_dir.join(candidate.archive_format.canonical_filename());
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
            let checksum_path = work_dir.join("checksum.sha256");
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

        let extract_dir = work_dir.join("extracted");
        let binary_path = super::extract_binary_by_candidate_names(
            &archive_path,
            candidate.archive_format,
            &expected_binary_names,
            &extract_dir,
        )
        .map_err(|source| {
            super::provider_asset_preparation_failed(BinaryProvider::GitlabReleases, &candidate.url, source)
        })?;

        let staged_path = super::stage_extracted_binary(&work_dir, &binary_name, target, &binary_path)?;
        Ok(ConclusiveResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::GitlabReleases,
            path: staged_path,
            target: candidate.target.to_string(),
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
        messages::MessageReporter, testdata::target_triple,
    };

    fn fast_retry_config() -> HttpConfig {
        HttpConfig {
            retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(10),
            ..Default::default()
        }
    }

    fn test_provider() -> (GitlabProvider, TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let http_client = HttpClient::new(&fast_retry_config()).unwrap();
        (
            GitlabProvider::new(MessageReporter::null(), &temp_dir, false, http_client),
            temp_dir,
        )
    }

    /// The two historical tag forms, for candidate-generation tests that don't care about the
    /// monorepo forms.
    fn v_and_bare_tags(version: &str) -> Vec<String> {
        vec![format!("v{version}"), version.to_string()]
    }

    #[test]
    fn test_release_asset_candidate_generation_includes_version_patterns() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
            &v_and_bare_tags("1.2.3"),
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
            &v_and_bare_tags("1.2.3"),
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
            &v_and_bare_tags("1.2.3"),
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
            &v_and_bare_tags("1.2.3"),
        );

        for candidate in &candidates {
            if candidate.url.ends_with(".tar.gz") || candidate.url.ends_with(".tgz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarGz);
            } else if candidate.url.ends_with(".tar.xz") || candidate.url.ends_with(".txz") {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarXz);
            } else if candidate.url.ends_with(".tar.zst")
                || candidate.url.ends_with(".tzst")
                || candidate.url.ends_with(".tzstd")
            {
                assert_eq!(candidate.archive_format, ArchiveFormat::TarZst);
            } else if candidate.url.ends_with(".tar.bz2")
                || candidate.url.ends_with(".tbz2")
                || candidate.url.ends_with(".tbz")
                || candidate.url.ends_with(".tar.bz")
            {
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

    /// Slash-form subcrate tags must appear percent-encoded in every generated download URL, so
    /// the tag stays a single path segment.
    #[test]
    fn test_release_asset_candidates_use_encoded_slash_tags() {
        let candidates = GitlabProvider::generate_release_asset_candidates(
            "https://gitlab.com/owner/repo",
            "mytool",
            &[],
            "1.2.3",
            &target_triple("x86_64-unknown-linux-gnu"),
            &["mytool/v1.2.3".to_string()],
        );

        assert!(!candidates.is_empty());
        for candidate in &candidates {
            assert!(
                candidate
                    .url
                    .contains("/-/releases/mytool%2Fv1.2.3/downloads/binaries/"),
                "expected encoded tag in URL, got: {}",
                candidate.url
            );
        }
    }

    #[test]
    fn release_tag_exists_200_returns_true() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/owner/repo/-/releases/v1.0.0");
            then.status(200);
        });
        let (provider, _temp_dir) = test_provider();

        let exists = provider
            .release_tag_exists(&server.url("/owner/repo"), "v1.0.0")
            .unwrap();

        assert!(exists);
        mock.assert_calls(1);
    }

    #[test]
    fn release_tag_exists_404_returns_false() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/owner/repo/-/releases/v1.0.0");
            then.status(404);
        });
        let (provider, _temp_dir) = test_provider();

        let exists = provider
            .release_tag_exists(&server.url("/owner/repo"), "v1.0.0")
            .unwrap();

        assert!(!exists);
        mock.assert_calls(1);
    }

    /// A status that is neither 2xx, 404, nor 429 (eg an auth wall's 403) must keep the tag in
    /// play rather than prune it: only a conclusive 404 may skip a tag's filename probing.
    #[test]
    fn release_tag_exists_unexpected_status_fails_open() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/owner/repo/-/releases/v1.0.0");
            then.status(403);
        });
        let (provider, _temp_dir) = test_provider();

        let exists = provider
            .release_tag_exists(&server.url("/owner/repo"), "v1.0.0")
            .unwrap();

        assert!(exists);
        mock.assert_calls(1);
    }

    #[test]
    fn release_tag_exists_429_becomes_provider_throttled() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/owner/repo/-/releases/v1.0.0");
            then.status(429).header("retry-after", "30");
        });
        let (provider, _temp_dir) = test_provider();

        let result = provider.release_tag_exists(&server.url("/owner/repo"), "v1.0.0");

        assert_matches!(result, Err(Error::ProviderThrottled { status: 429, .. }));
        mock.assert_calls(1);
    }

    #[test]
    fn release_tag_exists_encodes_slash_tags() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(HEAD).path("/owner/repo/-/releases/mytool%2Fv1.0.0");
            then.status(200);
        });
        let (provider, _temp_dir) = test_provider();

        let exists = provider
            .release_tag_exists(&server.url("/owner/repo"), "mytool/v1.0.0")
            .unwrap();

        assert!(exists);
        mock.assert_calls(1);
    }

    /// The release-page pre-probe is what keeps the filename cross-product bounded: when no tag
    /// form has a release, resolution must conclude Nonexistent after exactly one HEAD per tag
    /// form, without a single `downloads/binaries` probe.
    #[test]
    fn try_resolve_prunes_filename_probing_to_surviving_tags() {
        let server = MockServer::start();
        let tag_segments = [
            "v1.0.0",
            "1.0.0",
            "mytool-v1.0.0",
            "mytool-1.0.0",
            "mytool%2Fv1.0.0",
            "mytool%2F1.0.0",
        ];
        let tag_mocks: Vec<_> = tag_segments
            .iter()
            .map(|segment| {
                let path = format!("/owner/repo/-/releases/{}", segment);
                server.mock(move |when, then| {
                    when.method(HEAD).path(path);
                    then.status(404);
                })
            })
            .collect();
        let asset_probes = server.mock(|when, then| {
            when.method(HEAD).path_includes("/downloads/binaries/");
            then.status(404);
        });
        let (provider, _temp_dir) = test_provider();
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitLab {
                        custom_url: Some(Url::parse(&server.base_url()).unwrap()),
                        owner: "owner".to_string(),
                        repo: "repo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let result = provider.try_resolve(&krate, &target_triple("x86_64-unknown-linux-gnu"));

        assert_matches!(result, Ok(ConclusiveResolution::Nonexistent));
        for mock in &tag_mocks {
            mock.assert_calls(1);
        }
        asset_probes.assert_calls(0);
    }
}
