mod providers;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use providers::{BinstallProvider, GithubProvider, GitlabProvider, Provider, QuickinstallProvider};
use serde::{Deserialize, Serialize};
use snafu::{IntoError, ResultExt};

use crate::{
    Result,
    builder::{BuildOptions, BuildTarget},
    cache::Cache,
    config::{BinaryProvider, Config, UsePrebuiltBinaries},
    crate_resolver::ResolvedCrate,
    downloader::DownloadedCrate,
    error::{self, Error},
    http::HttpClient,
    messages::{MessageReporter, PrebuiltBinaryMessage, ProviderChangeReason},
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

/// A conclusive provider outcome: a binary was found, or this provider determined
/// no binary is available.
///
/// The two variants distinguish a positive result (a pre-built binary was resolved) from a
/// conclusive negative (we determined no pre-built binary is available).  Theoretically it's
/// possible that we could have checked at exactly the moment when a crate was just published but
/// artifacts not yet released, or that a maintainer could go back and publish artifacts for older
/// versions long after release, but both of these are highly unlikely.  By caching this result, we
/// can speed up subsequent runs and avoid the many network requests (and potential throttling)
/// that would be required to check for a pre-built binary every time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "only a handful of these exist at a time (one per resolved crate); the size disparity between \
              Found and Nonexistent does not matter and boxing would only add indirection"
)]
pub(crate) enum ConclusiveResolution {
    /// A pre-built binary was resolved.
    Found(ResolvedBinary),
    /// We conclusively determined that no pre-built binary is available.
    Nonexistent,
}

/// The persisted form of a binary-resolution outcome: a [`ConclusiveResolution`] paired with the
/// set of binary providers that were enabled when it was produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BinaryCacheEntry {
    #[serde(flatten)]
    pub(crate) outcome: ConclusiveResolution,
    /// The binary providers that were enabled for the resolution that produced [`Self::outcome`].
    pub(crate) enabled_providers: Vec<BinaryProvider>,
}

/// Create the default [`BinaryResolver`] implementation, respecting the given config and using the
/// provided cache.
pub(crate) fn create_resolver(
    config: Config,
    cache: Cache,
    reporter: MessageReporter,
    http_client: HttpClient,
) -> impl BinaryResolver {
    DefaultBinaryResolver::new(config, cache, reporter, http_client)
}

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

impl BinaryResolution {
    /// Map this binary resolution to the [`ConclusiveResolution`] (if any) that should be written
    /// to the binary cache to record this resolution for future runs.
    ///
    /// `Found`/`Nonexistent` are conclusive and cacheable; `Inconclusive` returns `None` and is
    /// never cached. This is the core invariant that prevents a transient failure from being
    /// persisted as a negative.
    fn to_cacheable(&self) -> Option<ConclusiveResolution> {
        match self {
            BinaryResolution::Found(binary) => Some(ConclusiveResolution::Found(binary.clone())),
            BinaryResolution::Nonexistent => Some(ConclusiveResolution::Nonexistent),
            BinaryResolution::Inconclusive { .. } => None,
        }
    }
}

impl From<ConclusiveResolution> for BinaryResolution {
    fn from(value: ConclusiveResolution) -> Self {
        match value {
            ConclusiveResolution::Found(binary) => Self::Found(binary),
            ConclusiveResolution::Nonexistent => Self::Nonexistent,
        }
    }
}

/// The prod [`BinaryResolver`] implementation, which delegates to the configured providers and
/// integrates with the cache to avoid repeated expensive provider calls.
struct DefaultBinaryResolver {
    config: Config,
    cache: Cache,
    reporter: MessageReporter,
    mode: UsePrebuiltBinaries,
    providers: Vec<Box<dyn Provider + Send + Sync>>,
}

