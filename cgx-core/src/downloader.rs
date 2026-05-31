use std::path::{Path, PathBuf};

use semver::Version;
use snafu::ResultExt;
use toml::Value;

use crate::{
    Result,
    cache::Cache,
    config::Config,
    crate_resolver::{ResolvedCrate, ResolvedSource},
    cratespec::RegistrySource,
    error,
    git::{GitClient, GitSelector},
    http::HttpClient,
    registry::{DownloadUrlLookup, RegistryClient},
};

/// A crate whose code is available locally on disk after downloading.
///
/// This nomenclature is perhaps a bit misleading, since it's possible for the user to specify a
/// [`crate::cratespec::CrateSpec::LocalDir`] crate spec to the resolver, which will resolve
/// directly to that local dir without any downloading or caching.  However,
/// `DownlaodedOrPossiblyAlreadyLocalCrate` isn't very catchy, so you'll have to do that
/// substitution in your head.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DownloadedCrate {
    /// The resolved crate metadata (name, version, source)
    pub resolved: ResolvedCrate,

    /// The path to the crate source code on disk.
    ///
    /// This may be a path into the crate cache, but if a local crate was specified then this is
    /// the direct path to that local crate without any cache layer.
    pub crate_path: PathBuf,
}

impl DownloadedCrate {
    /// Path to this crate's Cargo.toml.
    pub fn cargo_toml_path(&self) -> PathBuf {
        self.crate_path.join("Cargo.toml")
    }

    /// Read and parse the crate's Cargo.toml as a raw TOML [`Value`].
    ///
    /// Use this for accessing non-standard fields like `[package.metadata.binstall]`.
    /// For common fields, prefer the dedicated accessor methods.
    pub fn parsed_cargo_toml(&self) -> Result<Value> {
        let path = self.cargo_toml_path();
        let content =
            std::fs::read_to_string(&path).with_context(|_| error::IoSnafu { path: path.clone() })?;
        toml::from_str(&content).with_context(|_| error::CargoTomlParseSnafu { path })
    }

    /// Extract the `[package].repository` URL from Cargo.toml.
    ///
    /// Returns [`None`] if the field is absent.
    /// Fails if the Cargo.toml cannot be read or parsed.
    pub fn repository_url(&self) -> Result<Option<String>> {
        let doc = self.parsed_cargo_toml()?;
        Ok(doc
            .get("package")
            .and_then(|p| p.get("repository"))
            .and_then(|r| r.as_str())
            .map(|s| s.trim_end_matches('/').trim_end_matches(".git").to_string()))
    }

    /// List binary target names declared in Cargo.toml.
    ///
    /// Reads explicit `[[bin]]` entries. If none are declared, returns a
    /// single-element vec containing the package name (Cargo's default when
    /// `src/main.rs` exists, which is the common case for crates that
    /// distribute pre-built binaries).
    pub fn binary_names(&self) -> Result<Vec<String>> {
        let doc = self.parsed_cargo_toml()?;
        let pkg_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(&self.resolved.name);

        if let Some(bins) = doc.get("bin").and_then(|b| b.as_array()) {
            let names: Vec<String> = bins
                .iter()
                .filter_map(|b| b.get("name").and_then(|n| n.as_str()))
                .map(String::from)
                .collect();
            if !names.is_empty() {
                return Ok(names);
            }
        }

        Ok(vec![pkg_name.to_string()])
    }

    /// Determine the default binary name for this crate.
    ///
    /// Resolution order:
    /// 1. `package.default-run` if set
    /// 2. Single `[[bin]]` entry if there's exactly one
    /// 3. Package name (Cargo's implicit default)
    pub fn default_binary_name(&self) -> Result<String> {
        let doc = self.parsed_cargo_toml()?;
        let pkg = doc.get("package");
        let pkg_name = pkg
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(&self.resolved.name);

        if let Some(default_run) = pkg.and_then(|p| p.get("default-run")).and_then(|d| d.as_str()) {
            return Ok(default_run.to_string());
        }

        if let Some(bins) = doc.get("bin").and_then(|b| b.as_array()) {
            let names: Vec<&str> = bins
                .iter()
                .filter_map(|b| b.get("name").and_then(|n| n.as_str()))
                .collect();
            if names.len() == 1 {
                return Ok(names[0].to_string());
            }
        }

        Ok(pkg_name.to_string())
    }
}

