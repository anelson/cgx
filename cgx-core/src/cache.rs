use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snafu::ResultExt;
use tracing::*;

use crate::{
    Result,
    bin_resolver::BinaryCacheEntry,
    builder::{BuildOptions, BuildTarget},
    config::Config,
    crate_resolver::{ResolvedCrate, ResolvedSource},
    cratespec::{CrateSpec, Forge, RegistrySource},
    downloader::DownloadedCrate,
    error,
    messages::{BuildCacheMessage, CrateResolutionMessage, PrebuiltBinaryMessage, SourceMessage},
    target::TargetTriple,
};

/// A cache entry wrapping a value with timestamp metadata.
///
/// This generic wrapper is used for any cached data that has an expiration policy.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct CacheEntry<T> {
    #[serde(flatten)]
    value: T,
    cached_at: DateTime<Utc>,
}

impl<T> CacheEntry<T> {
    /// Create a new cache entry with the current timestamp.
    fn new(value: T) -> Self {
        Self {
            value,
            cached_at: Utc::now(),
        }
    }

    /// Get the age of this cache entry as a [`Duration`].
    fn age(&self) -> Duration {
        Utc::now()
            .signed_duration_since(self.cached_at)
            .to_std()
            .unwrap_or(Duration::ZERO)
    }

    /// Consume this cache entry and get at the inner value.
    fn into_inner(self) -> T {
        self.value
    }
}

/// A cache entry for a resolved crate specification.
type CrateResolveCacheEntry = CacheEntry<ResolvedCrate>;

/// Manages the various caches that cgx uses to operate.
///
/// The root of the caches is controlled by [`Config::cache_dir`].  Below that are multiple
/// subdirectories for caching various things:
/// - Results of crate spec resolution
/// - Downloaded/extracted crate source code packages
/// - Git database (bare repos)
/// - Git checkouts at specific commits
///
/// More may be added over time.
#[derive(Clone, Debug)]
pub(crate) struct Cache {
    inner: Arc<CacheInner>,
}

impl Cache {
    /// Create a new [`Cache`] with the given configuration and message reporter.
    pub(crate) fn new(config: Config, reporter: crate::messages::MessageReporter) -> Self {
        Self {
            inner: Arc::new(CacheInner { config, reporter }),
        }
    }

