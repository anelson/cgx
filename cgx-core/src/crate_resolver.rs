use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use snafu::OptionExt;

use crate::{
    Result,
    cache::Cache,
    cargo::{CargoMetadataOptions, CargoRunner},
    config::Config,
    cratespec::{CrateSpec, Forge, RegistrySource},
    error,
    git::{GitClient, GitSelector},
    http::HttpClient,
    registry::RegistryClient,
};

/// A resolved crate represents a concrete, validated reference to a specific crate version.
///
/// Unlike [`CrateSpec`], which may contain ambiguous information
/// (like version requirements or missing crate names), a [`ResolvedCrate`] always contains:
/// - An exact crate name
/// - An exact version (not a version requirement)
/// - A validated source location that is known to exist at the time of resolution
///
/// This type is the result of resolving a [`CrateSpec`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedCrate {
    /// The exact name of the crate
    pub name: String,

    /// The exact version of the crate
    pub version: Version,

    /// The source location where this crate was found
    pub source: ResolvedSource,
}

/// Abstract interface for resolving a (potentially ambiguous, potentially invalid) [`CrateSpec`]
/// to a concrete, validated [`ResolvedCrate`].
///
/// The trait abstraction is important to allow thorough testing of the many edge cases and failure
/// modes involved.
pub trait CrateResolver: std::fmt::Debug + Send + Sync + 'static {
    /// Resolve a (potentially ambiguous, potentially invalid) [`CrateSpec`] to a concrete,
    /// validated [`ResolvedCrate`].
    ///
    /// This involves:
    /// - Validating the crate specification
    /// - Querying remote registries or repositories as needed
    /// - Ensuring that the specified version (if any) is compatible with the found version
    ///
    /// # Errors
    ///
    /// Returns an error if the crate specification is invalid, if the crate cannot be found,
    /// or if the specified version is not compatible with the found version.
    fn resolve(&self, spec: &CrateSpec) -> Result<ResolvedCrate>;
}

/// The source location of a resolved crate.
///
/// Unlike [`CrateSpec`] variants, which may contain ambiguous
/// selectors (like branch names or tags), [`ResolvedSource`] variants contain only concrete,
/// immutable references (like commit hashes).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolvedSource {
    /// A crate from Crates.io
    CratesIo,

    /// A crate from another registry
    Registry {
        /// The registry source (named registry or index URL)
        source: RegistrySource,
    },

    /// A crate from a git repository
    Git {
        /// The repository URL
        repo: String,

        /// The exact commit hash (not a branch, tag, or other selector)
        commit: String,
    },

    /// A crate from a software forge (GitHub, GitLab, etc.)
    Forge {
        /// The forge where the crate is hosted
        forge: Forge,

        /// The exact commit hash (not a branch, tag, or other selector)
        commit: String,
    },

    /// A crate from a local directory
    LocalDir {
        /// The path to the directory containing the crate
        path: PathBuf,
    },
}

/// Create the default [`CrateResolver`] implementation, repecting the given config and using the
/// provided cache.
pub(crate) fn create_resolver(
    config: Config,
    cache: Cache,
    git_client: GitClient,
    cargo: Arc<dyn CargoRunner>,
    http_client: HttpClient,
) -> impl CrateResolver {
    let inner = DefaultCrateResolver::new(config, git_client, cargo, http_client);
    CachingResolver::new(inner, cache)
}

/// Default implementation of [`CrateResolver`] that performs actual network requests
/// and file system operations to resolve crate specifications.
#[derive(Debug, Clone)]
struct DefaultCrateResolver {
    config: Config,
    git_client: GitClient,
    cargo: Arc<dyn CargoRunner>,
    http_client: HttpClient,
}

impl DefaultCrateResolver {
    /// Create a new [`DefaultCrateResolver`] with the given configuration and git client.
    pub(crate) fn new(
        config: Config,
        git_client: GitClient,
        cargo: Arc<dyn CargoRunner>,
        http_client: HttpClient,
    ) -> Self {
        Self {
            config,
            git_client,
            cargo,
            http_client,
        }
    }