/// Abstract interface for downloading a (validated) [`ResolvedCrate`] and returning
/// the filesystem path where its source code is located.
///
/// The trait abstraction allows for thorough testing and alternative implementations
/// (e.g., mock downloaders for testing).
pub trait CrateDownloader: std::fmt::Debug + Send + Sync + 'static {
    /// Download a resolved crate and return a descriptor with which the crate code can be
    /// accessed.
    ///
    /// This involves:
    /// - Checking if the source is already cached
    /// - Downloading from registries, git repositories, or forges as needed
    /// - Extracting and caching the source code
    /// - Honoring offline mode (returning cached entries only)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The download fails
    /// - Extraction fails
    /// - Offline mode is enabled and the crate is not cached
    fn download(&self, krate: ResolvedCrate) -> Result<DownloadedCrate>;
}

/// Create a default implementation of [`CrateDownloader`] using the given cache, config, and git
/// client.
pub(crate) fn create_downloader(
    config: Config,
    cache: Cache,
    git_client: GitClient,
    http_client: HttpClient,
) -> impl CrateDownloader {
    DefaultCrateDownloader::new(cache, config, git_client, http_client)
}

/// Default implementation of [`CrateDownloader`] that performs actual network requests
/// and file system operations to download crate source code.
#[derive(Debug, Clone)]
struct DefaultCrateDownloader {
    cache: Cache,
    config: Config,
    git_client: GitClient,
    http_client: HttpClient,
}

impl DefaultCrateDownloader {
    /// Create a new [`DefaultCrateDownloader`] with the given cache, configuration, and git client.
    pub(crate) fn new(cache: Cache, config: Config, git_client: GitClient, http_client: HttpClient) -> Self {
        Self {
            cache,
            config,
            git_client,
            http_client,
        }
    }

    /// Download a crate from a registry (crates.io or custom) to the specified path.
    fn download_registry(
        &self,
        download_path: &Path,
        name: &str,
        version: &Version,
        source: Option<&RegistrySource>,
    ) -> Result<()> {
        let registry = RegistryClient::new(source, &self.http_client, &self.config.http)?;
        let download_url = match registry.crate_download_url(name, version, self.config.offline)? {
            DownloadUrlLookup::Url(download_url) => download_url,
            DownloadUrlLookup::CrateNotFound => {
                return error::CrateNotFoundInRegistrySnafu {
                    name: name.to_string(),
                }
                .fail();
            }
            DownloadUrlLookup::VersionNotFound => {
                return error::NoMatchingVersionSnafu {
                    name: name.to_string(),
                    requirement: version.to_string(),
                }
                .fail();
            }
            DownloadUrlLookup::UrlUnavailable => {
                return error::DownloadUrlUnavailableSnafu {
                    name: name.to_string(),
                    version: version.to_string(),
                }
                .fail();
            }
        };

        // Download the .crate file
        let response = self.http_client.get(&download_url)?;

        // The .crate file is a gzipped tarball, extract it to download_path
        //
        // Crates.io tarballs have all files nested under a top-level directory named
        // "{name}-{version}/" (e.g., "serde-1.0.200/Cargo.toml"). We need to strip this
        // prefix during extraction so files end up directly in download_path rather than
        // in a subdirectory. This is equivalent to `tar --strip-components=1`.
        let tar_gz = flate2::read::GzDecoder::new(response);
        let mut archive = tar::Archive::new(tar_gz);

        for entry in archive.entries().context(error::TarExtractionSnafu)? {
            let mut entry = entry.context(error::TarExtractionSnafu)?;
            let path = entry.path().context(error::TarExtractionSnafu)?;

            // Strip the first path component (the "{name}-{version}" directory)
            let stripped_path: PathBuf = path.components().skip(1).collect();

            // Skip if there's nothing left after stripping (shouldn't happen, but be safe)
            if stripped_path.as_os_str().is_empty() {
                continue;
            }

            let dest_path = download_path.join(stripped_path);

            // Ensure parent directory exists before unpacking
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
                    path: parent.to_path_buf(),
                })?;
            }

            entry.unpack(&dest_path).context(error::TarExtractionSnafu)?;
        }

        Ok(())
    }

    fn download_git(&self, krate: &ResolvedCrate, repo_url: &str, commit: String) -> Result<PathBuf> {
        // Git sources use the git-specific two-tier cache (db + checkout)
        // The checkout path IS the final source code, no need for duplication
        self.git_client
            .checkout_ref(repo_url, GitSelector::Commit(commit))
            .map(|(path, _commit_hash)| path) // Discard commit hash, downloader only needs path
            .map_err(|e| {
                // If we're offline and the checkout isn't cached, return OfflineMode error
                if self.config.offline {
                    error::OfflineModeSnafu {
                        name: krate.name.clone(),
                        version: krate.version.to_string(),
                    }
                    .build()
                } else {
                    e.into()
                }
            })
    }
}