    /// Get a cached crate resolution, or resolve it using the provided resolver function.
    ///
    /// This method implements the full caching strategy:
    /// - If a non-expired cache entry exists, return it without calling the resolver
    /// - Call the resolver function to compute a fresh value
    /// - On success, cache the result and return it
    /// - On transient errors (network/IO), fall back to stale cache if available
    /// - On permanent errors, propagate without using stale cache
    pub(crate) fn get_or_resolve_crate<F>(&self, spec: &CrateSpec, resolver: F) -> Result<ResolvedCrate>
    where
        F: FnOnce() -> Result<ResolvedCrate>,
    {
        self.inner
            .reporter
            .report(|| CrateResolutionMessage::cache_lookup(spec));

        let stale_entry = if !self.inner.config.refresh {
            if let Ok(Some(entry)) = self.get_resolved_crate(spec) {
                let age = entry.age();
                let ttl = self.inner.config.resolve_cache_timeout;

                if age < ttl {
                    let cache_path = self.crate_resolve_cache_path(spec).ok();
                    if let Some(path) = &cache_path {
                        self.inner
                            .reporter
                            .report(|| CrateResolutionMessage::cache_hit(path, age, ttl.saturating_sub(age)));
                    }
                    self.inner
                        .reporter
                        .report(|| CrateResolutionMessage::resolved(&entry.value));
                    return Ok(entry.value);
                }

                self.inner
                    .reporter
                    .report(|| CrateResolutionMessage::cache_stale(spec, age));
                Some(entry)
            } else {
                self.inner
                    .reporter
                    .report(|| CrateResolutionMessage::cache_miss(spec));
                None
            }
        } else {
            self.inner
                .reporter
                .report(|| CrateResolutionMessage::cache_miss(spec));
            None
        };

        self.inner
            .reporter
            .report(|| CrateResolutionMessage::resolving(spec));

        match resolver() {
            Ok(resolved) => {
                self.inner
                    .reporter
                    .report(|| CrateResolutionMessage::resolved(&resolved));
                if let Ok(path) = self.crate_resolve_cache_path(spec) {
                    let _ = self.put_resolved_crate(spec, &resolved);
                    self.inner
                        .reporter
                        .report(|| CrateResolutionMessage::cache_stored(&path));
                } else {
                    let _ = self.put_resolved_crate(spec, &resolved);
                }
                Ok(resolved)
            }
            Err(e) if !self.inner.config.refresh && Self::should_use_stale_cache(&e) => {
                // If there was already an entry in the cache, but we didn't use it because it was
                // stale, return it now as a fallback since a stale cache entry is better than
                // failing with this error
                if let Some(entry) = stale_entry {
                    let age = entry.age();
                    let resolved = entry.into_inner();
                    self.inner
                        .reporter
                        .report(|| CrateResolutionMessage::using_stale_fallback(spec, age));
                    self.inner
                        .reporter
                        .report(|| CrateResolutionMessage::resolved(&resolved));
                    Ok(resolved)
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Get a cached crate source code package, or download it using the provided downloader
    /// function.
    ///
    /// This method implements transactional caching for source downloads:
    /// 1. If the source is already cached, return it without calling the downloader
    /// 2. Create a temporary directory for the download
    /// 3. Call the downloader function with the temp directory path
    /// 4. On success, atomically rename the temp directory to the cache location
    /// 5. Handle race conditions where multiple processes download simultaneously
    pub(crate) fn get_or_download_crate<F>(
        &self,
        resolved: &ResolvedCrate,
        downloader: F,
    ) -> Result<DownloadedCrate>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        self.inner
            .reporter
            .report(|| SourceMessage::cache_lookup(resolved));

        // Compute the target cache path
        let cache_path = self.crate_source_cache_path(resolved)?;

        // Check if already cached
        if !self.inner.config.refresh {
            if let Ok(Some(cached)) = self.get_cached_crate_source(resolved) {
                self.inner
                    .reporter
                    .report(|| SourceMessage::cache_hit(&cached.crate_path));
                return Ok(cached);
            }
        } else {
            // When refresh is enabled, delete any existing cache to ensure a fresh download
            if cache_path.exists() {
                debug!(
                    "Refresh mode: removing existing source cache at {}",
                    cache_path.display()
                );
                let _ = fs::remove_dir_all(&cache_path);
            }
        }

        self.inner.reporter.report(|| SourceMessage::cache_miss(resolved));

        self.inner
            .reporter
            .report(|| SourceMessage::downloading(resolved));

        // Ensure parent directory exists
        let parent = cache_path
            .parent()
            .expect("BUG: cache_path is built by joining onto the cache root, so it always has a parent");
        fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
            path: parent.to_path_buf(),
        })?;

        // Create a temp directory in the same parent directory for atomic rename
        let temp_dir = tempfile::tempdir_in(parent).with_context(|_| error::TempDirInCreationSnafu {
            parent: parent.to_path_buf(),
        })?;

        // Call the downloader with the temp path
        downloader(temp_dir.path())?;
        self.inner
            .reporter
            .report(|| SourceMessage::downloaded(temp_dir.path()));

        // Success! Try to atomically move the temp dir to the cache location
        // Use keep() to prevent temp_dir cleanup
        let temp_path = temp_dir.keep();

        match fs::rename(&temp_path, &cache_path) {
            Ok(()) => {
                self.inner
                    .reporter
                    .report(|| SourceMessage::cache_stored(&cache_path));
                // Successfully moved to cache
                Ok(DownloadedCrate {
                    resolved: resolved.clone(),
                    crate_path: cache_path,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Someone else won the race - that's fine, use their result
                // Clean up our temp dir
                let _ = fs::remove_dir_all(&temp_path);
                Ok(DownloadedCrate {
                    resolved: resolved.clone(),
                    crate_path: cache_path,
                })
            }
            Err(e) => {
                // Some other error during rename - clean up and propagate
                let _ = fs::remove_dir_all(&temp_path);
                Err(e).with_context(|_| error::RenameFileSnafu {
                    src: temp_path.clone(),
                    dst: cache_path.clone(),
                })
            }
        }
    }

    /// Get a cached resolution for the given [`CrateSpec`], if one exists.
    ///
    /// Returns `None` if there is no cached entry or if reading the cache fails.
    fn get_resolved_crate(&self, spec: &CrateSpec) -> Result<Option<CacheEntry<ResolvedCrate>>> {
        let cache_file = self.crate_resolve_cache_path(spec)?;
        if !cache_file.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&cache_file).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;
        let entry: CrateResolveCacheEntry = serde_json::from_str(&contents).context(error::JsonSnafu)?;

        Ok(Some(entry))
    }

    /// Store a resolved crate in the cache for the given [`CrateSpec`].
    fn put_resolved_crate(&self, spec: &CrateSpec, resolved: &ResolvedCrate) -> Result<()> {
        let cache_file = self.crate_resolve_cache_path(spec)?;

        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        let entry = CacheEntry::new(resolved.clone());

        let json = serde_json::to_string_pretty(&entry).context(error::JsonSnafu)?;
        fs::write(&cache_file, json).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;

        Ok(())
    }

    /// Get the cached binary resolution outcome for the given [`ResolvedCrate`], if one exists.
    ///
    /// Binary resolution results never expire because crates are immutable. Once we determine
    /// whether a binary exists for a specific version on a specific platform, that answer remains
    /// valid forever. We cache both positive (binary found) and negative (no binary) results to
    /// avoid repeatedly checking providers.
    ///
    /// Unlike crate resolution, there is no TTL check - the cache entry is permanent.
    ///
    /// Returns `Ok(None)` when there is no cache entry for the crate, or `Ok(Some(entry))` carrying
    /// the cached resolution outcome.
    pub(crate) fn get_cached_binary(&self, krate: &ResolvedCrate) -> Result<Option<BinaryCacheEntry>> {
        self.inner
            .reporter
            .report(|| PrebuiltBinaryMessage::cache_lookup(krate));

        let entry = self.read_binary_cache_entry(krate);
        if entry.is_none() {
            self.inner
                .reporter
                .report(|| PrebuiltBinaryMessage::cache_miss(krate));
        }

        Ok(entry)
    }

    /// Store a binary-resolution cache entry for the given [`ResolvedCrate`].
    pub(crate) fn put_cached_binary(&self, krate: &ResolvedCrate, entry: BinaryCacheEntry) -> Result<()> {
        let cache_file = self.binary_cache_path(krate)?;

        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        let entry = CacheEntry::new(entry);

        let json = serde_json::to_string_pretty(&entry).context(error::JsonSnafu)?;
        fs::write(&cache_file, json).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;

        self.inner
            .reporter
            .report(|| PrebuiltBinaryMessage::cache_stored(&cache_file));

        Ok(())
    }

    /// Read and deserialize the binary cache entry for the crate `krate`, returning `None` if the
    /// cache entry is absent or unreadable.
    ///
    /// A corrupt or stale-format entry is treated as a miss (it will be re-resolved and
    /// overwritten).
    #[instrument(skip_all, fields(krate = %krate.name, version = %krate.version))]
    fn read_binary_cache_entry(&self, krate: &ResolvedCrate) -> Option<BinaryCacheEntry> {
        let cache_file = match self.binary_cache_path(krate) {
            Ok(cache_file) => cache_file,
            Err(e) => {
                debug!(
                    error = %e,
                    "failed to compute binary cache path; this should not ever happen"
                );
                return None;
            }
        };

        if !cache_file.exists() {
            return None;
        }

        let contents = match fs::read_to_string(&cache_file) {
            Ok(contents) => contents,
            Err(e) => {
                debug!(
                    path = %cache_file.display(),
                    error = %e,
                    "ignoring unreadable binary cache entry");
                return None;
            }
        };

        match serde_json::from_str::<CacheEntry<BinaryCacheEntry>>(&contents) {
            Ok(entry) => Some(entry.into_inner()),
            Err(e) => {
                debug!(path = %cache_file.display(),
                    error = %e,
                    "ignoring unparsable binary cache entry");
                None
            }
        }
    }

    /// Get the filesystem path for the binary resolution cache file for a given [`ResolvedCrate`].
    ///
    /// The cache key includes the crate identity (name, version, source) and the current platform.
    /// This ensures that binaries are cached per-platform, which is essential since pre-built
    /// binaries are platform-specific.
    fn binary_cache_path(&self, krate: &ResolvedCrate) -> Result<PathBuf> {
        let hash = Self::compute_binary_cache_hash(krate, TargetTriple::host())?;
        Ok(self
            .inner
            .config
            .cache_dir
            .join("binaries")
            .join(format!("{}.json", hash)))
    }

    /// Compute a SHA256 hash for the binary cache key.
    ///
    /// The hash includes:
    /// - Crate name
    /// - Crate version
    /// - Resolved source (crates.io vs git vs forge, etc.)
    /// - The target platform triple
    ///
    /// `target` is a parameter so the hash is distinct and testable for a fixed platform. This
    /// ensures the same crate on different platforms gets different cache entries.
    fn compute_binary_cache_hash(krate: &ResolvedCrate, target: &TargetTriple) -> Result<String> {
        #[derive(Serialize)]
        struct BinaryCacheKey<'a> {
            name: &'a str,
            version: &'a semver::Version,
            source: &'a ResolvedSource,
            platform: &'a TargetTriple,
        }

        let key = BinaryCacheKey {
            name: &krate.name,
            version: &krate.version,
            source: &krate.source,
            platform: target,
        };

        let json = serde_json::to_string(&key).context(error::JsonSnafu)?;
        Ok(Self::compute_hash(json.as_bytes()))
    }

    /// Get the filesystem path for the resolve cache file for a given [`CrateSpec`].
    fn crate_resolve_cache_path(&self, spec: &CrateSpec) -> Result<PathBuf> {
        let hash = Self::compute_spec_hash(spec)?;
        Ok(self
            .inner
            .config
            .cache_dir
            .join("resolve")
            .join(format!("{}.json", hash)))
    }

    /// Compute a SHA256 hash of the serialized [`CrateSpec`] to use as a cache key.
    fn compute_spec_hash(spec: &CrateSpec) -> Result<String> {
        let json = serde_json::to_string(spec).context(error::JsonSnafu)?;
        Ok(Self::compute_hash(json.as_bytes()))
    }

    /// Compute a SHA256 hash of the given data.
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        crate::helpers::format_hex_lower(hasher.finalize())
    }

    /// Determine if an error should trigger fallback to stale cache.
    ///
    /// Network and I/O errors are considered transient and should use stale cache if available.
    /// Other errors (like version mismatches) are permanent and should not use stale cache.
    fn should_use_stale_cache(error: &error::Error) -> bool {
        matches!(
            error,
            error::Error::Registry { .. } | error::Error::Git { .. } | error::Error::Io { .. }
        )
    }

    /// Check if a resolved crate's source code package is already in the cache.
    fn get_cached_crate_source(&self, resolved: &ResolvedCrate) -> Result<Option<DownloadedCrate>> {
        let cache_path = self.crate_source_cache_path(resolved)?;

        if cache_path.exists() {
            Ok(Some(DownloadedCrate {
                resolved: resolved.clone(),
                crate_path: cache_path,
            }))
        } else {
            Ok(None)
        }
    }