    /// Resolve a local directory crate specification.
    fn resolve_local_dir(
        &self,
        path: &Path,
        name: &Option<String>,
        version: &Option<VersionReq>,
    ) -> Result<ResolvedCrate> {
        let metadata = self.cargo.metadata(
            path,
            &CargoMetadataOptions {
                no_deps: true,
                ..Default::default()
            },
        )?;

        let package = if let Some(n) = name {
            metadata
                .packages
                .iter()
                .find(|p| p.name.as_str() == n)
                .with_context(|| error::PackageNotFoundInWorkspaceSnafu {
                    name: n.clone(),
                    available: metadata
                        .packages
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect::<Vec<_>>(),
                })?
        } else {
            if metadata.packages.len() != 1 {
                return error::AmbiguousPackageNameSnafu {
                    count: metadata.packages.len(),
                }
                .fail();
            }
            &metadata.packages[0]
        };

        if let Some(req) = version {
            if !req.matches(&package.version) {
                return error::VersionMismatchSnafu {
                    requirement: req.to_string(),
                    found: package.version.clone(),
                }
                .fail();
            }
        }

        Ok(ResolvedCrate {
            name: package.name.to_string(),
            version: package.version.clone(),
            source: ResolvedSource::LocalDir {
                path: path.to_path_buf(),
            },
        })
    }

    /// Resolve a registry crate specification.
    ///
    /// `source` is `None` to indicate the default (crates.io) registry.
    fn resolve_registry(
        &self,
        name: &str,
        version: Option<&VersionReq>,
        source: Option<&RegistrySource>,
    ) -> Result<ResolvedCrate> {
        // There is always some VersionReq; if not specified explicitly then "*" is implied
        let version = version.cloned().unwrap_or(VersionReq::STAR);
        let registry = RegistryClient::new(source, &self.http_client, &self.config.http)?;
        let versions = match registry.crate_versions(name, self.config.offline)? {
            Some(versions) => versions,
            None if self.config.offline => {
                return error::OfflineModeSnafu {
                    name: name.to_string(),
                    version: version.to_string(),
                }
                .fail();
            }
            None => {
                return error::CrateNotFoundInRegistrySnafu {
                    name: name.to_string(),
                }
                .fail();
            }
        };

        // Filter non-yanked versions matching the requirement and select the best, by which
        // we mean the highest version number.
        let best_version = versions
            .iter()
            .filter(|v| !v.yanked)
            .filter_map(|v| {
                Version::parse(&v.version)
                    .ok()
                    .filter(|ver| version.matches(ver))
                    .map(|ver| (v.version.clone(), ver))
            })
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(_, best)| best)
            .with_context(|| error::NoMatchingVersionSnafu {
                name: name.to_string(),
                requirement: version.to_string(),
            })?;

        // Record the resolved source which we store alongside the crate, as we will still need
        // to retrieve the crate contents at some point later.
        let resolved_source = match source {
            None => ResolvedSource::CratesIo,
            Some(custom_registry) => ResolvedSource::Registry {
                source: custom_registry.clone(),
            },
        };