impl CrateDownloader for DefaultCrateDownloader {
    fn download(&self, krate: ResolvedCrate) -> Result<DownloadedCrate> {
        let source = krate.source.clone();
        match source {
            ResolvedSource::LocalDir { path } => {
                // Local directories don't need caching or downloading
                Ok(DownloadedCrate {
                    resolved: krate,
                    crate_path: path,
                })
            }

            ResolvedSource::Git { repo, commit } => {
                let cached_krate_path = self.download_git(&krate, &repo, commit)?;

                Ok(DownloadedCrate {
                    resolved: krate,
                    crate_path: cached_krate_path,
                })
            }

            ResolvedSource::Forge { forge, commit } => {
                // Forge sources also use git
                let repo_url = forge.git_url();
                let cached_krate_path = self.download_git(&krate, &repo_url, commit)?;

                Ok(DownloadedCrate {
                    resolved: krate,
                    crate_path: cached_krate_path,
                })
            }

            ResolvedSource::CratesIo { .. } | ResolvedSource::Registry { .. } => {
                // For registry sources, use the cache which handles checking for existing
                // cached copies and atomically downloading if not present
                let cached_krate_path = self
                    .cache
                    .get_or_download_crate(&krate, |download_path| {
                        // The cache check happens before this closure is called, so if we're here
                        // it means we need to actually download the crate.
                        //
                        // Check offline mode AFTER the cache check, so cached entries work offline
                        if self.config.offline {
                            return error::OfflineModeSnafu {
                                name: krate.name.clone(),
                                version: krate.version.to_string(),
                            }
                            .fail();
                        }

                        // Perform the actual download based on source type
                        match source {
                            ResolvedSource::CratesIo => {
                                self.download_registry(download_path, &krate.name, &krate.version, None)
                            }
                            ResolvedSource::Registry {
                                source: registry_source,
                            } => self.download_registry(
                                download_path,
                                &krate.name,
                                &krate.version,
                                Some(&registry_source),
                            ),
                            _ => unreachable!("Git, Forge, and LocalDir handled above"),
                        }
                    })
                    .map(|cached| cached.crate_path)?;

                Ok(DownloadedCrate {
                    resolved: krate,
                    crate_path: cached_krate_path,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;
    use crate::{Config, cargo::CargoRunner};

    /// Create a test downloader with online config and an isolated temp directory.
    ///
    /// Returns the downloader and the `TempDir` which must be kept alive for the test duration.
    fn test_downloader() -> (DefaultCrateDownloader, tempfile::TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, config) = crate::config::create_test_env();
        let reporter = crate::messages::MessageReporter::null();
        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = GitClient::new(cache.clone(), reporter, config.http.clone());
        let http_client = HttpClient::new(&config.http).unwrap();
        (
            DefaultCrateDownloader::new(cache, config, git_client, http_client),
            temp_dir,
        )
    }

    /// Create a test downloader with offline config and an isolated temp directory.
    fn test_downloader_offline() -> (DefaultCrateDownloader, tempfile::TempDir) {
        let (downloader, temp_dir) = test_downloader();
        let mut config = downloader.config;
        config.offline = true;
        let reporter = crate::messages::MessageReporter::null();
        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = GitClient::new(cache.clone(), reporter, config.http.clone());
        let http_client = HttpClient::new(&config.http).unwrap();
        (
            DefaultCrateDownloader::new(cache, config, git_client, http_client),
            temp_dir,
        )
    }

    fn test_cargo_runner() -> impl CargoRunner {
        crate::logging::init_test_logging();

        crate::cargo::find_cargo(crate::messages::MessageReporter::null()).unwrap()
    }

    fn validate_downloaded_crate(downloaded: &DownloadedCrate) {
        // Basic sanity checks on the downloaded crate
        assert!(
            downloaded.crate_path.exists(),
            "Downloaded crate path does not exist"
        );
        assert!(
            downloaded.crate_path.join("Cargo.toml").exists(),
            "Downloaded crate missing Cargo.toml"
        );

        // Make sure we can query metadata on it
        let cargo_runner = test_cargo_runner();
        let metadata = cargo_runner
            .metadata(
                &downloaded.crate_path,
                &crate::cargo::CargoMetadataOptions::default(),
            )
            .unwrap();

        // Most of the validation is the fact that cargo metadata was successful.
        // Just do a few basic checks on the metadata itself to make sure it matches the crate we
        // downloaded
        assert!(
            metadata
                .packages
                .iter()
                .any(|p| p.name.as_str() == downloaded.resolved.name
                    && p.version == downloaded.resolved.version),
            "Downloaded crate metadata does not match expected name/version"
        );
    }

    mod local_dir {
        use super::*;

        /// When the resolved crate is on a local path, there isn't actually any downloading or
        /// caching needed since it's already local.
        #[test]
        fn returns_path_directly() {
            let (downloader, _temp_dir) = test_downloader();

            let local_path = PathBuf::from("/some/local/path");
            let resolved = ResolvedCrate {
                name: "test-crate".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::LocalDir {
                    path: local_path.clone(),
                },
            };

            let downloaded_crate = downloader.download(resolved).unwrap();
            assert_eq!(downloaded_crate.crate_path, local_path);
        }
    }

    mod registry {
        use super::*;

        #[test]
        fn downloads_serde_and_extracts() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("1.0.200").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            let downloaded_crate = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate);
        }

        #[test]
        fn cache_hit_skips_redownload() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("1.0.201").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            // First download
            let path1 = downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&path1);

            // Second download - should hit cache
            let path2 = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&path2);

            // Should be the exact same path
            assert_eq!(path1, path2, "Cached download should return same path");
        }

