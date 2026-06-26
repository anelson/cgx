mod providers;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use providers::{BinstallProvider, GithubProvider, GitlabProvider, Provider, QuickinstallProvider};
use serde::{Deserialize, Serialize};
use snafu::{IntoError, ResultExt};

use crate::{
    Result,
    builder::{BuildOptions, BuildTarget},
    cache::{BinaryCacheEntry, Cache},
    config::{BinaryProvider, Config, UsePrebuiltBinaries},
    crate_resolver::ResolvedCrate,
    downloader::DownloadedCrate,
    error::{self, Error},
    http::HttpClient,
    messages::{MessageReporter, PrebuiltBinaryMessage},
};

/// A resolved binary is a pre-built executable that cgx found and prepared, so the crate can run
/// without being built from source.
///
/// This type is the result of resolving a [`ResolvedCrate`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedBinary {
    /// The crate for which this binary was resolved
    pub krate: ResolvedCrate,

    /// From what binary provider this binary was obtained
    pub provider: BinaryProvider,

    /// Path to the executable cgx should run
    pub path: std::path::PathBuf,
}

pub trait BinaryResolver {
    /// Attempt to resolve a pre-built binary for the given crate from cache or providers.
    ///
    /// Returns:
    /// - `Ok(Some(ResolvedBinary))` - Found a pre-built binary
    /// - `Ok(None)` - No pre-built binary available (or pre-built binaries are
    ///   disabled/disqualified, or resolution was inconclusive in a non-`always` mode)
    /// - `Err(...)` - Resolution failed in a way that should stop execution
    fn resolve(
        &self,
        krate: &DownloadedCrate,
        build_options: &BuildOptions,
    ) -> Result<Option<ResolvedBinary>>;
}

/// The outcome of attempting to resolve a pre-built binary
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "this is a short-lived return value (a handful per resolution), never stored in bulk; boxing \
              the common Found payload would only add a heap allocation to the success path"
)]
enum BinaryResolution {
    /// A pre-built binary was found and downloaded.
    Found(ResolvedBinary),
    /// We determined conclusively that no pre-built binary is available.
    Nonexistent,
    /// We could not determine whether a pre-built binary exists, because a transient error (a rate
    /// limit, a network failure, etc) prevented a definitive answer. Behaves as "none for now"
    /// but must never be cached.
    Inconclusive { source: Box<Error> },
}

/// Create the default [`BinaryResolver`] implementation, respecting the given config and using the
/// provided cache.
pub(crate) fn create_resolver(
    config: Config,
    cache: Cache,
    reporter: MessageReporter,
    http_client: HttpClient,
) -> impl BinaryResolver {
    BinaryResolverImpl::new(config, cache, reporter, http_client)
}

/// The prod [`BinaryResolver`] implementation, which delegates to the configured providers and
/// integrates with the cache to avoid repeated expensive provider calls.
struct BinaryResolverImpl {
    config: Config,
    cache: Cache,
    reporter: MessageReporter,
    mode: UsePrebuiltBinaries,
    providers: Vec<Box<dyn Provider + Send + Sync>>,
}

impl BinaryResolverImpl {
    fn new(config: Config, cache: Cache, reporter: MessageReporter, http_client: HttpClient) -> Self {
        let providers = {
            let cache_dir = &config.cache_dir;
            let verify = config.prebuilt_binaries.verify_checksums;

            config
                .prebuilt_binaries
                .binary_providers
                .iter()
                .map(|provider_type| -> Box<dyn Provider + Send + Sync> {
                    match provider_type {
                        BinaryProvider::Binstall => Box::new(BinstallProvider::new(
                            reporter.clone(),
                            cache_dir.clone(),
                            verify,
                            http_client.clone(),
                        )),
                        BinaryProvider::GithubReleases => Box::new(GithubProvider::new(
                            reporter.clone(),
                            cache_dir.clone(),
                            verify,
                            http_client.clone(),
                        )),
                        BinaryProvider::GitlabReleases => Box::new(GitlabProvider::new(
                            reporter.clone(),
                            cache_dir.clone(),
                            verify,
                            http_client.clone(),
                        )),
                        BinaryProvider::Quickinstall => Box::new(QuickinstallProvider::new(
                            reporter.clone(),
                            cache_dir.clone(),
                            http_client.clone(),
                        )),
                    }
                })
                .collect()
        };

        Self::with_providers(config, cache, reporter, providers)
    }