impl DefaultBinaryResolver {
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
    fn combine_resolutions(resolutions: impl IntoIterator<Item = BinaryResolution>) -> BinaryResolution {
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

    /// Look up a cached binary-resolution outcome for `krate`, honoring it only if it is still
    /// valid for the config.
    ///
    /// Each cached entry records the providers that were enabled when it was written. A cached
    /// outcome is used only when:
    ///
    /// - every currently-enabled provider was among those recorded. A newly-enabled provider that
    ///   wasn't accounted for could change the outcome (a negative might become a positive, or a
    ///   higher-precedence provider might now win), so the entry is stale and we should resolve the
    ///   binary anew with all currently enabled providers. An entry that recorded *more* providers
    ///   than are currently enabled stays valid.
    /// - (positive entries only) the provider that produced the cached binary is still enabled. A
    ///   binary must never be served from a provider the user has disabled.
    ///
    /// Returns `None` (so resolution falls through to the providers) when there is no entry or it
    /// is stale for the current configuration.
    fn get_cached_resolution(&self, krate: &ResolvedCrate) -> Option<ConclusiveResolution> {
        let entry = self.cache.get_cached_binary(krate).ok()??;
        let enabled = &self.config.prebuilt_binaries.binary_providers;

        // If a provider that is enabled now was not enabled when this cache entry was written, then we
        // consider the cache entry stale and must not use it.
        if let Some(missing) = enabled.iter().find(|p| !entry.enabled_providers.contains(p)) {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::cache_invalidated_by_provider_change(
                    krate,
                    ProviderChangeReason::RequiredProviderNotEnabled(*missing),
                )
            });
            return None;
        }

        // If the cached entry is a positive hit but it uses a provider that the user has currently
        // disabled, then obviously we will not use that binary and the cache entry is considered stale.
        if let ConclusiveResolution::Found(binary) = &entry.outcome {
            if !enabled.contains(&binary.provider) {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::cache_invalidated_by_provider_change(
                        krate,
                        ProviderChangeReason::SourceProviderDisabled(binary.provider),
                    )
                });
                return None;
            }
        }

        match &entry.outcome {
            ConclusiveResolution::Found(binary) => self
                .reporter
                .report(|| PrebuiltBinaryMessage::positive_cache_hit(krate, &binary.path, binary.provider)),
            ConclusiveResolution::Nonexistent => self
                .reporter
                .report(|| PrebuiltBinaryMessage::negative_cache_hit(krate)),
        }

        Some(entry.outcome)
    }

    /// Consult each configured provider in order, short-circuiting on the first `Found`, and fold
    /// the results with [`Self::combine_resolutions`].
    fn resolve_via_providers(&self, krate: &DownloadedCrate, platform: &str) -> Result<BinaryResolution> {
        if self.providers.is_empty() {
            return error::NoProvidersConfiguredSnafu.fail();
        }

        let resolved = &krate.resolved;
        let mut results = Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            self.reporter
                .report(|| PrebuiltBinaryMessage::checking_provider(resolved, provider.kind()));

            // Invoke the provider, and based on the outcome construct the BinaryResolution result.
            //
            // If a single provide fails with any kind of error, we consider that an inconclusive
            // result.  Under normal circumstances, providers should not be failing; if the
            // provider determines the binary isn't found that should be an Ok response.  However,
            // the providers do a lot of fallible things, including making API calls that can be
            // throttled, parsing formats that could be invalid, etc.
            //
            // If a provider fails for a given crate input, that is likely a bug in the provider
            // code somewhere, but such a bug should not cause the binary resolution process itself
            // to return an error, nor should it result in a permanent negative cache entry.
            let resolution = match provider.try_resolve(krate, platform) {
                Ok(resolution) => BinaryResolution::from(resolution),
                Err(source) => BinaryResolution::Inconclusive {
                    source: Box::new(source),
                },
            };
            let found = matches!(resolution, BinaryResolution::Found(_));
            results.push(resolution);
            if found {
                break;
            }
        }

        Ok(Self::combine_resolutions(results))
    }

    /// Relocate a resolved binary from the provider's internal path to the `bin_dir` structure.
    ///
    /// Pre-built binaries are copied into `bin_dir` so the path cgx returns is stable and separate
    /// from provider-specific download/cache directories.
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