        #[test]
        fn offline_mode_with_cached_works() {
            let (online_downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("1.0.202").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            let online_result = online_downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&online_result);

            // Now try offline mode - should work because it's cached
            let offline_config = Config {
                offline: true,
                ..online_downloader.config
            };
            let reporter = crate::messages::MessageReporter::null();
            let cache = Cache::new(offline_config.clone(), reporter.clone());
            let git_client = GitClient::new(cache.clone(), reporter, offline_config.http.clone());
            let http_client = HttpClient::new(&offline_config.http).unwrap();
            let offline_downloader =
                DefaultCrateDownloader::new(cache, offline_config, git_client, http_client);

            let offline_result = offline_downloader.download(resolved).unwrap();
            validate_downloaded_crate(&offline_result);
        }

        #[test]
        fn offline_mode_without_cached_fails() {
            let (downloader, _temp_dir) = test_downloader_offline();

            // Use an obscure version that's unlikely to be cached
            let resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("1.0.203").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            let result = downloader.download(resolved);
            assert_matches!(result.unwrap_err(), error::Error::OfflineMode { .. });
        }
    }

    mod git {
        use super::*;

        #[test]
        fn downloads_rustlings_and_extracts() {
            let (downloader, _temp_dir) = test_downloader();

            // Use a specific commit from rustlings history
            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                    commit: "28d2bb0".to_string(), // Short hash for v6.0.0 tag
                },
            };

            let downloaded_crate = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate);
        }

        #[test]
        fn excludes_git_directory() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                    commit: "28d2bb0".to_string(),
                },
            };

            let downloaded_crate = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate);

            // .git directory should not be in the cached result
            assert!(
                !downloaded_crate.crate_path.join(".git").exists(),
                ".git directory should be excluded from cache"
            );
        }

        #[test]
        fn cache_hit_skips_reclone() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                    commit: "28d2bb0".to_string(),
                },
            };

            // First clone
            let downloaded_crate1 = downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&downloaded_crate1);

            // Second clone - should hit cache
            let downloaded_crate2 = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate2);

            assert_eq!(
                downloaded_crate1, downloaded_crate2,
                "Cached clone should return same result"
            );
        }

        #[test]
        fn offline_mode_with_cached_works() {
            let (online_downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                    commit: "28d2bb0".to_string(),
                },
            };

            let online_downloaded_crate = online_downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&online_downloaded_crate);

            // Now try offline mode - should work because it's cached
            let offline_config = Config {
                offline: true,
                ..online_downloader.config
            };
            let reporter = crate::messages::MessageReporter::null();
            let cache = Cache::new(offline_config.clone(), reporter.clone());
            let git_client = GitClient::new(cache.clone(), reporter, offline_config.http.clone());
            let http_client = HttpClient::new(&offline_config.http).unwrap();
            let offline_downloader =
                DefaultCrateDownloader::new(cache, offline_config, git_client, http_client);

            let offline_downloaded_crate = offline_downloader.download(resolved).unwrap();
            validate_downloaded_crate(&offline_downloaded_crate);

            assert_eq!(online_downloaded_crate, offline_downloaded_crate);
        }

        #[test]
        fn offline_mode_without_cached_fails() {
            let (downloader, _temp_dir) = test_downloader_offline();

            // Use a different commit that's unlikely to be cached
            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                    commit: "abcdef123456".to_string(),
                },
            };

            let result = downloader.download(resolved);
            assert!(matches!(result, Err(error::Error::OfflineMode { .. })),);
        }
    }

    mod forge {
        use super::*;
        use crate::cratespec::Forge;

        #[test]
        fn downloads_github_rustlings() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "rust-lang".to_string(),
                        repo: "rustlings".to_string(),
                    },
                    commit: "28d2bb0".to_string(),
                },
            };

            let downloaded_crate = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate);
        }

        #[test]
        fn cache_hit_skips_redownload() {
            let (downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "rust-lang".to_string(),
                        repo: "rustlings".to_string(),
                    },
                    commit: "28d2bb0".to_string(),
                },
            };

            // First download
            let downloaded_crate1 = downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&downloaded_crate1);

            // Second download - should hit cache
            let downloaded_crate2 = downloader.download(resolved).unwrap();
            validate_downloaded_crate(&downloaded_crate2);

            assert_eq!(
                downloaded_crate1, downloaded_crate2,
                "Cached forge download should return same path"
            );
        }

        #[test]
        fn offline_mode_with_cached_works() {
            let (online_downloader, _temp_dir) = test_downloader();

            let resolved = ResolvedCrate {
                name: "rustlings".to_string(),
                version: Version::parse("6.0.0").unwrap(),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "rust-lang".to_string(),
                        repo: "rustlings".to_string(),
                    },
                    commit: "28d2bb0".to_string(),
                },
            };

            let online_result = online_downloader.download(resolved.clone()).unwrap();
            validate_downloaded_crate(&online_result);

            // Now try offline mode - should work because it's cached
            let offline_config = Config {
                offline: true,
                ..online_downloader.config
            };
            let reporter = crate::messages::MessageReporter::null();
            let cache = Cache::new(offline_config.clone(), reporter.clone());
            let git_client = GitClient::new(cache.clone(), reporter, offline_config.http.clone());
            let http_client = HttpClient::new(&offline_config.http).unwrap();
            let offline_downloader =
                DefaultCrateDownloader::new(cache, offline_config, git_client, http_client);

            let offline_result = offline_downloader.download(resolved).unwrap();
            validate_downloaded_crate(&offline_result);
        }
    }
}