    /// Get the cache directory path for a resolved crate's source code package.
    fn crate_source_cache_path(&self, resolved: &ResolvedCrate) -> Result<PathBuf> {
        let base = self.inner.config.cache_dir.join("sources");

        let path = match &resolved.source {
            ResolvedSource::CratesIo => base
                .join("crates-io")
                .join(&resolved.name)
                .join(resolved.version.to_string()),

            ResolvedSource::Registry { source } => match source {
                RegistrySource::Named(name) => base
                    .join("registry")
                    .join(name)
                    .join(&resolved.name)
                    .join(resolved.version.to_string()),

                RegistrySource::IndexUrl(url) => {
                    let url_hash = Self::compute_hash(url.as_str().as_bytes());
                    base.join("registry-index")
                        .join(url_hash)
                        .join(&resolved.name)
                        .join(resolved.version.to_string())
                }
            },

            ResolvedSource::Git { repo, commit } => {
                let repo_hash = Self::compute_hash(repo.as_bytes());
                base.join("git").join(repo_hash).join(commit)
            }

            ResolvedSource::Forge { forge, commit } => match forge {
                Forge::GitHub { owner, repo, .. } => base.join("github").join(owner).join(repo).join(commit),
                Forge::GitLab { owner, repo, .. } => base.join("gitlab").join(owner).join(repo).join(commit),
            },

            ResolvedSource::LocalDir { .. } => {
                unreachable!("BUG: LocalDir sources should not be passed to source_cache_path")
            }
        };

        Ok(path)
    }

    /// Get the cache path for a git database (bare repo) for a URL.
    pub(crate) fn git_db_path(&self, url: &str) -> PathBuf {
        let ident = Self::compute_git_ident(url);
        self.inner.config.cache_dir.join("git-db").join(ident)
    }

    /// Get the cache path for a git checkout at a specific commit.
    pub(crate) fn git_checkout_path(&self, url: &str, commit: &str) -> PathBuf {
        let ident = Self::compute_git_ident(url);
        self.inner
            .config
            .cache_dir
            .join("git-checkouts")
            .join(ident)
            .join(commit)
    }

    /// Compute stable identifier for git URL (like cargo's ident).
    ///
    /// Format: `{repo-name}-{short-hash}`
    /// Example: `tokio-a1b2c3d4` for `https://github.com/tokio-rs/tokio`
    fn compute_git_ident(url: &str) -> String {
        // Extract repo name from URL (last path component)
        let name = url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or("repo");

        // Short hash of full URL for uniqueness
        #[expect(
            clippy::string_slice,
            reason = "compute_hash returns a 64-char ASCII hex digest, so [..8] is in range and on a char \
                      boundary"
        )]
        let hash = &Self::compute_hash(url.as_bytes())[..8];

        format!("{}-{}", name, hash)
    }

    /// Test helper to manually insert a stale resolve cache entry.
    ///
    /// This allows tests to populate the cache with entries of a specific age,
    /// useful for testing stale cache behavior and offline mode.
    #[cfg(test)]
    pub(crate) fn insert_stale_resolve_entry(
        &self,
        spec: &CrateSpec,
        resolved: &ResolvedCrate,
        age: Duration,
    ) -> Result<()> {
        let cache_file = self.crate_resolve_cache_path(spec)?;

        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        let cached_at = Utc::now() - chrono::Duration::from_std(age).unwrap();
        let entry = CacheEntry {
            value: resolved.clone(),
            cached_at,
        };

        let json = serde_json::to_string_pretty(&entry).context(error::JsonSnafu)?;
        fs::write(&cache_file, json).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;

        Ok(())
    }

    /// Get a cached binary or build it if not present.
    ///
    /// This method implements binary caching with a cache key computed from both the
    /// crate identity and the build options. Local directory sources are never cached,
    /// as their source code can change arbitrarily.
    ///
    /// An SBOM (Software Bill of Materials) is stored alongside the binary
    /// for all cached sources, describing the dependencies and build configuration.
    ///
    /// # Arguments
    ///
    /// * `krate` - The resolved crate to build
    /// * `options` - Build options that affect the output binary
    /// * `build_fn` - Closure that builds the binary and returns both the binary path and the
    ///   generated SBOM
    ///
    /// # Returns
    ///
    /// The path to the binary, either from cache or freshly built.
    pub(crate) fn get_or_build_binary<F>(
        &self,
        krate: &ResolvedCrate,
        options: &BuildOptions,
        build_fn: F,
    ) -> Result<PathBuf>
    where
        F: FnOnce() -> Result<(PathBuf, crate::sbom::CycloneDx)>,
    {
        // Don't cache local directories - their source can change
        if matches!(krate.source, ResolvedSource::LocalDir { .. }) {
            self.inner
                .reporter
                .report(BuildCacheMessage::skipping_cache_local_dir);
            let (binary_path, _sbom) = build_fn()?;
            return Ok(binary_path);
        }

        self.inner
            .reporter
            .report(|| BuildCacheMessage::cache_lookup(krate, options));

        let source_hash = Self::compute_source_hash(&krate.source);
        let build_hash = Self::compute_build_hash(options);
        let binary_name = Self::expected_binary_name(&krate.name, &options.build_target);

        let cache_dir = self
            .inner
            .config
            .bin_dir
            .join(format!("{}-{}", krate.name, krate.version))
            .join(source_hash)
            .join(build_hash);

        let cache_path = cache_dir.join(&binary_name);
        let sbom_path = cache_dir.join("sbom.cyclonedx.json");

        // Return cached binary if it exists (SBOM is presumed to also exist in this case)
        if cache_path.exists() {
            if !self.inner.config.refresh {
                self.inner
                    .reporter
                    .report(|| BuildCacheMessage::cache_hit(&cache_path, &sbom_path));
                return Ok(cache_path);
            } else {
                debug!(
                    cache_dir = %cache_dir.display(),
                    "Refresh mode: removing existing binary cache",
                );
                let _ = fs::remove_dir_all(&cache_dir);
            }
        }

        self.inner
            .reporter
            .report(|| BuildCacheMessage::cache_miss(krate));

        // Build the binary and get the SBOM
        let (built_binary, sbom) = build_fn()?;

        // Create cache directory
        fs::create_dir_all(&cache_dir).with_context(|_| error::IoSnafu {
            path: cache_dir.clone(),
        })?;

        // Copy binary to cache
        fs::copy(&built_binary, &cache_path).with_context(|_| error::CopyBinarySnafu {
            src: built_binary.clone(),
            dst: cache_path.clone(),
        })?;

        // Serialize and write SBOM to cache
        let sbom_json = serde_json::to_string_pretty(&sbom).context(error::JsonSnafu)?;
        fs::write(&sbom_path, sbom_json).with_context(|_| error::IoSnafu {
            path: sbom_path.clone(),
        })?;

        self.inner
            .reporter
            .report(|| BuildCacheMessage::cache_stored(&cache_path, &sbom_path));

        Ok(cache_path)
    }

    /// Compute a hash of the resolved source to distinguish different crate origins.
    ///
    /// Different sources (crates.io vs git vs forge) will produce different hashes
    /// even for the same crate name and version.
    ///
    /// Uses SHA-256 over the source's JSON serialization, so the resulting build-cache paths are
    /// stable across toolchains and platforms (presuming, of course, that the JSON serialized
    /// representation of the source is itself stable across toolchains and platforms, which we
    /// hope that it is).
    fn compute_source_hash(source: &ResolvedSource) -> String {
        // It makes absolutely no sense to try to cache a local dir source!  Higher-level code
        // should have already bypassed the cache for this source.
        if matches!(source, ResolvedSource::LocalDir { .. }) {
            panic!("BUG: Should not compute a build-cache hash for LocalDir sources");
        }

        source.source_hash()
    }

    /// Compute a hash of build options that affect the output binary.
    ///
    /// Only options that actually change the binary output are included.
    /// Options like `offline`, `jobs`, and `ignore_rust_version` affect build
    /// behavior but not the resulting binary, so they're excluded.
    ///
    /// The `locked` flag DOES affect the binary because it affects dependency
    /// resolution - different dependency versions produce different binaries.
    ///
    /// Features are sorted before hashing to ensure consistent cache keys
    /// regardless of the order they're specified.
    fn compute_build_hash(options: &BuildOptions) -> String {
        // Sort features for consistency - order shouldn't matter for cache key
        let mut features = options.features.clone();
        features.sort();

        // Only the options that actually change the built binary are part of the key. `offline`,
        // `jobs`, and `ignore_rust_version` affect build behavior but not output and are excluded;
        // `locked` IS included because it affects dependency resolution (hence the binary).
        //
        // The key is serialized to JSON  and hashed with SHA-256 so build-cache paths are stable
        // across toolchains.
        #[derive(Serialize)]
        struct BuildCacheKey<'a> {
            features: &'a [String],
            all_features: bool,
            no_default_features: bool,
            profile: &'a Option<String>,
            target: &'a Option<TargetTriple>,
            build_target: &'a BuildTarget,
            toolchain: &'a Option<String>,
            locked: bool,
        }

        let key = BuildCacheKey {
            features: &features,
            all_features: options.all_features,
            no_default_features: options.no_default_features,
            profile: &options.profile,
            target: &options.target,
            build_target: &options.build_target,
            toolchain: &options.toolchain,
            locked: options.locked,
        };

        let json =
            serde_json::to_string(&key).expect("serializing a BuildCacheKey of plain fields cannot fail");
        Self::compute_hash(json.as_bytes())
    }

    /// Compute the expected binary name based on the build target.
    ///
    /// The binary name is deterministic based on the crate name and build target,
    /// with platform-specific extensions added automatically.
    fn expected_binary_name(crate_name: &str, build_target: &BuildTarget) -> String {
        let base_name = match build_target {
            BuildTarget::DefaultBin => crate_name,
            BuildTarget::Bin(name) | BuildTarget::Example(name) => name.as_str(),
        };

        #[cfg(windows)]
        return format!("{}.exe", base_name);

        #[cfg(not(windows))]
        return base_name.to_string();
    }
}