impl BinaryResolver for DefaultBinaryResolver {
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
            if let Some(cached) = self.get_cached_resolution(resolved_krate) {
                let resolution = match cached {
                    ConclusiveResolution::Found(binary) => BinaryResolution::Found(binary),
                    ConclusiveResolution::Nonexistent => {
                        // `cached_resolution` already reported the negative hit.  There is no reason
                        // to try to find a prebuilt binary again.
                        self.reporter.report(|| {
                            PrebuiltBinaryMessage::no_binary_found(
                                resolved_krate,
                                vec!["negative cache hit - no binary available".to_string()],
                            )
                        });
                        BinaryResolution::Nonexistent
                    }
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

        // Persist only conclusive outcomes; an inconclusive result is structurally uncacheable. The
        // entry records the providers enabled for this resolution so a future run can tell whether the
        // cached answer still applies (see [`Self::cached_resolution`]).
        if let Some(outcome) = resolution.to_cacheable() {
            let entry = BinaryCacheEntry {
                outcome,
                enabled_providers: self.config.prebuilt_binaries.binary_providers.clone(),
            };
            self.cache.put_cached_binary(resolved_krate, entry)?;
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

    /// The canned outcome a [`StubProvider`] returns.
    #[expect(
        clippy::large_enum_variant,
        reason = "test stub; at most one instance exists per test, so the size disparity is irrelevant"
    )]
    enum StubOutcome {
        Found(ResolvedBinary),
        Nonexistent,
        Error,
    }

    /// A [`Provider`] standing in for a real provider, returning a canned provider result and
    /// counting how often it is consulted (so tests can assert short-circuiting and cache hits).
    struct StubProvider {
        outcome: StubOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl StubProvider {
        /// A `Found` stub whose binary's `provider` (and therefore the cached finder) is
        /// `provider`, with a real on-disk file at `path` so relocation succeeds.
        fn found(provider: BinaryProvider, path: PathBuf) -> Self {
            Self {
                outcome: StubOutcome::Found(ResolvedBinary {
                    krate: test_downloaded_crate().resolved,
                    provider,
                    path,
                }),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn nonexistent() -> Self {
            Self {
                outcome: StubOutcome::Nonexistent,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn error() -> Self {
            Self {
                outcome: StubOutcome::Error,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Provider for StubProvider {
        fn kind(&self) -> BinaryProvider {
            BinaryProvider::GithubReleases
        }

        fn try_resolve(&self, _krate: &DownloadedCrate, _platform: &str) -> Result<ConclusiveResolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.outcome {
                StubOutcome::Found(binary) => Ok(ConclusiveResolution::Found(binary.clone())),
                StubOutcome::Nonexistent => Ok(ConclusiveResolution::Nonexistent),
                StubOutcome::Error => Err(transient_error()),
            }
        }
    }

    /// A transient-looking HTTP 429 error, standing in for a rate limit error / network glitch.
    fn transient_error() -> Error {
        error::HttpStatusSnafu {
            url: "https://api.github.com/repos/x/y/releases/tags/v1.0.0".to_string(),
            status: 429u16,
        }
        .build()
    }

    fn boxed_transient() -> Box<Error> {
        Box::new(transient_error())
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
    ) -> (DefaultBinaryResolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut config = config;
        config.prebuilt_binaries.use_prebuilt_binaries = mode;
        let providers: Vec<Box<dyn Provider + Send + Sync>> = vec![Box::new(StubProvider {
            outcome,
            calls: calls.clone(),
        })];
        (
            DefaultBinaryResolver::with_providers(config, cache, MessageReporter::null(), providers),
            calls,
        )
    }

    /// Build an `auto`-mode resolver whose enabled provider list (the set recorded in / checked
    /// against the cache) is `enabled`, backed by the given stub `providers` (which only supply
    /// outcomes; their `kind()` is irrelevant to the cache's provider-set checks).
    fn resolver_with_enabled_providers(
        cache: Cache,
        config: Config,
        enabled: Vec<BinaryProvider>,
        providers: Vec<Box<dyn Provider + Send + Sync>>,
    ) -> DefaultBinaryResolver {
        let mut config = config;
        config.prebuilt_binaries.use_prebuilt_binaries = UsePrebuiltBinaries::Auto;
        config.prebuilt_binaries.binary_providers = enabled;
        DefaultBinaryResolver::with_providers(config, cache, MessageReporter::null(), providers)
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

    /// Test that default build options are not disqualified
    #[test]
    fn test_disqualification_default_options_ok() {
        let options = BuildOptions::default();
        assert_eq!(DefaultBinaryResolver::is_disqualified(&options), None);
    }

    /// Test that explicit --bin flag disqualifies pre-built binaries
    #[test]
    fn test_disqualification_explicit_bin() {
        let options = BuildOptions {
            build_target: BuildTarget::Bin("specific-bin".to_string()),
            ..Default::default()
        };
        assert_eq!(
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
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
            DefaultBinaryResolver::is_disqualified(&options),
            Some("custom toolchain specified")
        );
    }
    #[test]
    fn combine_empty_is_nonexistent() {
        assert_matches!(
            DefaultBinaryResolver::combine_resolutions(Vec::<BinaryResolution>::new()),
            BinaryResolution::Nonexistent
        );
    }

    #[test]
    fn combine_all_nonexistent_is_nonexistent() {
        let combined = DefaultBinaryResolver::combine_resolutions([
            BinaryResolution::Nonexistent,
            BinaryResolution::Nonexistent,
        ]);
        assert_matches!(combined, BinaryResolution::Nonexistent);
    }

    #[test]
    fn combine_any_found_wins() {
        let combined = DefaultBinaryResolver::combine_resolutions([
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
        let combined = DefaultBinaryResolver::combine_resolutions([
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
            BinaryResolution::Found(test_resolved_binary()).to_cacheable(),
            Some(ConclusiveResolution::Found(_))
        );
        assert_matches!(
            BinaryResolution::Nonexistent.to_cacheable(),
            Some(ConclusiveResolution::Nonexistent)
        );
        assert_matches!(
            BinaryResolution::Inconclusive {
                source: boxed_transient()
            }
            .to_cacheable(),
            None
        );
    }

    #[test]
    fn apply_mode_found_returns_binary_in_any_mode() {
        let resolved = test_downloaded_crate().resolved;
        for mode in [UsePrebuiltBinaries::Auto, UsePrebuiltBinaries::Always] {
            let out = DefaultBinaryResolver::apply_mode(
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
            DefaultBinaryResolver::apply_mode(
                BinaryResolution::Nonexistent,
                UsePrebuiltBinaries::Auto,
                &resolved
            ),
            Ok(None)
        );
        assert_matches!(
            DefaultBinaryResolver::apply_mode(
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
            DefaultBinaryResolver::apply_mode(
                BinaryResolution::Inconclusive {
                    source: boxed_transient()
                },
                UsePrebuiltBinaries::Auto,
                &resolved
            ),
            Ok(None)
        );
        let err = DefaultBinaryResolver::apply_mode(
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
            StubOutcome::Error,
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

    #[test]
    fn auto_mode_continues_after_provider_error_and_returns_later_found() {
        let (cache, config, temp) = test_env();
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();

        let first = StubProvider::error();
        let first_calls = first.calls.clone();
        let second = StubProvider::found(BinaryProvider::GithubReleases, src);
        let second_calls = second.calls.clone();
        let resolver = resolver_with_enabled_providers(
            cache,
            config,
            vec![BinaryProvider::GitlabReleases, BinaryProvider::GithubReleases],
            vec![Box::new(first), Box::new(second)],
        );

        let result = resolver
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap()
            .unwrap();

        assert_eq!(result.provider, BinaryProvider::GithubReleases);
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
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
            Ok(Some(BinaryCacheEntry {
                outcome: ConclusiveResolution::Nonexistent,
                ..
            }))
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
            StubOutcome::Error,
        );

        let result = resolver.resolve(&test_downloaded_crate(), &BuildOptions::default());

        assert_matches!(
            result,
            Err(Error::PrebuiltBinaryResolutionFailed { ref name, ref source, .. })
                if name == "serde" && matches!(source.as_ref(), Error::HttpStatus { status: 429, .. })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_matches!(
            cache.get_cached_binary(&test_downloaded_crate().resolved),
            Ok(None)
        );
    }

    /// The reported bug: a negative result cached when only GitLab was enabled must be re-resolved
    /// once GitHub is also enabled, and GitHub (which has the binary) must actually be consulted.
    #[test]
    fn negative_cache_invalidated_when_new_provider_enabled() {
        let (cache, config, temp) = test_env();

        // Phase 1: only GitLab enabled, no binary -> caches Nonexistent with providers = [GitLab].
        let gitlab1 = StubProvider::nonexistent();
        let gitlab1_calls = gitlab1.calls.clone();
        let r1 = resolver_with_enabled_providers(
            cache.clone(),
            config.clone(),
            vec![BinaryProvider::GitlabReleases],
            vec![Box::new(gitlab1)],
        );
        assert_matches!(
            r1.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(None)
        );
        assert_eq!(gitlab1_calls.load(Ordering::SeqCst), 1);

        // Phase 2: GitLab + GitHub enabled, and GitHub has a binary.
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();
        let gitlab2 = StubProvider::nonexistent();
        let github = StubProvider::found(BinaryProvider::GithubReleases, src);
        let github_calls = github.calls.clone();
        let r2 = resolver_with_enabled_providers(
            cache.clone(),
            config,
            vec![BinaryProvider::GitlabReleases, BinaryProvider::GithubReleases],
            vec![Box::new(gitlab2), Box::new(github)],
        );

        let result = r2
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();

        assert_matches!(result, Some(_));
        assert!(
            github_calls.load(Ordering::SeqCst) >= 1,
            "GitHub must be consulted once the stale negative entry is invalidated"
        );
    }

    /// An identical provider set across runs must NOT invalidate: the second run is a cache hit and
    /// the providers are not consulted again.
    #[test]
    fn identical_provider_set_is_cache_hit() {
        let (cache, config, _temp) = test_env();
        let enabled = vec![BinaryProvider::GitlabReleases, BinaryProvider::GithubReleases];

        let gl1 = StubProvider::nonexistent();
        let gh1 = StubProvider::nonexistent();
        let (gl1_calls, gh1_calls) = (gl1.calls.clone(), gh1.calls.clone());
        let r1 = resolver_with_enabled_providers(
            cache.clone(),
            config.clone(),
            enabled.clone(),
            vec![Box::new(gl1), Box::new(gh1)],
        );
        assert_matches!(
            r1.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(None)
        );
        assert_eq!(gl1_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gh1_calls.load(Ordering::SeqCst), 1);

        let gl2 = StubProvider::nonexistent();
        let gh2 = StubProvider::nonexistent();
        let (gl2_calls, gh2_calls) = (gl2.calls.clone(), gh2.calls.clone());
        let r2 = resolver_with_enabled_providers(cache, config, enabled, vec![Box::new(gl2), Box::new(gh2)]);
        assert_matches!(
            r2.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(None)
        );
        assert_eq!(
            gl2_calls.load(Ordering::SeqCst),
            0,
            "cache hit must not re-consult providers"
        );
        assert_eq!(
            gh2_calls.load(Ordering::SeqCst),
            0,
            "cache hit must not re-consult providers"
        );
    }

    /// Removing a provider that did NOT produce the cached binary keeps the positive entry valid
    /// (coverage still holds and the finder is still enabled): cache hit, no re-consultation.
    #[test]
    fn removing_non_finder_provider_keeps_positive_entry() {
        let (cache, config, temp) = test_env();
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();

        // Phase 1: GitHub (the finder) + Quickinstall enabled; GitHub is first and is found.
        let github = StubProvider::found(BinaryProvider::GithubReleases, src);
        let quick = StubProvider::nonexistent();
        let r1 = resolver_with_enabled_providers(
            cache.clone(),
            config.clone(),
            vec![BinaryProvider::GithubReleases, BinaryProvider::Quickinstall],
            vec![Box::new(github), Box::new(quick)],
        );
        assert_matches!(
            r1.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(Some(_))
        );

        // Phase 2: drop the non-finder Quickinstall; GitHub remains enabled -> still a cache hit.
        let github2 = StubProvider::nonexistent();
        let github2_calls = github2.calls.clone();
        let r2 = resolver_with_enabled_providers(
            cache,
            config,
            vec![BinaryProvider::GithubReleases],
            vec![Box::new(github2)],
        );
        let result = r2
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();
        assert_matches!(result, Some(_));
        assert_eq!(
            github2_calls.load(Ordering::SeqCst),
            0,
            "a still-valid positive entry must not re-consult providers"
        );
    }

    /// Disabling the exact provider that produced the cached binary invalidates the positive entry:
    /// the binary must not be served, and the crate is re-resolved.
    #[test]
    fn disabling_finder_invalidates_positive_entry() {
        let (cache, config, temp) = test_env();
        let src = temp.path().join("serde");
        std::fs::write(&src, b"binary").unwrap();

        // Phase 1: GitLab + GitHub enabled; GitHub (second) is found, so the finder is GitHub.
        let gitlab = StubProvider::nonexistent();
        let github = StubProvider::found(BinaryProvider::GithubReleases, src);
        let r1 = resolver_with_enabled_providers(
            cache.clone(),
            config.clone(),
            vec![BinaryProvider::GitlabReleases, BinaryProvider::GithubReleases],
            vec![Box::new(gitlab), Box::new(github)],
        );
        assert_matches!(
            r1.resolve(&test_downloaded_crate(), &BuildOptions::default()),
            Ok(Some(_))
        );

        // Phase 2: GitHub (the finder) disabled, only GitLab enabled (which has no binary).
        let gitlab2 = StubProvider::nonexistent();
        let gitlab2_calls = gitlab2.calls.clone();
        let r2 = resolver_with_enabled_providers(
            cache,
            config,
            vec![BinaryProvider::GitlabReleases],
            vec![Box::new(gitlab2)],
        );
        let result = r2
            .resolve(&test_downloaded_crate(), &BuildOptions::default())
            .unwrap();
        assert_eq!(
            result, None,
            "a binary from a now-disabled provider must not be served"
        );
        assert_eq!(
            gitlab2_calls.load(Ordering::SeqCst),
            1,
            "must re-resolve once the finder is disabled"
        );
    }
}