    fn with_providers(
        config: Config,
        cache: Cache,
        reporter: MessageReporter,
        providers: Vec<Box<dyn Provider + Send + Sync>>,
    ) -> Self {
        let mode = config.prebuilt_binaries.use_prebuilt_binaries;
        Self {
            config,
            cache,
            reporter,
            mode,
            providers,
        }
    }

    /// Check if the build options disqualify the use of pre-built binaries.
    ///
    /// Pre-built binaries are skipped when the request changes what Cargo would build, such as
    /// selecting features, a target, a profile, a toolchain, a bin, or an example.
    fn is_disqualified(build_options: &BuildOptions) -> Option<&'static str> {
        if build_options.build_target != BuildTarget::DefaultBin {
            return Some("explicit --bin or --example specified");
        }

        if !build_options.features.is_empty() {
            return Some("custom features specified");
        }

        if build_options.all_features {
            return Some("--all-features specified");
        }

        if build_options.no_default_features {
            return Some("--no-default-features specified");
        }

        if build_options.profile.is_some() {
            return Some("custom profile specified");
        }

        if build_options.target.is_some() {
            return Some("custom target specified");
        }

        if build_options.toolchain.is_some() {
            return Some("custom toolchain specified");
        }

        None
    }

    /// Combine the per-provider resolutions into a single outcome.
    ///
    /// Precedence is `Found` > `Inconclusive` > `Nonexistent`: a found binary wins outright;
    /// failing that, if any provider was inconclusive the overall result is inconclusive (we
    /// cannot rule out a binary), keeping the first inconclusive error; only if every provider
    /// conclusively reported no binary is the result `Nonexistent`. An empty iterator yields
    /// `Nonexistent`.
    fn combine(resolutions: impl IntoIterator<Item = BinaryResolution>) -> BinaryResolution {
        let mut inconclusive: Option<Box<Error>> = None;
        for resolution in resolutions {
            match resolution {
                BinaryResolution::Found(binary) => return BinaryResolution::Found(binary),
                BinaryResolution::Inconclusive { source } => {
                    inconclusive.get_or_insert(source);
                }
                BinaryResolution::Nonexistent => {}
            }
        }
        match inconclusive {
            Some(source) => BinaryResolution::Inconclusive { source },
            None => BinaryResolution::Nonexistent,
        }
    }

    /// Map a binary resolution to the [`BinaryCacheEntry`] (if any) that should be written to the
    /// binary cache.
    ///
    /// `Found`/`Nonexistent` are conclusive and cacheable; `Inconclusive` returns `None` and is
    /// never cached. This is the core invariant that prevents a transient failure from being
    /// persisted as a negative.
    fn cacheable(resolution: &BinaryResolution) -> Option<BinaryCacheEntry> {
        match resolution {
            BinaryResolution::Found(binary) => Some(BinaryCacheEntry::Found(binary.clone())),
            BinaryResolution::Nonexistent => Some(BinaryCacheEntry::Nonexistent),
            BinaryResolution::Inconclusive { .. } => None,
        }
    }

    /// Convert a binary resolution to the public `Option<ResolvedBinary>`, applying the
    /// `--prebuilt-binary` mode policy.
    ///
    /// This will fail with appropriate errors depending on what mode is specified and what the
    /// resolution actually was.
    fn apply_mode(
        resolution: BinaryResolution,
        mode: UsePrebuiltBinaries,
        krate: &ResolvedCrate,
    ) -> Result<Option<ResolvedBinary>> {
        // If mode was never, execution would not make it this far, so we don't have to handle that
        debug_assert_ne!(mode, UsePrebuiltBinaries::Never);

        match resolution {
            BinaryResolution::Found(binary) => Ok(Some(binary)),
            BinaryResolution::Nonexistent => {
                if mode == UsePrebuiltBinaries::Always {
                    error::PrebuiltBinaryRequiredSnafu {
                        name: krate.name.clone(),
                        version: krate.version.to_string(),
                    }
                    .fail()
                } else {
                    Ok(None)
                }
            }
            BinaryResolution::Inconclusive { source } => {
                if mode == UsePrebuiltBinaries::Always {
                    Err(error::PrebuiltBinaryResolutionFailedSnafu {
                        name: krate.name.clone(),
                        version: krate.version.to_string(),
                    }
                    .into_error(source))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Consult each configured provider in order, short-circuiting on the first `Found`, and fold
    /// the results with [`Self::combine`].
    fn resolve_via_providers(&self, krate: &DownloadedCrate, platform: &str) -> Result<BinaryResolution> {
        if self.providers.is_empty() {
            return error::NoProvidersConfiguredSnafu.fail();
        }

        let resolved = &krate.resolved;
        let mut results = Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            self.reporter
                .report(|| PrebuiltBinaryMessage::checking_provider(resolved, provider.kind()));
            let resolution = provider.try_resolve(krate, platform)?;
            let found = matches!(resolution, BinaryResolution::Found(_));
            results.push(resolution);
            if found {
                break;
            }
        }

        Ok(Self::combine(results))
    }

    /// Relocate a resolved binary from the provider's cache to the `bin_dir` structure.
    ///
    /// Pre-built binaries are copied into `bin_dir` so the path cgx returns is stable and separate
    /// from provider-specific cache directories.
    fn relocate_to_bin_dir(
        &self,
        mut binary: ResolvedBinary,
        krate: &ResolvedCrate,
        platform: &str,
    ) -> Result<ResolvedBinary> {
        let source_hash = krate.source.source_hash();

        // Build target directory: bin_dir/<crate>-<version>/<source-hash>/prebuilt-<provider>-<platform>/
        let target_dir = self
            .config
            .bin_dir
            .join(format!("{}-{}", krate.name, krate.version))
            .join(source_hash)
            .join(format!("prebuilt-{:?}-{}", binary.provider, platform));

        std::fs::create_dir_all(&target_dir).with_context(|_| error::IoSnafu {
            path: target_dir.clone(),
        })?;

        let binary_name = binary.path.file_name().ok_or_else(|| Error::Io {
            path: binary.path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "binary path has no filename"),
        })?;

        let target_path = target_dir.join(binary_name);

        // Copy (don't move) so the provider's cache remains intact
        std::fs::copy(&binary.path, &target_path).with_context(|_| error::CopyBinarySnafu {
            src: binary.path.clone(),
            dst: target_path.clone(),
        })?;

        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&target_path)
                .with_context(|_| error::IoSnafu {
                    path: target_path.clone(),
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&target_path, perms).with_context(|_| error::IoSnafu {
                path: target_path.clone(),
            })?;
        }

        binary.path = target_path;
        Ok(binary)
    }
}