#[derive(Debug)]
struct CacheInner {
    config: Config,
    reporter: crate::messages::MessageReporter,
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use assert_matches::assert_matches;
    use semver::Version;
    use snafu::IntoError;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        bin_resolver::{BinaryCacheEntry, ConclusiveResolution, ResolvedBinary},
        target::TargetTriple,
    };

    fn target(target: &'static str) -> TargetTriple {
        TargetTriple::from_static(target).unwrap()
    }

    fn test_cache() -> (Cache, TempDir) {
        test_cache_with_timeout(Duration::from_secs(3600))
    }

    fn test_cache_with_timeout(timeout: Duration) -> (Cache, TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, mut config) = crate::config::create_test_env();
        config.resolve_cache_timeout = timeout;
        (
            Cache::new(config, crate::messages::MessageReporter::null()),
            temp_dir,
        )
    }

    fn test_cache_with_refresh() -> (Cache, TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, mut config) = crate::config::create_test_env();
        config.refresh = true;
        (
            Cache::new(config, crate::messages::MessageReporter::null()),
            temp_dir,
        )
    }

    fn test_spec() -> CrateSpec {
        CrateSpec::CratesIo {
            name: "serde".to_string(),
            version: None,
        }
    }

    fn test_spec_alt() -> CrateSpec {
        CrateSpec::CratesIo {
            name: "tokio".to_string(),
            version: None,
        }
    }

    fn test_resolved() -> ResolvedCrate {
        ResolvedCrate {
            name: "serde".to_string(),
            version: Version::parse("1.0.0").unwrap(),
            source: ResolvedSource::CratesIo,
        }
    }

    fn test_resolved_alt() -> ResolvedCrate {
        ResolvedCrate {
            name: "serde".to_string(),
            version: Version::parse("1.0.1").unwrap(),
            source: ResolvedSource::CratesIo,
        }
    }

    /// On-disk format / cache-key compatibility checks.
    ///
    /// These tests assert contents and paths that are persistent on disk, to detect when we've
    /// made any changes that will break compatibility with existing cache entries.
    ///
    /// A test failure here isn't automatically a bug that you have to fix.  The consequences for
    /// broken compat are pretty low: a cache miss followed by some otherwise-unnecessary rework
    /// like crate resolution or maybe even rebuilding from source.  That's not the end of the
    /// world, but if we're going to break compat we must do so deliberately; that's what these
    /// tests are here for.
    ///
    /// If you have decided to make a breaking change, update the tests here so that they pass with
    /// your new changes.  But that must always be a conscious decision.  God help you if I find
    /// out you vibe-coded some change and your clanker blithely updated these tests to ignore the
    /// break!
    mod compat {
        use chrono::{DateTime, Utc};

        use super::*;
        use crate::config::BinaryProvider;

        /// A fixed timestamp so serialized output is deterministic.
        fn fixed_cached_at() -> DateTime<Utc> {
            "2024-01-15T12:30:00Z".parse().unwrap()
        }

        /// The binary resolution cache, which stores both positive and negative results for
        /// whether a pre-built binary exists for a given crate.
        mod resolved_binary {
            use super::*;

            /// A positive entry: a resolved binary was found and cached.
            fn positive_entry() -> CacheEntry<BinaryCacheEntry> {
                CacheEntry {
                    value: BinaryCacheEntry {
                        outcome: ConclusiveResolution::Found(ResolvedBinary {
                            krate: ResolvedCrate {
                                name: "eza".to_string(),
                                version: Version::parse("0.23.1").unwrap(),
                                source: ResolvedSource::CratesIo,
                            },
                            provider: BinaryProvider::GithubReleases,
                            path: PathBuf::from("/cache/bin/eza"),
                        }),
                        enabled_providers: vec![
                            BinaryProvider::Binstall,
                            BinaryProvider::GithubReleases,
                            BinaryProvider::GitlabReleases,
                            BinaryProvider::Quickinstall,
                        ],
                    },
                    cached_at: fixed_cached_at(),
                }
            }

            /// A negative entry: we conclusively determined no binary is available.
            fn negative_entry() -> CacheEntry<BinaryCacheEntry> {
                CacheEntry {
                    value: BinaryCacheEntry {
                        outcome: ConclusiveResolution::Nonexistent,
                        enabled_providers: vec![
                            BinaryProvider::GithubReleases,
                            BinaryProvider::GitlabReleases,
                        ],
                    },
                    cached_at: fixed_cached_at(),
                }
            }

            const POSITIVE_JSON: &str = r#"{
  "outcome": "found",
  "krate": {
    "name": "eza",
    "version": "0.23.1",
    "source": "crates_io"
  },
  "provider": "github-releases",
  "path": "/cache/bin/eza",
  "enabled_providers": [
    "binstall",
    "github-releases",
    "gitlab-releases",
    "quickinstall"
  ],
  "cached_at": "2024-01-15T12:30:00Z"
}"#;

            const NEGATIVE_JSON: &str = r#"{
  "outcome": "nonexistent",
  "enabled_providers": [
    "github-releases",
    "gitlab-releases"
  ],
  "cached_at": "2024-01-15T12:30:00Z"
}"#;

            #[test]
            fn positive_entry_serializes_to_expected_json() {
                assert_eq!(
                    serde_json::to_string_pretty(&positive_entry()).unwrap(),
                    POSITIVE_JSON
                );
            }

            #[test]
            fn negative_entry_serializes_to_expected_json() {
                assert_eq!(
                    serde_json::to_string_pretty(&negative_entry()).unwrap(),
                    NEGATIVE_JSON
                );
            }

            #[test]
            fn positive_entry_round_trips() {
                let original = positive_entry();
                let json = serde_json::to_string_pretty(&original).unwrap();
                let back: CacheEntry<BinaryCacheEntry> = serde_json::from_str(&json).unwrap();
                assert_eq!(back, original);
                let from_literal: CacheEntry<BinaryCacheEntry> = serde_json::from_str(POSITIVE_JSON).unwrap();
                assert_eq!(from_literal, original);
            }

            #[test]
            fn negative_entry_round_trips() {
                let original = negative_entry();
                let json = serde_json::to_string_pretty(&original).unwrap();
                let back: CacheEntry<BinaryCacheEntry> = serde_json::from_str(&json).unwrap();
                assert_eq!(back, original);
                let from_literal: CacheEntry<BinaryCacheEntry> = serde_json::from_str(NEGATIVE_JSON).unwrap();
                assert_eq!(from_literal, original);
            }
        }

        /// The crate resolution cache, which stores the resolved crate identity (name, version,
        /// source) for a given [`CrateSpec`].  Since the resolved crate also has a
        /// [`ResolvedSource`], this test also covers compat testing for that type as well.
        mod resolved_crate {
            use super::*;

            fn crate_with(source: ResolvedSource) -> ResolvedCrate {
                ResolvedCrate {
                    name: "demo".to_string(),
                    version: Version::parse("1.2.3").unwrap(),
                    source,
                }
            }

            const ON_DISK_JSON: &str = r#"{
  "name": "demo",
  "version": "1.2.3",
  "source": "crates_io",
  "cached_at": "2024-01-15T12:30:00Z"
}"#;

            #[test]
            fn on_disk_cache_entry_round_trips_to_expected_json() {
                let entry = CacheEntry {
                    value: crate_with(ResolvedSource::CratesIo),
                    cached_at: fixed_cached_at(),
                };
                assert_eq!(serde_json::to_string_pretty(&entry).unwrap(), ON_DISK_JSON);
                let back: CacheEntry<ResolvedCrate> = serde_json::from_str(ON_DISK_JSON).unwrap();
                assert_eq!(back, entry);
            }

            /// Assert a `ResolvedSource` serializes to exactly `expected` and round-trips back.
            fn check(source: ResolvedSource, expected: &str) {
                assert_eq!(serde_json::to_string_pretty(&source).unwrap(), expected);
                let back: ResolvedSource = serde_json::from_str(expected).unwrap();
                assert_eq!(back, source);
            }

            #[test]
            fn crates_io() {
                check(ResolvedSource::CratesIo, r#""crates_io""#);
            }

            #[test]
            fn registry_named() {
                check(
                    ResolvedSource::Registry {
                        source: RegistrySource::Named("my-registry".to_string()),
                    },
                    r#"{
  "registry": {
    "source": {
      "named": "my-registry"
    }
  }
}"#,
                );
            }

            #[test]
            fn registry_index_url() {
                check(
                    ResolvedSource::Registry {
                        source: RegistrySource::IndexUrl(
                            url::Url::parse("https://example.com/index").unwrap(),
                        ),
                    },
                    r#"{
  "registry": {
    "source": {
      "index_url": "https://example.com/index"
    }
  }
}"#,
                );
            }

            #[test]
            fn git() {
                check(
                    ResolvedSource::Git {
                        repo: "https://github.com/owner/repo.git".to_string(),
                        commit: "abc123".to_string(),
                    },
                    r#"{
  "git": {
    "repo": "https://github.com/owner/repo.git",
    "commit": "abc123"
  }
}"#,
                );
            }

            #[test]
            fn forge_github() {
                check(
                    ResolvedSource::Forge {
                        forge: Forge::GitHub {
                            custom_url: None,
                            owner: "owner".to_string(),
                            repo: "repo".to_string(),
                        },
                        commit: "abc123".to_string(),
                    },
                    r#"{
  "forge": {
    "forge": {
      "git_hub": {
        "custom_url": null,
        "owner": "owner",
        "repo": "repo"
      }
    },
    "commit": "abc123"
  }
}"#,
                );
            }

            #[test]
            fn forge_gitlab() {
                check(
                    ResolvedSource::Forge {
                        forge: Forge::GitLab {
                            custom_url: None,
                            owner: "owner".to_string(),
                            repo: "repo".to_string(),
                        },
                        commit: "def456".to_string(),
                    },
                    r#"{
  "forge": {
    "forge": {
      "git_lab": {
        "custom_url": null,
        "owner": "owner",
        "repo": "repo"
      }
    },
    "commit": "def456"
  }
}"#,
                );
            }

            #[test]
            fn local_dir() {
                check(
                    ResolvedSource::LocalDir {
                        path: PathBuf::from("/some/local/path"),
                    },
                    r#"{
  "local_dir": {
    "path": "/some/local/path"
  }
}"#,
                );
            }
        }

        /// The resolve-cache key, which is a hash computed from a crate spec and used to construct
        /// entries.
        mod crate_spec_hash {
            use semver::VersionReq;

            use super::*;
            use crate::git::GitSelector;

            #[test]
            fn crates_io() {
                let spec = CrateSpec::CratesIo {
                    name: "serde".to_string(),
                    version: Some(VersionReq::parse("=1.0.0").unwrap()),
                };
                assert_eq!(
                    Cache::compute_spec_hash(&spec).unwrap(),
                    "79b48b0d3feee138ee1ea1f22108170f85431c6240de6571ed3693c1001a9974"
                );
            }

            #[test]
            fn registry_named() {
                let spec = CrateSpec::Registry {
                    source: RegistrySource::Named("my-registry".to_string()),
                    name: "serde".to_string(),
                    version: None,
                };
                assert_eq!(
                    Cache::compute_spec_hash(&spec).unwrap(),
                    "7a3b4789c6780ca2b848cc536fbb0ea1e034128e2e492ada1a31ce7f633c4f52"
                );
            }

            #[test]
            fn git() {
                let spec = CrateSpec::Git {
                    repo: "https://github.com/owner/repo.git".to_string(),
                    selector: GitSelector::Tag("v1.0.0".to_string()),
                    name: None,
                    version: None,
                };
                assert_eq!(
                    Cache::compute_spec_hash(&spec).unwrap(),
                    "5d8eebf800c54c1e46ed66e72ff8b1c9bd7bfc88325aa2473b068aa69d9466a4"
                );
            }

            #[test]
            fn forge_github() {
                let spec = CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "owner".to_string(),
                        repo: "repo".to_string(),
                    },
                    selector: GitSelector::DefaultBranch,
                    name: None,
                    version: None,
                };
                assert_eq!(
                    Cache::compute_spec_hash(&spec).unwrap(),
                    "9338fcc8761b894c6681a9100281143350ad2ded6bce067a4b39d2a787972ef2"
                );
            }

            #[test]
            fn local_dir() {
                let spec = CrateSpec::LocalDir {
                    path: PathBuf::from("/some/local/path"),
                    name: None,
                    version: None,
                };
                assert_eq!(
                    Cache::compute_spec_hash(&spec).unwrap(),
                    "e87d6dd1b220d02006524fbaac4d7c654b55eff1bd9a1332fd52a5e738a8fca5"
                );
            }
        }

        /// The binary-resolution-cache key, which is a hash computed from a resolved crate and the
        /// target platform.
        mod binary_cache_hash {
            use super::*;

            #[test]
            fn crates_io() {
                let krate = ResolvedCrate {
                    name: "eza".to_string(),
                    version: Version::parse("0.23.1").unwrap(),
                    source: ResolvedSource::CratesIo,
                };
                assert_eq!(
                    Cache::compute_binary_cache_hash(&krate, &target("x86_64-unknown-linux-gnu")).unwrap(),
                    "e73c84363ece02d92a3dfe4ff390dcbe0dbe3381f050280505a1adb3916fad78"
                );
            }

            #[test]
            fn platform_changes_the_hash() {
                let krate = ResolvedCrate {
                    name: "eza".to_string(),
                    version: Version::parse("0.23.1").unwrap(),
                    source: ResolvedSource::CratesIo,
                };
                assert_ne!(
                    Cache::compute_binary_cache_hash(&krate, &target("x86_64-unknown-linux-gnu")).unwrap(),
                    Cache::compute_binary_cache_hash(&krate, &target("aarch64-apple-darwin")).unwrap(),
                );
            }
        }

        /// The build-cache key, a hash computed from the resolved source and build options that
        /// affect the output binary.
        mod build_cache_hash {
            use super::*;
            use crate::builder::{BuildOptions, BuildTarget};

            #[test]
            fn source_hash_crates_io() {
                assert_eq!(
                    Cache::compute_source_hash(&ResolvedSource::CratesIo),
                    "797cfdcfdf4d45ed0d3963577b0e549fe0c37295320d320d7173bfcf7bc42842"
                );
            }

            #[test]
            fn build_hash_default_options() {
                assert_eq!(
                    Cache::compute_build_hash(&BuildOptions::default()),
                    "115fc6c55136730f0dbc99c9d90074d669d03b8ab9c61ccad2b76cb535f688af"
                );
            }

            #[test]
            fn build_hash_with_options() {
                let options = BuildOptions {
                    features: vec!["json".to_string(), "tls".to_string()],
                    all_features: false,
                    no_default_features: true,
                    profile: Some("release".to_string()),
                    target: Some(target("x86_64-unknown-linux-gnu")),
                    build_target: BuildTarget::Bin("mybin".to_string()),
                    toolchain: Some("stable".to_string()),
                    locked: true,
                    ..Default::default()
                };
                assert_eq!(
                    Cache::compute_build_hash(&options),
                    "122238fafcf514ae03c828f2b94c68a787ec9b3c69854aff6b51ae4b7f732c85"
                );
            }
        }

        /// The crate source cache directory layout, where crate source code is cached for reuse
        /// across builds.
        mod source_cache_paths {
            use super::*;

            fn assert_source_path(source: ResolvedSource, expected_suffix: &[&str]) {
                let (cache, _temp) = test_cache();
                let resolved = ResolvedCrate {
                    name: "demo".to_string(),
                    version: Version::parse("1.2.3").unwrap(),
                    source,
                };
                let mut expected = cache.inner.config.cache_dir.join("sources");
                for component in expected_suffix {
                    expected = expected.join(component);
                }
                assert_eq!(cache.crate_source_cache_path(&resolved).unwrap(), expected);
            }

            #[test]
            fn crates_io() {
                assert_source_path(ResolvedSource::CratesIo, &["crates-io", "demo", "1.2.3"]);
            }

            #[test]
            fn registry_named() {
                assert_source_path(
                    ResolvedSource::Registry {
                        source: RegistrySource::Named("my-registry".to_string()),
                    },
                    &["registry", "my-registry", "demo", "1.2.3"],
                );
            }

            #[test]
            fn registry_index_url() {
                assert_source_path(
                    ResolvedSource::Registry {
                        source: RegistrySource::IndexUrl(
                            url::Url::parse("https://example.com/index").unwrap(),
                        ),
                    },
                    &[
                        "registry-index",
                        "bf2749857dd97af7bf8b1b035bd103caee1276d7cf54cd24ea19dc9167bb0918",
                        "demo",
                        "1.2.3",
                    ],
                );
            }

            #[test]
            fn git() {
                // `sources/git/{sha256(repo_url)}/{commit}` (no name/version component)
                assert_source_path(
                    ResolvedSource::Git {
                        repo: "https://github.com/owner/repo.git".to_string(),
                        commit: "abc123".to_string(),
                    },
                    &[
                        "git",
                        "bc40893b43beea6303cb93d2e61df787840b4e09dcf293506e68491b9a082686",
                        "abc123",
                    ],
                );
            }

            #[test]
            fn forge_github() {
                // `sources/github/{owner}/{repo}/{commit}` (no name/version component)
                assert_source_path(
                    ResolvedSource::Forge {
                        forge: Forge::GitHub {
                            custom_url: None,
                            owner: "owner".to_string(),
                            repo: "repo".to_string(),
                        },
                        commit: "abc123".to_string(),
                    },
                    &["github", "owner", "repo", "abc123"],
                );
            }

            #[test]
            fn forge_gitlab() {
                // `sources/gitlab/{owner}/{repo}/{commit}` (no name/version component)
                assert_source_path(
                    ResolvedSource::Forge {
                        forge: Forge::GitLab {
                            custom_url: None,
                            owner: "owner".to_string(),
                            repo: "repo".to_string(),
                        },
                        commit: "def456".to_string(),
                    },
                    &["gitlab", "owner", "repo", "def456"],
                );
            }
        }
    }

    mod get_or_resolve {
        use super::*;

        #[test]
        fn cache_miss_calls_closure() {
            let (cache, _temp) = test_cache();
            let spec = test_spec();
            let resolved = test_resolved();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();
            let resolved_clone = resolved.clone();

            let result = cache.get_or_resolve_crate(&spec, || {
                *call_count_clone.borrow_mut() += 1;
                Ok(resolved_clone.clone())
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), resolved);
            assert_eq!(*call_count.borrow(), 1);

            let cached = cache.get_resolved_crate(&spec).unwrap();
            assert_eq!(cached.map(|e| e.value), Some(resolved));
        }

        #[test]
        fn cache_hit_valid_skips_closure() {
            let (cache, _temp) = test_cache();
            let spec = test_spec();
            let resolved = test_resolved();

            cache.put_resolved_crate(&spec, &resolved).unwrap();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result = cache.get_or_resolve_crate(&spec, || {
                *call_count_clone.borrow_mut() += 1;
                Ok(test_resolved_alt())
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), resolved);
            assert_eq!(*call_count.borrow(), 0);
        }

        #[test]
        fn cache_hit_expired_calls_closure() {
            let (cache, _temp) = test_cache_with_timeout(Duration::from_secs(0));
            let spec = test_spec();
            let old_resolved = test_resolved();
            let new_resolved = test_resolved_alt();

            cache.put_resolved_crate(&spec, &old_resolved).unwrap();
            std::thread::sleep(Duration::from_secs(1));

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();
            let new_resolved_clone = new_resolved.clone();

            let result = cache.get_or_resolve_crate(&spec, || {
                *call_count_clone.borrow_mut() += 1;
                Ok(new_resolved_clone.clone())
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), new_resolved);
            assert_eq!(*call_count.borrow(), 1);
        }

        #[test]
        fn network_error_with_stale_returns_stale() {
            let (cache, _temp) = test_cache_with_timeout(Duration::from_secs(0));
            let spec = test_spec();
            let resolved = test_resolved();

            cache.put_resolved_crate(&spec, &resolved).unwrap();
            std::thread::sleep(Duration::from_secs(1));

            let result = cache.get_or_resolve_crate(&spec, || {
                Err(
                    error::RegistrySnafu.into_error(tame_index::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "network error",
                    ))),
                )
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), resolved);
        }

        #[test]
        fn network_error_without_stale_propagates() {
            let (cache, _temp) = test_cache();
            let spec = test_spec();

            let result = cache.get_or_resolve_crate(&spec, || {
                Err(
                    error::RegistrySnafu.into_error(tame_index::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "network error",
                    ))),
                )
            });

            assert_matches!(result.unwrap_err(), error::Error::Registry { .. });
        }

        #[test]
        fn io_error_with_stale_returns_stale() {
            let (cache, _temp) = test_cache_with_timeout(Duration::from_secs(0));
            let spec = test_spec();
            let resolved = test_resolved();

            cache.put_resolved_crate(&spec, &resolved).unwrap();
            std::thread::sleep(Duration::from_secs(1));

            let result = cache.get_or_resolve_crate(&spec, || {
                Err(error::IoSnafu {
                    path: PathBuf::from("/fake/test/path"),
                }
                .into_error(std::io::Error::new(std::io::ErrorKind::Other, "io error")))
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), resolved);
        }

        #[test]
        fn other_error_never_uses_stale() {
            let (cache, _temp) = test_cache_with_timeout(Duration::from_secs(0));
            let spec = test_spec();
            let resolved = test_resolved();

            cache.put_resolved_crate(&spec, &resolved).unwrap();
            std::thread::sleep(Duration::from_secs(1));

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result = cache.get_or_resolve_crate(&spec, || {
                *call_count_clone.borrow_mut() += 1;
                error::VersionMismatchSnafu {
                    requirement: "2.0.0".to_string(),
                    found: Version::parse("1.0.0").unwrap(),
                }
                .fail()
            });

            assert_eq!(*call_count.borrow(), 1, "Closure should have been called");
            assert_matches!(result.unwrap_err(), error::Error::VersionMismatch { .. });
        }

        #[test]
        fn successful_resolve_updates_cache() {
            let (cache, _temp) = test_cache_with_timeout(Duration::from_secs(0));
            let spec = test_spec();
            let old_resolved = test_resolved();
            let new_resolved = test_resolved_alt();

            cache.put_resolved_crate(&spec, &old_resolved).unwrap();
            std::thread::sleep(Duration::from_secs(1));

            let result = cache.get_or_resolve_crate(&spec, || Ok(new_resolved.clone()));

            assert!(result.is_ok());
            assert_eq!(result.unwrap(), new_resolved);

            let cached = cache.get_resolved_crate(&spec).unwrap();
            assert_eq!(cached.map(|e| e.value), Some(new_resolved));
        }

        #[test]
        fn refresh_bypasses_valid_cache() {
            let (cache, _temp) = test_cache_with_refresh();
            let spec = test_spec();
            let cached_resolved = test_resolved();
            let new_resolved = test_resolved_alt();

            cache.put_resolved_crate(&spec, &cached_resolved).unwrap();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();
            let new_resolved_clone = new_resolved.clone();

            let resolved_crate = cache
                .get_or_resolve_crate(&spec, || {
                    *call_count_clone.borrow_mut() += 1;
                    Ok(new_resolved_clone.clone())
                })
                .unwrap();

            assert_eq!(resolved_crate, new_resolved);
            assert_eq!(
                *call_count.borrow(),
                1,
                "Resolver should be called even with valid cache"
            );
        }

        #[test]
        fn refresh_disables_stale_cache_fallback() {
            let (cache, _temp) = test_cache_with_refresh();
            let spec = test_spec();
            let stale_resolved = test_resolved();

            cache
                .insert_stale_resolve_entry(&spec, &stale_resolved, Duration::from_secs(9999))
                .unwrap();

            let result = cache.get_or_resolve_crate(&spec, || {
                Err(
                    error::RegistrySnafu.into_error(tame_index::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "network error",
                    ))),
                )
            });

            assert_matches!(result, Err(error::Error::Registry { .. }));
        }
    }

    mod get_or_download {
        use super::*;

        #[test]
        fn source_cache_hit_skips_downloader() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();

            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();
            fs::create_dir_all(&cache_path).unwrap();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result = cache.get_or_download_crate(&resolved, |_download_path| {
                *call_count_clone.borrow_mut() += 1;
                Err(error::IoSnafu {
                    path: PathBuf::from("/fake/test/path"),
                }
                .into_error(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "should not be called",
                )))
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap().crate_path, cache_path);
            assert_eq!(*call_count.borrow(), 0);
        }

        #[test]
        fn source_cache_miss_calls_downloader() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();
            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result = cache.get_or_download_crate(&resolved, |download_path| {
                *call_count_clone.borrow_mut() += 1;
                // Create a test file to simulate successful download
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("test.txt"), b"test content").unwrap();
                Ok(())
            });

            assert!(result.is_ok());
            assert_eq!(result.unwrap().crate_path, cache_path);
            assert_eq!(*call_count.borrow(), 1);

            // Verify the downloaded file is in the cache
            assert!(cache_path.join("test.txt").exists());
        }

        #[test]
        fn download_error_without_cache() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();

            let result = cache.get_or_download_crate(&resolved, |_download_path| {
                Err(error::IoSnafu {
                    path: PathBuf::from("/fake/test/path"),
                }
                .into_error(std::io::Error::new(std::io::ErrorKind::Other, "download failed")))
            });

            assert_matches!(result.unwrap_err(), error::Error::Io { .. });
        }

        #[test]
        fn successful_download_creates_cache_entry() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();
            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();

            // Verify cache doesn't exist initially
            assert!(!cache_path.exists());

            let result = cache.get_or_download_crate(&resolved, |download_path| {
                // Create multiple files to simulate real download
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("Cargo.toml"), b"[package]\nname = \"test\"").unwrap();
                fs::write(download_path.join("lib.rs"), b"pub fn test() {}").unwrap();
                Ok(())
            });

            assert!(result.is_ok());
            let cached = result.unwrap();
            assert_eq!(cached.crate_path, cache_path);

            // Verify files are in the cache location, not temp
            assert!(cache_path.join("Cargo.toml").exists());
            assert!(cache_path.join("lib.rs").exists());
        }

        #[test]
        fn failed_download_does_not_create_cache_entry() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();
            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();

            let result = cache.get_or_download_crate(&resolved, |download_path| {
                // Create some files but then fail
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("partial.txt"), b"partial data").unwrap();
                Err(error::IoSnafu {
                    path: PathBuf::from("/fake/test/path"),
                }
                .into_error(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "simulated failure",
                )))
            });

            assert_matches!(result.unwrap_err(), error::Error::Io { .. });

            // Verify cache path doesn't exist (no partial download)
            assert!(!cache_path.exists());

            // Verify no temp directories were left behind in the parent
            let cache_parent = cache_path.parent().unwrap();
            if cache_parent.exists() {
                let entries: Vec<_> = fs::read_dir(cache_parent)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .collect();
                // Should be empty or not contain our cache entry
                assert!(entries.is_empty() || !entries.iter().any(|e| e.path() == cache_path));
            }
        }

        #[test]
        fn race_condition_both_downloads_succeed() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();
            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();

            // Simulate first download
            let result1 = cache.get_or_download_crate(&resolved, |download_path| {
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("version.txt"), b"download1").unwrap();
                Ok(())
            });

            assert!(result1.is_ok());
            let cached1 = result1.unwrap();
            assert_eq!(cached1.crate_path, cache_path);

            // Simulate second download (race condition - someone already downloaded)
            // This should return the existing cache without calling the downloader
            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result2 = cache.get_or_download_crate(&resolved, |download_path| {
                *call_count_clone.borrow_mut() += 1;
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("version.txt"), b"download2").unwrap();
                Ok(())
            });

            assert!(result2.is_ok());
            let cached2 = result2.unwrap();
            assert_eq!(cached2.crate_path, cache_path);

            // Second downloader should not have been called
            assert_eq!(*call_count.borrow(), 0);

            // Verify first download's content is preserved
            let content = fs::read_to_string(cache_path.join("version.txt")).unwrap();
            assert_eq!(content, "download1");
        }

        #[test]
        fn refresh_bypasses_source_cache() {
            let (cache, _temp) = test_cache_with_refresh();
            let resolved = test_resolved();
            let cache_path = cache.crate_source_cache_path(&resolved).unwrap();

            fs::create_dir_all(&cache_path).unwrap();
            fs::write(cache_path.join("cached.txt"), b"cached content").unwrap();

            let call_count = Rc::new(RefCell::new(0));
            let call_count_clone = call_count.clone();

            let result = cache.get_or_download_crate(&resolved, |download_path| {
                *call_count_clone.borrow_mut() += 1;
                fs::create_dir_all(download_path).unwrap();
                fs::write(download_path.join("fresh.txt"), b"fresh content").unwrap();
                Ok(())
            });

            result.unwrap();
            assert_eq!(
                *call_count.borrow(),
                1,
                "Downloader should be called even with cached source"
            );
        }
    }

    mod binary_cache_hash {
        use super::*;
        use crate::builder::{BuildOptions, BuildTarget};

        #[test]
        fn same_inputs_produce_same_hash() {
            let options = BuildOptions {
                features: vec!["foo".to_string(), "bar".to_string()],
                profile: Some("release".to_string()),
                ..Default::default()
            };

            let hash1 = Cache::compute_build_hash(&options);
            let hash2 = Cache::compute_build_hash(&options);

            assert_eq!(hash1, hash2);
        }

        #[test]
        fn feature_order_doesnt_matter() {
            let options1 = BuildOptions {
                features: vec!["foo".to_string(), "bar".to_string(), "baz".to_string()],
                ..Default::default()
            };
            let options2 = BuildOptions {
                features: vec!["baz".to_string(), "foo".to_string(), "bar".to_string()],
                ..Default::default()
            };

            assert_eq!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2),
                "Same features in different order should produce same hash"
            );
        }

        #[test]
        fn different_features_produce_different_hash() {
            let options1 = BuildOptions {
                features: vec!["foo".to_string()],
                ..Default::default()
            };
            let options2 = BuildOptions {
                features: vec!["bar".to_string()],
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn different_profile_produces_different_hash() {
            let options1 = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };
            let options2 = BuildOptions {
                profile: Some("release".to_string()),
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn different_target_produces_different_hash() {
            let options1 = BuildOptions {
                target: Some(target("x86_64-unknown-linux-gnu")),
                ..Default::default()
            };
            let options2 = BuildOptions {
                target: Some(target("aarch64-unknown-linux-gnu")),
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn different_toolchain_produces_different_hash() {
            let options1 = BuildOptions {
                toolchain: Some("stable".to_string()),
                ..Default::default()
            };
            let options2 = BuildOptions {
                toolchain: Some("nightly".to_string()),
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn different_build_target_produces_different_hash() {
            let options1 = BuildOptions {
                build_target: BuildTarget::DefaultBin,
                ..Default::default()
            };
            let options2 = BuildOptions {
                build_target: BuildTarget::Bin("foo".to_string()),
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn all_features_affects_hash() {
            let options1 = BuildOptions {
                all_features: false,
                ..Default::default()
            };
            let options2 = BuildOptions {
                all_features: true,
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn no_default_features_affects_hash() {
            let options1 = BuildOptions {
                no_default_features: false,
                ..Default::default()
            };
            let options2 = BuildOptions {
                no_default_features: true,
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2)
            );
        }

        #[test]
        fn locked_flag_affects_hash() {
            let options1 = BuildOptions {
                locked: true,
                ..Default::default()
            };
            let options2 = BuildOptions {
                locked: false,
                ..Default::default()
            };

            assert_ne!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2),
                "locked flag affects dependency resolution, so it must affect hash"
            );
        }

        #[test]
        fn offline_flag_does_not_affect_hash() {
            let options1 = BuildOptions {
                offline: true,
                ..Default::default()
            };
            let options2 = BuildOptions {
                offline: false,
                ..Default::default()
            };

            assert_eq!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2),
                "offline flag should not affect hash"
            );
        }

        #[test]
        fn jobs_does_not_affect_hash() {
            let options1 = BuildOptions {
                jobs: Some(1),
                ..Default::default()
            };
            let options2 = BuildOptions {
                jobs: Some(8),
                ..Default::default()
            };

            assert_eq!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2),
                "jobs setting should not affect hash"
            );
        }

        #[test]
        fn ignore_rust_version_does_not_affect_hash() {
            let options1 = BuildOptions {
                ignore_rust_version: true,
                ..Default::default()
            };
            let options2 = BuildOptions {
                ignore_rust_version: false,
                ..Default::default()
            };

            assert_eq!(
                Cache::compute_build_hash(&options1),
                Cache::compute_build_hash(&options2),
                "ignore_rust_version should not affect hash"
            );
        }

        #[test]
        fn source_hash_distinguishes_crates_io() {
            let hash = Cache::compute_source_hash(&ResolvedSource::CratesIo);
            assert_eq!(hash.len(), 64, "SHA-256 hash should be 64 hex chars");
        }

        #[test]
        fn source_hash_distinguishes_git() {
            let hash1 = Cache::compute_source_hash(&ResolvedSource::Git {
                repo: "https://github.com/rust-lang/cargo".to_string(),
                commit: "abc123".to_string(),
            });
            let hash2 = Cache::compute_source_hash(&ResolvedSource::Git {
                repo: "https://github.com/rust-lang/cargo".to_string(),
                commit: "def456".to_string(),
            });

            assert_ne!(hash1, hash2, "Different commits should produce different hashes");
        }

        #[test]
        fn source_hash_distinguishes_forge() {
            let hash1 = Cache::compute_source_hash(&ResolvedSource::Forge {
                forge: Forge::GitHub {
                    custom_url: None,
                    owner: "rust-lang".to_string(),
                    repo: "cargo".to_string(),
                },
                commit: "abc123".to_string(),
            });
            let hash2 = Cache::compute_source_hash(&ResolvedSource::Forge {
                forge: Forge::GitHub {
                    custom_url: None,
                    owner: "rust-lang".to_string(),
                    repo: "cargo".to_string(),
                },
                commit: "def456".to_string(),
            });

            assert_ne!(hash1, hash2, "Different commits should produce different hashes");
        }

        #[test]
        fn source_hash_distinguishes_registry() {
            let hash1 = Cache::compute_source_hash(&ResolvedSource::Registry {
                source: RegistrySource::Named("my-registry".to_string()),
            });
            let hash2 = Cache::compute_source_hash(&ResolvedSource::Registry {
                source: RegistrySource::Named("other-registry".to_string()),
            });

            assert_ne!(
                hash1, hash2,
                "Different registries should produce different hashes"
            );
        }

        #[test]
        fn expected_binary_name_default_bin() {
            let name = Cache::expected_binary_name("my-crate", &BuildTarget::DefaultBin);
            #[cfg(windows)]
            assert_eq!(name, "my-crate.exe");
            #[cfg(not(windows))]
            assert_eq!(name, "my-crate");
        }

        #[test]
        fn expected_binary_name_specific_bin() {
            let name = Cache::expected_binary_name("my-crate", &BuildTarget::Bin("foo".to_string()));
            #[cfg(windows)]
            assert_eq!(name, "foo.exe");
            #[cfg(not(windows))]
            assert_eq!(name, "foo");
        }

        #[test]
        fn expected_binary_name_example() {
            let name = Cache::expected_binary_name("my-crate", &BuildTarget::Example("bar".to_string()));
            #[cfg(windows)]
            assert_eq!(name, "bar.exe");
            #[cfg(not(windows))]
            assert_eq!(name, "bar");
        }
    }

    mod utility {
        use super::*;

        #[test]
        fn hash_stability() {
            let spec = test_spec();

            let hash1 = Cache::compute_spec_hash(&spec).unwrap();
            let hash2 = Cache::compute_spec_hash(&spec).unwrap();

            assert_eq!(hash1, hash2);
        }

        #[test]
        fn hash_uniqueness() {
            let spec1 = test_spec();
            let spec2 = test_spec_alt();

            let hash1 = Cache::compute_spec_hash(&spec1).unwrap();
            let hash2 = Cache::compute_spec_hash(&spec2).unwrap();

            assert_ne!(hash1, hash2);
        }
    }

    /// The cache's job for binary entries is pretty dumb at this level; just get and put cache
    /// entry structs.
    mod binary_cache {
        use super::*;
        use crate::config::BinaryProvider;

        #[test]
        fn put_then_get_round_trips_outcome_and_providers() {
            let (cache, _temp) = test_cache();
            let krate = test_resolved();
            let providers = vec![BinaryProvider::Binstall, BinaryProvider::GithubReleases];

            assert_matches!(cache.get_cached_binary(&krate), Ok(None));

            cache
                .put_cached_binary(
                    &krate,
                    BinaryCacheEntry {
                        outcome: ConclusiveResolution::Nonexistent,
                        enabled_providers: providers.clone(),
                    },
                )
                .unwrap();

            let entry = cache.get_cached_binary(&krate).unwrap().unwrap();
            assert_eq!(entry.outcome, ConclusiveResolution::Nonexistent);
            assert_eq!(entry.enabled_providers, providers);
        }

        #[test]
        fn put_overwrites_previous_entry() {
            let (cache, _temp) = test_cache();
            let krate = test_resolved();

            cache
                .put_cached_binary(
                    &krate,
                    BinaryCacheEntry {
                        outcome: ConclusiveResolution::Nonexistent,
                        enabled_providers: vec![BinaryProvider::GitlabReleases],
                    },
                )
                .unwrap();

            let found = BinaryCacheEntry {
                outcome: ConclusiveResolution::Found(ResolvedBinary {
                    krate: test_resolved(),
                    provider: BinaryProvider::GithubReleases,
                    path: PathBuf::from("/cache/bin/serde"),
                }),
                enabled_providers: vec![BinaryProvider::GithubReleases],
            };
            cache.put_cached_binary(&krate, found.clone()).unwrap();

            let entry = cache.get_cached_binary(&krate).unwrap().unwrap();
            assert_eq!(entry, found);
        }
    }
}
