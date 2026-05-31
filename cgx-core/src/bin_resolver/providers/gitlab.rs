use super::{ArchiveFormat, CandidateFilename, Provider};
use crate::{
    Result,
    bin_resolver::ResolvedBinary,
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    cratespec::Forge,
    downloader::DownloadedCrate,
    error,
    http::{Bytes, HttpClient},
    messages::PrebuiltBinaryMessage,
};
use sha2::{Digest, Sha256};
use snafu::ResultExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

    /// Get the repository URL for a crate, filtering for GitLab hosts.
    ///
    /// If the crate came from a GitLab forge, the forge URL is used directly (handles the fork
    /// scenario where Cargo.toml may still point to the upstream). For all other sources
    /// (including non-GitLab forges), falls back to the `[package].repository` field in
    /// Cargo.toml, filtered to GitLab hosts only.
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
        name: &str,
        version: &str,
        platform: &str,
    ) -> Vec<(String, ArchiveFormat)> {
        let candidates = super::generate_candidate_filenames(name, version, platform);
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

    /// Probe a URL with a HEAD request to check if the asset exists.
    ///
    /// Returns `Ok(true)` if the asset exists (200 response), `Ok(false)` if it doesn't
    /// (404 or other non-success), or `Err` if a connection/timeout error occurred.
    /// The error case is used by the caller to bail early when the server is unreachable.
    fn head_probe(&self, url: &str) -> Result<bool> {
        let response = self.http_client.head(url)?;
        Ok(response.status().is_success())
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

impl Provider for GitlabProvider {
    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<Option<ResolvedBinary>> {
        let repo_url = if let Some(url) = Self::get_repo_url(krate)? {
            url
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    "no repository URL available",
                )
            });
            return Ok(None);
        };

        let urls = Self::generate_urls(
            &repo_url,
            &krate.resolved.name,
            &krate.resolved.version.to_string(),
            platform,
        );

        // Probe sequentially with HEAD requests; stop at the first 200.
        // If we hit a connection/timeout error, bail immediately rather than continuing
        // to probe all ~160 candidate URLs against a dead server.
        let mut found = None;
        for (url, format) in &urls {
            match self.head_probe(url) {
                Ok(true) => {
                    found = Some((url.clone(), *format));
                    break;
                }
                Err(e) if HttpClient::is_connection_error(&e) => {
                    tracing::debug!("GitLab HEAD probe failed with connection error, bailing: {:?}", e);
                    self.reporter.report(|| {
                        PrebuiltBinaryMessage::provider_has_no_binary(
                            BinaryProvider::GitlabReleases,
                            "server unreachable",
                        )
                    });
                    return Ok(None);
                }
                Ok(false) | Err(_) => continue,
            }
        }
        let Some((url, format)) = found else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    "no matching release found",
                )
            });
            return Ok(None);
        };

        self.reporter
            .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::GitlabReleases));

        let data = if let Some(data) = self.try_download(&url)? {
            data
        } else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::GitlabReleases,
                    format!("failed to download asset: {}", url),
                )
            });
            return Ok(None);
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

        Ok(Some(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::GitlabReleases,
            path: final_path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crate_resolver::ResolvedSource, cratespec::Forge};
    use semver::Version;
    use std::fs;
    use url::Url;

    #[test]
    fn test_url_generation_includes_version_patterns() {
        let urls = GitlabProvider::generate_urls(
            "https://gitlab.com/owner/repo",
            "mytool",
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
}