        Ok(ResolvedCrate {
            name: name.to_string(),
            version: best_version,
            source: resolved_source,
        })
    }

    /// Resolve a git repository crate specification.
    fn resolve_git(
        &self,
        repo: &str,
        selector: &GitSelector,
        name: &Option<String>,
        version: &Option<VersionReq>,
    ) -> Result<ResolvedCrate> {
        // Checkout using git client (returns cached checkout path and commit hash)
        let (checkout_path, commit_hash) = self.git_client.checkout_ref(repo, selector.clone())?;

        // Use cargo_metadata to read the crate info
        let metadata = self.cargo.metadata(
            &checkout_path,
            &CargoMetadataOptions {
                no_deps: true,
                ..Default::default()
            },
        )?;

        let package = if let Some(n) = name {
            metadata
                .packages
                .iter()
                .find(|p| p.name.as_str() == n)
                .with_context(|| error::PackageNotFoundInWorkspaceSnafu {
                    name: n.clone(),
                    available: metadata
                        .packages
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect::<Vec<_>>(),
                })?
        } else {
            if metadata.packages.len() != 1 {
                return error::AmbiguousPackageNameSnafu {
                    count: metadata.packages.len(),
                }
                .fail();
            }
            &metadata.packages[0]
        };

        if let Some(req) = version {
            if !req.matches(&package.version) {
                return error::VersionMismatchSnafu {
                    requirement: req.to_string(),
                    found: package.version.clone(),
                }
                .fail();
            }
        }

        Ok(ResolvedCrate {
            name: package.name.to_string(),
            version: package.version.clone(),
            source: ResolvedSource::Git {
                repo: repo.to_string(),
                commit: commit_hash,
            },
        })
    }

    /// Resolve a forge (GitHub, GitLab, etc.) crate specification.
    fn resolve_forge(
        &self,
        forge: &Forge,
        selector: &GitSelector,
        name: &Option<String>,
        version: &Option<VersionReq>,
    ) -> Result<ResolvedCrate> {
        // Convert Forge to git URL
        let git_url = forge.git_url();

        // Resolve using git resolution logic
        let mut resolved = self.resolve_git(&git_url, selector, name, version)?;

        // Replace the source with Forge instead of Git
        if let ResolvedSource::Git { commit, .. } = resolved.source {
            resolved.source = ResolvedSource::Forge {
                forge: forge.clone(),
                commit,
            };
        } else {
            panic!("BUG: Expected ResolvedSource::Git from resolve_git");
        }

        Ok(resolved)
    }
}

impl CrateResolver for DefaultCrateResolver {
    fn resolve(&self, spec: &CrateSpec) -> Result<ResolvedCrate> {
        match spec {
            CrateSpec::CratesIo { name, version } => self.resolve_registry(name, version.as_ref(), None),
            CrateSpec::Registry {
                source,
                name,
                version,
            } => self.resolve_registry(name, version.as_ref(), Some(source)),
            CrateSpec::Git {
                repo,
                selector,
                name,
                version,
            } => self.resolve_git(repo, selector, name, version),
            CrateSpec::Forge {
                forge,
                selector,
                name,
                version,
            } => self.resolve_forge(forge, selector, name, version),
            CrateSpec::LocalDir { path, name, version } => self.resolve_local_dir(path, name, version),
        }
    }
}

/// A caching wrapper around any [`CrateResolver`] implementation.
///
/// This resolver adds a caching layer on top of an inner resolver, storing resolutions
/// in a cache and using them to avoid unnecessary network requests. It also implements
/// resilient behavior like falling back to stale cache entries when network errors occur.
#[derive(Debug)]
pub(crate) struct CachingResolver<R: CrateResolver> {
    inner: R,
    cache: Cache,
}

impl<R: CrateResolver> CachingResolver<R> {
    /// Create a new [`CachingResolver`] that wraps the given inner resolver.
    pub(crate) fn new(inner: R, cache: Cache) -> Self {
        Self { inner, cache }
    }
}

impl<R: CrateResolver> CrateResolver for CachingResolver<R> {
    fn resolve(&self, spec: &CrateSpec) -> Result<ResolvedCrate> {
        if matches!(spec, CrateSpec::LocalDir { .. }) {
            return self.inner.resolve(spec);
        }

        self.cache.get_or_resolve_crate(spec, || self.inner.resolve(spec))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert_matches::assert_matches;

    use super::*;
    use crate::testdata::CrateTestCase;

    /// Create a test resolver with online config and an isolated temp directory.
    ///
    /// Returns the resolver and the `TempDir` which must be kept alive for the test duration.
    fn test_resolver() -> (CachingResolver<DefaultCrateResolver>, tempfile::TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, config) = crate::config::create_test_env();
        let reporter = crate::messages::MessageReporter::null();
        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = GitClient::new(cache.clone(), reporter.clone(), config.http.clone());
        let http_client = HttpClient::new(&config.http).unwrap();
        let resolver = DefaultCrateResolver::new(
            config.clone(),
            git_client,
            Arc::new(crate::cargo::create_cargo_runner(config.clone(), reporter).unwrap()),
            http_client,
        );
        (CachingResolver::new(resolver, cache), temp_dir)
    }