impl BinaryResolver for BinaryResolverImpl {
    fn resolve(
        &self,
        krate: &DownloadedCrate,
        build_options: &BuildOptions,
    ) -> Result<Option<ResolvedBinary>> {
        let resolved_krate = &krate.resolved;

        tracing::debug!(
            "BinaryResolver::resolve called for {}@{}",
            resolved_krate.name,
            resolved_krate.version
        );

        if self.mode == UsePrebuiltBinaries::Never {
            self.reporter
                .report(PrebuiltBinaryMessage::prebuilt_binaries_disabled);
            return Ok(None);
        }

        // Check build-options disqualification BEFORE touching the cache: the binary cache is keyed
        // on the resolved crate alone, so a cached binary must not be served to a build whose
        // options disqualify the user of prebuilt binaries (such as options that enable or disable
        // features, or otherwise require a binary that is built from source).
        if let Some(reason) = Self::is_disqualified(build_options) {
            if self.mode == UsePrebuiltBinaries::Always {
                return error::PrebuiltBinaryDisqualifiedSnafu {
                    name: resolved_krate.name.clone(),
                    version: resolved_krate.version.to_string(),
                    reason,
                }
                .fail();
            }
            self.reporter
                .report(|| PrebuiltBinaryMessage::disqualified_due_to_customization(reason));
            return Ok(None);
        }

        // Check the binary resolution cache first unless refresh mode is enabled. Only conclusive
        // outcomes are ever stored, so a cache hit is always authoritative. The cache itself reports
        // the lookup/hit/miss events; here we only translate a hit into a resolution outcome.
        if !self.config.refresh {
            if let Ok(Some(cached)) = self.cache.get_cached_binary(resolved_krate) {
                let resolution = if let BinaryCacheEntry::Found(binary) = cached {
                    BinaryResolution::Found(binary)
                } else {
                    // The cache already reported the negative hit.  There is no reason to try to
                    // find a prebuilt binary again.
                    self.reporter.report(|| {
                        PrebuiltBinaryMessage::no_binary_found(
                            resolved_krate,
                            vec!["negative cache hit - no binary available".to_string()],
                        )
                    });
                    BinaryResolution::Nonexistent
                };
                return Self::apply_mode(resolution, self.mode, resolved_krate);
            }
        }

        // Always use the build target platform for pre-built binaries. If the user overrides this by
        // specifying a custom target, execution is not supposed to reach this point.
        let platform: &'static str = build_context::TARGET;

        let resolution = self.resolve_via_providers(krate, platform)?;

        // For a found binary, relocate it into `bin_dir` (so the cached/returned path is stable and
        // separate from provider caches) and report it. Report the terminal non-found states too.
        let resolution = match resolution {
            BinaryResolution::Found(binary) => {
                let relocated = self.relocate_to_bin_dir(binary, resolved_krate, platform)?;
                self.reporter
                    .report(|| PrebuiltBinaryMessage::resolved(&relocated));
                BinaryResolution::Found(relocated)
            }
            BinaryResolution::Nonexistent => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::no_binary_found(
                        resolved_krate,
                        vec!["no binary found from any configured provider".to_string()],
                    )
                });
                BinaryResolution::Nonexistent
            }
            BinaryResolution::Inconclusive { source } => {
                self.reporter
                    .report(|| PrebuiltBinaryMessage::resolution_inconclusive(source.to_string()));
                BinaryResolution::Inconclusive { source }
            }
        };

        // Persist only conclusive outcomes; an inconclusive result is structurally uncacheable.
        if let Some(entry) = Self::cacheable(&resolution) {
            self.cache.put_cached_binary(resolved_krate, &entry)?;
        }

        Self::apply_mode(resolution, self.mode, resolved_krate)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use assert_matches::assert_matches;
    use semver::Version;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        builder::{BuildOptions, BuildTarget},
        crate_resolver::ResolvedSource,
    };

    /// Test that default build options are not disqualified
    #[test]
    fn test_disqualification_default_options_ok() {
        let options = BuildOptions::default();
        assert_eq!(BinaryResolverImpl::is_disqualified(&options), None);
    }

    /// Test that explicit --bin flag disqualifies pre-built binaries
    #[test]
    fn test_disqualification_explicit_bin() {
        let options = BuildOptions {
            build_target: BuildTarget::Bin("specific-bin".to_string()),
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("explicit --bin or --example specified")
        );
    }

    /// Test that explicit --example flag disqualifies pre-built binaries
    #[test]
    fn test_disqualification_explicit_example() {
        let options = BuildOptions {
            build_target: BuildTarget::Example("my-example".to_string()),
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("explicit --bin or --example specified")
        );
    }

    /// Test that custom features disqualify pre-built binaries
    #[test]
    fn test_disqualification_custom_features() {
        let options = BuildOptions {
            features: vec!["serde".to_string(), "json".to_string()],
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("custom features specified")
        );
    }

    /// Test that --all-features disqualifies pre-built binaries
    #[test]
    fn test_disqualification_all_features() {
        let options = BuildOptions {
            all_features: true,
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("--all-features specified")
        );
    }

    /// Test that --no-default-features disqualifies pre-built binaries
    #[test]
    fn test_disqualification_no_default_features() {
        let options = BuildOptions {
            no_default_features: true,
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("--no-default-features specified")
        );
    }

    /// Test that custom profile disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_profile() {
        let options = BuildOptions {
            profile: Some("release-with-debug".to_string()),
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("custom profile specified")
        );
    }

    /// Test that custom target disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_target() {
        let options = BuildOptions {
            target: Some("x86_64-unknown-linux-musl".to_string()),
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("custom target specified")
        );
    }

    /// Test that custom toolchain disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_toolchain() {
        let options = BuildOptions {
            toolchain: Some("nightly".to_string()),
            ..Default::default()
        };
        assert_eq!(
            BinaryResolverImpl::is_disqualified(&options),
            Some("custom toolchain specified")
        );
    }

    /// The canned outcome a [`StubProvider`] returns.
    #[expect(
        clippy::large_enum_variant,
        reason = "test stub; at most one instance exists per test, so the size disparity is irrelevant"
    )]
    enum StubOutcome {
        Found(ResolvedBinary),
        Nonexistent,
        Inconclusive,
    }

    /// A [`Provider`] standing in for a real provider, returning a canned [`BinaryResolution`] and
    /// counting how often it is consulted (so tests can assert short-circuiting and cache hits).
    struct StubProvider {
        outcome: StubOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl Provider for StubProvider {
        fn kind(&self) -> BinaryProvider {
            BinaryProvider::GithubReleases
        }

        fn try_resolve(&self, _krate: &DownloadedCrate, _platform: &str) -> Result<BinaryResolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match &self.outcome {
                StubOutcome::Found(binary) => BinaryResolution::Found(binary.clone()),
                StubOutcome::Nonexistent => BinaryResolution::Nonexistent,
                StubOutcome::Inconclusive => BinaryResolution::Inconclusive {
                    source: boxed_transient(),
                },
            })
        }
    }

    /// A boxed transient (HTTP 429) error, standing in for a rate limit error / network glitch.
    fn boxed_transient() -> Box<Error> {
        Box::new(
            error::HttpStatusSnafu {
                url: "https://api.github.com/repos/x/y/releases/tags/v1.0.0".to_string(),
                status: 429u16,
            }
            .build(),
        )
    }

    fn test_env() -> (Cache, Config, TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, config) = crate::config::create_test_env();
        let cache = Cache::new(config.clone(), MessageReporter::null());
        (cache, config, temp_dir)
    }

    /// Build a [`BinaryResolverImpl`] backed by a single [`StubProvider`] with the given mode and
    /// canned outcome.
    fn resolver_with(
        cache: Cache,
        config: Config,
        mode: UsePrebuiltBinaries,
        outcome: StubOutcome,
    ) -> (BinaryResolverImpl, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut config = config;
        config.prebuilt_binaries.use_prebuilt_binaries = mode;
        let providers: Vec<Box<dyn Provider + Send + Sync>> = vec![Box::new(StubProvider {
            outcome,
            calls: calls.clone(),
        })];
        (
            BinaryResolverImpl::with_providers(config, cache, MessageReporter::null(), providers),
            calls,
        )
    }

    fn test_downloaded_crate() -> DownloadedCrate {
        DownloadedCrate {
            resolved: ResolvedCrate {
                name: "serde".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::CratesIo,
            },
            crate_path: PathBuf::from("/nonexistent"),
        }
    }

    fn test_resolved_binary() -> ResolvedBinary {
        ResolvedBinary {
            krate: test_downloaded_crate().resolved,
            provider: BinaryProvider::GithubReleases,
            path: PathBuf::from("/fake/bin/serde"),
        }
    }

    #[test]
    fn combine_empty_is_nonexistent() {
        assert_matches!(
            BinaryResolverImpl::combine(Vec::<BinaryResolution>::new()),
            BinaryResolution::Nonexistent
        );
    }

    #[test]
    fn combine_all_nonexistent_is_nonexistent() {
        let combined =
            BinaryResolverImpl::combine([BinaryResolution::Nonexistent, BinaryResolution::Nonexistent]);
        assert_matches!(combined, BinaryResolution::Nonexistent);
    }

    #[test]
    fn combine_any_found_wins() {
        let combined = BinaryResolverImpl::combine([
            BinaryResolution::Inconclusive {
                source: boxed_transient(),
            },
            BinaryResolution::Found(test_resolved_binary()),
            BinaryResolution::Nonexistent,
        ]);
        assert_matches!(combined, BinaryResolution::Found(_));
    }

    #[test]
    fn combine_inconclusive_beats_nonexistent() {
        let combined = BinaryResolverImpl::combine([
            BinaryResolution::Nonexistent,
            BinaryResolution::Inconclusive {
                source: boxed_transient(),
            },
            BinaryResolution::Nonexistent,
        ]);
        assert_matches!(combined, BinaryResolution::Inconclusive { .. });
    }

    #[test]
    fn cacheable_found_and_nonexistent_but_never_inconclusive() {
        assert_matches!(
            BinaryResolverImpl::cacheable(&BinaryResolution::Found(test_resolved_binary())),
            Some(BinaryCacheEntry::Found(_))
        );
        assert_matches!(
            BinaryResolverImpl::cacheable(&BinaryResolution::Nonexistent),
            Some(BinaryCacheEntry::Nonexistent)
        );
        assert_matches!(
            BinaryResolverImpl::cacheable(&BinaryResolution::Inconclusive {
                source: boxed_transient()
            }),
            None
        );
    }

    #[test]
    fn apply_mode_found_returns_binary_in_any_mode() {
        let resolved = test_downloaded_crate().resolved;
        for mode in [UsePrebuiltBinaries::Auto, UsePrebuiltBinaries::Always] {
            let out = BinaryResolverImpl::apply_mode(
                BinaryResolution::Found(test_resolved_binary()),
                mode,
                &resolved,
            )
            .unwrap();
            assert_matches!(out, Some(_));
        }
    }

    #[test]
    fn apply_mode_nonexistent_is_none_in_auto_but_errors_in_always() {
        let resolved = test_downloaded_crate().resolved;
        assert_matches!(
            BinaryResolverImpl::apply_mode(
                BinaryResolution::Nonexistent,
                UsePrebuiltBinaries::Auto,
                &resolved
            ),
            Ok(None)
        );
        assert_matches!(
            BinaryResolverImpl::apply_mode(
                BinaryResolution::Nonexistent,
                UsePrebuiltBinaries::Always,
                &resolved
            ),
            Err(Error::PrebuiltBinaryRequired { .. })
        );
    }

    #[test]
    fn apply_mode_inconclusive_is_none_in_auto_but_errors_with_source_in_always() {
        let resolved = test_downloaded_crate().resolved;
        assert_matches!(
            BinaryResolverImpl::apply_mode(
                BinaryResolution::Inconclusive {
                    source: boxed_transient()
                },
                UsePrebuiltBinaries::Auto,
                &resolved
            ),
            Ok(None)
        );
        let err = BinaryResolverImpl::apply_mode(
            BinaryResolution::Inconclusive {
                source: boxed_transient(),
            },
            UsePrebuiltBinaries::Always,
            &resolved,
        )
        .unwrap_err();
        assert_matches!(
            err,
            Error::PrebuiltBinaryResolutionFailed { ref name, .. } if name == "serde"
        );
    }

    /// In `always` mode, build options that disqualify the use of prebuilt binaries fail the binary
    /// resolution because the user demanded a prebuilt binary, and yet the build options would
    /// force a source build. Providers are not consulted.
    #[test]
    fn always_mode_rejects_disqualifying_build_options() {
        let (cache, config, _temp) = test_env();
        let (resolver, calls) = resolver_with(
            cache,
            config,
            UsePrebuiltBinaries::Always,
            StubOutcome::Nonexistent,
        );
        let options = BuildOptions {
            profile: Some("dev".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve(&test_downloaded_crate(), &options);

        assert_matches!(
            result,
            Err(Error::PrebuiltBinaryDisqualified { ref name, ref reason, .. })
                if name == "serde" && reason.contains("profile")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// In `auto` mode, build options that disquality the use of prebuilt binaries cause so the
    /// binary resolution to fall back to a source build.
    #[test]
    fn auto_mode_skips_prebuilt_for_disqualifying_build_options() {
        let (cache, config, _temp) = test_env();
        let (resolver, calls) =
            resolver_with(cache, config, UsePrebuiltBinaries::Auto, StubOutcome::Nonexistent);
        let options = BuildOptions {
            profile: Some("dev".to_string()),
            ..Default::default()
        };

        let result = resolver.resolve(&test_downloaded_crate(), &options).unwrap();

        assert_eq!(result, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// In `always` mode a conclusively-missing prebuilt binary is an error because we are not
    /// permitted to fall back to building from source.
    #[test]
    fn always_mode_errors_when_no_provider_has_binary() {
        let (cache, config, _temp) = test_env();
        let (resolver, calls) = resolver_with(
            cache,
            config,
            UsePrebuiltBinaries::Always,
            StubOutcome::Nonexistent,
        );

        let result = resolver.resolve(&test_downloaded_crate(), &BuildOptions::default());

        assert_matches!(
            result,
            Err(Error::PrebuiltBinaryRequired { ref name, .. }) if name == "serde"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// `always` mode applies to cached negative results too: a previous run's conclusive "no binary
    /// available" answer fails the invocation instead of silently building from source, and is
    /// served from cache without re-consulting providers.
    #[test]
    fn always_mode_errors_on_cached_negative_result() {
        let (cache, config, _temp) = test_env();

        let (auto_resolver, auto_calls) = resolver_with(
            cache.clone(),
            config.clone(),
            UsePrebuiltBinaries::Auto,
            StubOutcome::Nonexistent,
        );
        assert_matches!(
            auto_resolver.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(None)
        );
        assert_eq!(auto_calls.load(Ordering::SeqCst), 1);

        let (always_resolver, always_calls) = resolver_with(
            cache,
            config,
            UsePrebuiltBinaries::Always,
            StubOutcome::Nonexistent,
        );
        let result = always_resolver.resolve(&test_downloaded_crate(), &BuildOptions::default());

        assert_matches!(result, Err(Error::PrebuiltBinaryRequired { .. }));
        assert_eq!(always_calls.load(Ordering::SeqCst), 0);
    }

    /// In `never` mode the cache and providers are not consulted at all.
    #[test]
    fn never_mode_returns_none_without_consulting_providers() {
        let (cache, config, temp) = test_env();
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();
        let binary = ResolvedBinary {
            krate: test_downloaded_crate().resolved,
            provider: BinaryProvider::GithubReleases,
            path: src,
        };
        let (resolver, calls) = resolver_with(
            cache,
            config,
            UsePrebuiltBinaries::Never,
            StubOutcome::Found(binary),
        );

        let result = resolver
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();

        assert_eq!(result, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A binary a provider resolves is relocated into `bin_dir` and returned in `always` mode.
    #[test]
    fn resolved_binary_relocated_and_returned_in_always_mode() {
        let (cache, config, temp) = test_env();
        let bin_dir = config.bin_dir.clone();
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();
        let binary = ResolvedBinary {
            krate: test_downloaded_crate().resolved,
            provider: BinaryProvider::GithubReleases,
            path: src.clone(),
        };
        let (resolver, _calls) = resolver_with(
            cache,
            config,
            UsePrebuiltBinaries::Always,
            StubOutcome::Found(binary),
        );

        let result = resolver
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap()
            .unwrap();

        assert_eq!(result.provider, BinaryProvider::GithubReleases);
        assert!(result.path.exists(), "relocated binary should exist");
        assert!(
            result.path.starts_with(&bin_dir),
            "binary should be relocated under bin_dir"
        );
        assert_ne!(result.path, src);
    }

    /// A transient (inconclusive) resolution must NOT be cached as a negative, so a
    /// later run re-consults providers again.
    #[test]
    fn inconclusive_result_is_not_cached() {
        let (cache, config, _temp) = test_env();
        let (resolver, calls) = resolver_with(
            cache.clone(),
            config,
            UsePrebuiltBinaries::Auto,
            StubOutcome::Inconclusive,
        );

        let result = resolver
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();

        assert_eq!(result, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_matches!(
            cache.get_cached_binary(&test_downloaded_crate().resolved),
            Ok(None),
            "an inconclusive resolution must not be persisted"
        );
    }

    /// A conclusive absence IS cached (as a negative entry), so later runs skip the providers.
    #[test]
    fn nonexistent_result_is_cached() {
        let (cache, config, _temp) = test_env();
        let (resolver, _calls) = resolver_with(
            cache.clone(),
            config,
            UsePrebuiltBinaries::Auto,
            StubOutcome::Nonexistent,
        );

        resolver
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();

        assert_matches!(
            cache.get_cached_binary(&test_downloaded_crate().resolved),
            Ok(Some(BinaryCacheEntry::Nonexistent))
        );
    }

    /// In `always` mode an inconclusive resolution is a hard error carrying the transient source,
    /// and is never cached.
    #[test]
    fn always_mode_errors_on_inconclusive_resolution() {
        let (cache, config, _temp) = test_env();
        let (resolver, calls) = resolver_with(
            cache.clone(),
            config,
            UsePrebuiltBinaries::Always,
            StubOutcome::Inconclusive,
        );

        let result = resolver.resolve(&test_downloaded_crate(), &BuildOptions::default());

        assert_matches!(
            result,
            Err(Error::PrebuiltBinaryResolutionFailed { ref name, .. }) if name == "serde"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_matches!(
            cache.get_cached_binary(&test_downloaded_crate().resolved),
            Ok(None)
        );
    }
}