    /// Create a test resolver with offline config and an isolated temp directory.
    fn test_resolver_offline() -> (CachingResolver<DefaultCrateResolver>, tempfile::TempDir) {
        let (resolver, temp_dir) = test_resolver();
        let mut config = resolver.inner.config;
        config.offline = true;
        let reporter = crate::messages::MessageReporter::null();
        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = GitClient::new(cache.clone(), reporter, config.http.clone());
        let http_client = HttpClient::new(&config.http).unwrap();
        let resolver = DefaultCrateResolver::new(config, git_client, resolver.inner.cargo, http_client);
        (CachingResolver::new(resolver, cache), temp_dir)
    }

    /// Exercise resolving `LocalDir` crate specs using test cases from testdata/.
    mod local_dir {
        use super::*;
        use crate::error::Error;

        #[test]
        fn single_package_auto_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_bin_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: None,
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "simple-bin-no-deps");
            assert_matches!(resolved.source, ResolvedSource::LocalDir { .. });
        }

        #[test]
        fn single_package_explicit_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_bin_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: Some("simple-bin-no-deps".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "simple-bin-no-deps");
        }

        #[test]
        fn single_package_wrong_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_bin_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: Some("wrong-name".to_string()),
                version: None,
            };

            let result = resolver.resolve(&spec);
            assert_matches!(result.unwrap_err(), Error::PackageNotFoundInWorkspace { .. });
        }

        #[test]
        fn single_package_version_req_match() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_bin_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: None,
                version: Some(VersionReq::parse(">=0.1.0").unwrap()),
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.version, Version::parse("0.1.0").unwrap());
        }

        #[test]
        fn single_package_version_req_mismatch() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_bin_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: None,
                version: Some(VersionReq::parse(">=999.0.0").unwrap()),
            };

            let result = resolver.resolve(&spec);
            assert_matches!(result.unwrap_err(), Error::VersionMismatch { .. });
        }

        #[test]
        fn invalid_path() {
            let (resolver, _temp_dir) = test_resolver();
            let invalid_path = PathBuf::from("/nonexistent/path/to/nowhere");

            let spec = CrateSpec::LocalDir {
                path: invalid_path,
                name: None,
                version: None,
            };

            let result = resolver.resolve(&spec);
            assert_matches!(result.unwrap_err(), Error::CargoMetadata { .. });
        }

        #[test]
        fn workspace_ambiguous_without_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::workspace_all_libs();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: None,
                version: None,
            };

            let result = resolver.resolve(&spec);
            assert_matches!(result.unwrap_err(), Error::AmbiguousPackageName { .. });
        }

        #[test]
        fn workspace_with_valid_package_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::workspace_all_libs();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: Some("lib1".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "lib1");
            assert_eq!(resolved.version, Version::parse("0.1.0").unwrap());
        }

        #[test]
        fn workspace_with_nonexistent_package() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::workspace_all_libs();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: Some("nonexistent".to_string()),
                version: None,
            };

            let result = resolver.resolve(&spec);
            assert_matches!(result.unwrap_err(), Error::PackageNotFoundInWorkspace { .. });
        }

        #[test]
        fn workspace_package_with_version() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::workspace_all_libs();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: Some("lib2".to_string()),
                version: Some(VersionReq::parse("=0.1.0").unwrap()),
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "lib2");
            assert_eq!(resolved.version, Version::parse("0.1.0").unwrap());
        }

        #[test]
        fn library_package_auto_name() {
            let (resolver, _temp_dir) = test_resolver();
            let testcase = CrateTestCase::simple_lib_no_deps();

            let spec = CrateSpec::LocalDir {
                path: testcase.path().to_path_buf(),
                name: None,
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "simple-lib-no-deps");
            assert_matches!(resolved.source, ResolvedSource::LocalDir { .. });
        }
    }

    /// Tests exercising crate specs using a registry (mostly crates.io).
    ///
    /// These tests will actually hit the registry over the network.  Hopefully they don't get
    /// throttled.
    mod registry {
        use std::thread;

        use rand::seq::SliceRandom;

        use super::*;
        use crate::error::Error;

        #[test]
        fn serde_latest() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "serde");
            assert_matches!(resolved.source, ResolvedSource::CratesIo);
        }

        #[test]
        fn with_version() {
            let (resolver, _temp_dir) = test_resolver();
            let version_req = VersionReq::parse("^1.0").unwrap();

            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: Some(version_req.clone()),
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "serde");
            assert!(version_req.matches(&resolved.version));
        }

        #[test]
        fn star_version() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::CratesIo {
                name: "tokio".to_string(),
                version: Some(VersionReq::STAR),
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "tokio");
        }

        #[test]
        fn nonexistent() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::CratesIo {
                name: "definitely-not-a-real-crate-xyzzy-12345".to_string(),
                version: None,
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::CrateNotFoundInRegistry { .. });
        }

        /// It's safe to assume serde will never release version 999.0.0, so this tests the proper
        /// behavior when the crate exists on the registry but no compatible version is present
        #[test]
        fn non_existent_version() {
            let (resolver, _temp_dir) = test_resolver();
            let version_req = VersionReq::parse(">=999.0.0").unwrap();

            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: Some(version_req),
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::NoMatchingVersion { .. });
        }

        #[test]
        fn selects_highest_version() {
            let (resolver, _temp_dir) = test_resolver();
            let version_req = VersionReq::parse(">=1.0.0").unwrap();

            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: Some(version_req.clone()),
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert!(resolved.version.major >= 1);
            assert!(version_req.matches(&resolved.version));
        }

        /// Test that resolving an uncached crate in offline mode fails.
        ///
        /// This test attempts to resolve a definitely-nonexistent crate name in offline mode
        /// without any prior caching. Because the crate is not in `tame_index`'s cache and we're
        /// in offline mode (which only uses `cached_krate`), the resolve fails with
        /// [`OfflineMode`].
        #[test]
        fn offline_without_cached_fails() {
            let (resolver, _temp_dir) = test_resolver_offline();

            let spec = CrateSpec::CratesIo {
                name: "definitely-not-real-crate-xyzzy-offline-99999".to_string(),
                version: None,
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::OfflineMode { .. });
        }

        /// Test that resolving a cached crate in offline mode succeeds.
        ///
        /// This test first queries serde online to populate `tame_index`'s cache, then
        /// queries the same crate in offline mode. The second query should succeed
        /// by using our cached resolution result. While we can't prove the network wasn't
        /// used, this exercises the offline code path that calls `cached_krate` instead of krate.
        #[test]
        fn offline_with_cached_works() {
            let (online_resolver, _temp_dir) = test_resolver();

            // Query serde online first to populate tame_index cache
            let version_req = VersionReq::parse("^1.0").unwrap();
            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: Some(version_req),
            };

            let online_resolved = online_resolver.resolve(&spec).unwrap();

            // Now try offline mode - should work because the crate resolution is cached
            let offline_config = Config {
                offline: true,
                ..online_resolver.inner.config.clone()
            };
            let git_client = GitClient::new(
                online_resolver.cache.clone(),
                crate::messages::MessageReporter::null(),
                offline_config.http.clone(),
            );
            let http_client = HttpClient::new(&offline_config.http).unwrap();
            let offline_resolver = CachingResolver::new(
                DefaultCrateResolver::new(
                    offline_config,
                    git_client,
                    online_resolver.inner.cargo.clone(),
                    http_client,
                ),
                online_resolver.cache.clone(),
            );

            let offline_resolved = offline_resolver.resolve(&spec).unwrap();

            assert_eq!(online_resolved.name, offline_resolved.name);
            assert_eq!(online_resolved.version, offline_resolved.version);
        }

        /// Test that stale cgx cache entries are returned in offline mode for invalid crate names.
        ///
        /// This test inserts a fake cache entry for an invalid crate name (+invalid-crate-name,
        /// which contains characters not allowed in crate names) with a stale timestamp. When
        /// resolving in offline mode, the resolver returns the stale entry. We know for certain
        /// the network wasn't hit because querying crates.io for an invalid crate name would
        /// cause an error.
        #[test]
        fn stale_invalid_crate_returned_in_offline_mode() {
            let (resolver, _temp_dir) = test_resolver();
            let cache_timeout = resolver.inner.config.resolve_cache_timeout;

            // Create a spec for an invalid crate name (+ is not valid in crate names)
            let invalid_spec = CrateSpec::CratesIo {
                name: "+invalid-crate-name".to_string(),
                version: None,
            };

            // Create a fake resolved crate
            let fake_resolved = ResolvedCrate {
                name: "+invalid-crate-name".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            // Insert a stale cache entry (older than timeout)
            resolver
                .cache
                .insert_stale_resolve_entry(
                    &invalid_spec,
                    &fake_resolved,
                    cache_timeout + Duration::from_secs(1),
                )
                .unwrap();

            // Create offline config and resolver, that shares the same cache as the one we just
            // inserted the stale cache entry into
            let offline_config = Config {
                offline: true,
                ..resolver.inner.config.clone()
            };
            let git_client = GitClient::new(
                resolver.cache.clone(),
                crate::messages::MessageReporter::null(),
                offline_config.http.clone(),
            );
            let http_client = HttpClient::new(&offline_config.http).unwrap();
            let offline_resolver = CachingResolver::new(
                DefaultCrateResolver::new(
                    offline_config,
                    git_client,
                    resolver.inner.cargo.clone(),
                    http_client,
                ),
                resolver.cache,
            );

            // Query in offline mode - should return stale entry without hitting network
            let resolved = offline_resolver.resolve(&invalid_spec).unwrap();

            assert_eq!(resolved.name, fake_resolved.name);
            assert_eq!(resolved.version, fake_resolved.version);
        }

        /// Test that a non-stale cache entry is served without querying the registry.
        ///
        /// This test inserts a fake serde@999.99.99 entry (which doesn't exist on crates.io)
        /// into the cgx cache with a fresh timestamp. When resolving in online mode, if the cache
        /// entry is returned, we know for certain that the registry was not queried (because
        /// the registry would fail to find version 999.99.99, which doesn't exist).
        #[test]
        fn cache_serves_non_stale_entry_without_registry_lookup() {
            let (resolver, _temp_dir) = test_resolver();

            // Create a spec for serde with a nonexistent version
            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: None,
            };

            // Create a fake resolved crate with a version that doesn't exist
            let fake_resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("999.99.99").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            // Insert a NON-stale cache entry (fresh, within timeout)
            resolver
                .cache
                .insert_stale_resolve_entry(&spec, &fake_resolved, Duration::from_secs(1))
                .unwrap();

            // Query in online mode - should return the cached fake entry without hitting registry
            let resolved = resolver.resolve(&spec).unwrap();

            assert_eq!(resolved.name, "serde");
            assert_eq!(resolved.version, Version::parse("999.99.99").unwrap());
        }

        /// Test that stale cache entries are not used as fallback for permanent errors.
        ///
        /// This test inserts a fake serde@999.99.99 entry into the cache with a stale timestamp.
        /// When resolving in online mode, the resolver queries the registry, which returns
        /// `NoMatchingVersion` (since 999.99.99 doesn't exist). Because `NoMatchingVersion` is not
        /// a transient error (not in `should_use_stale_cache` list), the stale cache should NOT
        /// be used as a fallback, and the error should propagate.
        #[test]
        fn stale_cache_not_used_for_permanent_errors() {
            let (resolver, _temp_dir) = test_resolver();
            let cache_timeout = resolver.inner.config.resolve_cache_timeout;

            // Create a spec for serde with a specific nonexistent version
            let spec = CrateSpec::CratesIo {
                name: "serde".to_string(),
                version: Some(VersionReq::parse("=999.99.99").unwrap()),
            };

            // Create a fake resolved crate with the nonexistent version
            let fake_resolved = ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("999.99.99").unwrap(),
                source: ResolvedSource::CratesIo,
            };

            // Insert a STALE cache entry (older than timeout)
            resolver
                .cache
                .insert_stale_resolve_entry(&spec, &fake_resolved, cache_timeout + Duration::from_secs(1))
                .unwrap();

            // Query in online mode - should fail because registry returns NoMatchingVersion
            // and stale cache is not used for this error type
            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::NoMatchingVersion { .. });
        }

        /// Stress test to reproduce Windows file lock bug through heavy lock contention.
        ///
        /// This test spawns 20 threads that all simultaneously query different crates from
        /// crates.io in random order. Each thread queries 10 crates, creating heavy contention
        /// for the cargo package cache lock.
        ///
        /// On Windows with the tame-index bug where `None` timeout maps to 0 instead of INFINITE,
        /// threads will fail with `TimedOut` errors. With the fix, all threads properly wait
        /// for the lock and succeed.
        ///
        /// This reproduces: <https://github.com/EmbarkStudios/tame-index/issues/94>
        #[test]
        fn lock_contention_stress_test() {
            let crates = vec![
                "serde",
                "tokio",
                "reqwest",
                "clap",
                "anyhow",
                "thiserror",
                "tracing",
                "syn",
                "quote",
                "proc-macro2",
                "serde_json",
                "regex",
                "rayon",
                "bytes",
                "http",
                "futures",
                "async-trait",
                "rand",
                "chrono",
                "log",
            ];

            let (resolver, _temp_dir) = test_resolver();
            let resolver = Arc::new(resolver);

            let mut handles = vec![];

            for thread_id in 0..20 {
                let resolver = Arc::clone(&resolver);
                let mut thread_crates = crates.clone();

                let handle = thread::spawn(move || {
                    let mut rng = rand::rng();
                    thread_crates.shuffle(&mut rng);

                    for krate_name in thread_crates.iter().take(10) {
                        let spec = CrateSpec::CratesIo {
                            name: (*krate_name).to_string(),
                            version: None,
                        };

                        match resolver.resolve(&spec) {
                            Ok(resolved) => {
                                assert_eq!(resolved.name, *krate_name);
                            }
                            Err(e) => {
                                panic!("[Thread {}] Failed to resolve {}: {:?}", thread_id, krate_name, e);
                            }
                        }
                    }
                });

                handles.push(handle);
            }

            for (i, handle) in handles.into_iter().enumerate() {
                handle
                    .join()
                    .unwrap_or_else(|e| panic!("Thread {} panicked: {:?}", i, e));
            }
        }
    }

    /// Tests exercising crate specs pointing to git repositories.
    mod git {
        use super::*;
        use crate::error::Error;

        /// Absent any kind of selector, defaults to the most recent commit on the default branch.
        #[test]
        fn default_branch() {
            let (resolver, _temp_dir) = test_resolver();
            let repo = "https://github.com/rust-lang/rustlings.git";

            let spec = CrateSpec::Git {
                repo: repo.to_string(),
                selector: GitSelector::DefaultBranch,
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "rustlings");
            if let ResolvedSource::Git { repo: r, commit } = &resolved.source {
                assert_eq!(r, repo);
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Git source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn with_branch() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::Branch("main".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            if let ResolvedSource::Git { commit, .. } = &resolved.source {
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Git source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn with_tag() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::Tag("v6.0.0".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            if let ResolvedSource::Git { commit, .. } = &resolved.source {
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Git source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn with_commit() {
            let (resolver, _temp_dir) = test_resolver();

            // Use actual commit hash (not tag object hash)
            // This is the commit that v6.0.0 tag points to
            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::Commit("28d2bb04326d7036514245d73f10fb72b9ed108c".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "rustlings");

            if let ResolvedSource::Git { repo: r, commit } = &resolved.source {
                assert_eq!(r, "https://github.com/rust-lang/rustlings.git");
                assert_eq!(commit, "28d2bb04326d7036514245d73f10fb72b9ed108c");
            } else {
                panic!("Expected Git source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn nonexistent_branch() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::Branch("nonexistent-branch-xyzzy-99999".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::Git { .. });
        }

        #[test]
        fn nonexistent_tag() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::Tag("999.999.999".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::Git { .. });
        }

        #[test]
        fn invalid_url() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Git {
                repo: "https://[invalid-url".to_string(),
                selector: GitSelector::DefaultBranch,
                name: None,
                version: None,
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::Git { .. });
        }

        /// As with local paths, versions don't have to be specified when pointing to a git repo
        /// but if specified the version must be compatible with whatever is at that repo
        #[test]
        fn version_mismatch() {
            let (resolver, _temp_dir) = test_resolver();
            // As `rustlings` evolves this version must remain compatible with it; presumably it's
            // a long way off from version 999...
            let version_req = VersionReq::parse(">=999.0.0").unwrap();

            let spec = CrateSpec::Git {
                repo: "https://github.com/rust-lang/rustlings.git".to_string(),
                selector: GitSelector::DefaultBranch,
                name: Some("rustlings".to_string()),
                version: Some(version_req),
            };

            let result = resolver.resolve(&spec);

            assert_matches!(result.unwrap_err(), Error::VersionMismatch { .. });
        }
    }

    /// Tests exercising crate specs pointing to forges (GitHub, GitLab, etc.)
    ///
    /// Mostly this is just a thin wrapper around git resolution, so these tests are lighter.
    /// We don't care about the forge vs other git distinction until we start looking for
    /// pre-built binaries to download, which is outside of the scope of this module.
    mod forge {
        use super::*;

        #[test]
        fn github() {
            let (resolver, _temp_dir) = test_resolver();
            let spec = CrateSpec::Forge {
                forge: Forge::GitHub {
                    custom_url: None,
                    owner: "rust-lang".to_string(),
                    repo: "rustlings".to_string(),
                },
                selector: GitSelector::DefaultBranch,
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            assert_eq!(resolved.name, "rustlings");
            if let ResolvedSource::Forge { forge: f, commit } = &resolved.source {
                assert_matches!(f, Forge::GitHub { .. });
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Forge source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn github_with_branch() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Forge {
                forge: Forge::GitHub {
                    custom_url: None,
                    owner: "rust-lang".to_string(),
                    repo: "rustlings".to_string(),
                },
                selector: GitSelector::Branch("main".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            if let ResolvedSource::Forge { forge: f, commit, .. } = &resolved.source {
                assert_matches!(f, Forge::GitHub { .. });
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Forge source, got {:?}", resolved.source);
            }
        }

        #[test]
        fn github_with_tag() {
            let (resolver, _temp_dir) = test_resolver();

            let spec = CrateSpec::Forge {
                forge: Forge::GitHub {
                    custom_url: None,
                    owner: "rust-lang".to_string(),
                    repo: "rustlings".to_string(),
                },
                selector: GitSelector::Tag("v6.0.0".to_string()),
                name: Some("rustlings".to_string()),
                version: None,
            };

            let resolved = resolver.resolve(&spec).unwrap();
            if let ResolvedSource::Forge { forge: f, commit, .. } = &resolved.source {
                assert_matches!(f, Forge::GitHub { .. });
                assert!(!commit.is_empty());
            } else {
                panic!("Expected Forge source, got {:?}", resolved.source);
            }
        }
    }
}
