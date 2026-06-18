use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
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
    bin_resolver::ResolvedBinary,
    builder::{BuildOptions, BuildTarget},
    config::Config,
    crate_resolver::{ResolvedCrate, ResolvedSource},
    cratespec::{CrateSpec, Forge, RegistrySource},
    downloader::DownloadedCrate,
    error,
    messages::{BuildCacheMessage, CrateResolutionMessage, PrebuiltBinaryMessage, SourceMessage},
};

/// A cache entry wrapping a value with timestamp metadata.
///
/// This generic wrapper is used for any cached data that has an expiration policy.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct CacheEntry<T> {
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

    /// Get a cached binary resolution result, or resolve it using the provided resolver function.
    ///
    /// Binary resolution results never expire because crates are immutable. Once we determine
    /// whether a binary exists for a specific version on a specific platform, that answer remains
    /// valid forever. We cache both positive (binary found) and negative (no binary) results to
    /// avoid repeatedly checking providers.
    ///
    /// Unlike crate resolution, there is no TTL check - the cache entry is permanent.
    ///
    /// # Arguments
    ///
    /// * `krate` - The resolved crate to find a binary for
    /// * `resolver` - Function that attempts to find and download a pre-built binary
    ///
    /// # Returns
    ///
    /// * `Ok(Some(ResolvedBinary))` - Found a pre-built binary (either cached or freshly resolved)
    /// * `Ok(None)` - No pre-built binary available (either cached negative result or resolver
    ///   returned None)
    /// * `Err(...)` - An error occurred during resolution
    pub(crate) fn get_or_resolve_binary<F>(
        &self,
        krate: &ResolvedCrate,
        resolver: F,
    ) -> Result<Option<ResolvedBinary>>
    where
        F: FnOnce() -> Result<Option<ResolvedBinary>>,
    {
        // Check cache unless refresh mode is enabled
        let use_cache = !self.inner.config.refresh;

        if use_cache {
            self.inner
                .reporter
                .report(|| PrebuiltBinaryMessage::cache_lookup(krate));

            if let Ok(Some(entry)) = self.get_cached_binary(krate) {
                match &entry.value {
                    Some(binary) => {
                        self.inner
                            .reporter
                            .report(|| PrebuiltBinaryMessage::cache_hit(&binary.path, binary.provider));
                    }
                    None => {
                        // Negative cache hit - we previously determined no binary was available
                        self.inner.reporter.report(|| {
                            PrebuiltBinaryMessage::no_binary_found(
                                krate,
                                vec!["negative cache hit - no binary available".to_string()],
                            )
                        });
                    }
                }
                // Return the cached result whether it's Some or None
                return Ok(entry.value);
            }

            self.inner
                .reporter
                .report(|| PrebuiltBinaryMessage::cache_miss(krate));
        }

        // Call the resolver to attempt finding a binary
        match resolver() {
            Ok(result) => {
                // Cache the result (whether Some or None)
                let _ = self.put_cached_binary(krate, &result);

                if let Some(ref _binary) = result {
                    if let Ok(cache_path) = self.binary_cache_path(krate) {
                        self.inner
                            .reporter
                            .report(|| PrebuiltBinaryMessage::cache_stored(&cache_path));
                    }
                } else {
                    // Also report when we cache a negative result
                    if let Ok(cache_path) = self.binary_cache_path(krate) {
                        self.inner
                            .reporter
                            .report(|| PrebuiltBinaryMessage::cache_stored(&cache_path));
                    }
                }

                Ok(result)
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
        F: FnOnce(&std::path::Path) -> Result<()>,
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
        let temp_dir = tempfile::tempdir_in(parent).with_context(|_| error::TempDirCreationSnafu {
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

    /// Get a cached binary resolution result for the given [`ResolvedCrate`], if one exists.
    ///
    /// Returns `None` if there is no cached entry or if reading the cache fails.
    /// Note that a cached entry can contain `Some(ResolvedBinary)` or `None` - we cache
    /// both positive and negative results.
    fn get_cached_binary(&self, krate: &ResolvedCrate) -> Result<Option<CacheEntry<Option<ResolvedBinary>>>> {
        let cache_file = self.binary_cache_path(krate)?;
        if !cache_file.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&cache_file).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;
        let entry: CacheEntry<Option<ResolvedBinary>> =
            serde_json::from_str(&contents).context(error::JsonSnafu)?;

        Ok(Some(entry))
    }

    /// Store a binary resolution result in the cache for the given [`ResolvedCrate`].
    ///
    /// This stores both positive results (Some(ResolvedBinary)) and negative results (None).
    fn put_cached_binary(&self, krate: &ResolvedCrate, result: &Option<ResolvedBinary>) -> Result<()> {
        let cache_file = self.binary_cache_path(krate)?;

        if let Some(parent) = cache_file.parent() {
            fs::create_dir_all(parent).with_context(|_| error::IoSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        let entry = CacheEntry::new(result.clone());

        let json = serde_json::to_string_pretty(&entry).context(error::JsonSnafu)?;
        fs::write(&cache_file, json).with_context(|_| error::IoSnafu {
            path: cache_file.clone(),
        })?;

        Ok(())
    }

    /// Get the filesystem path for the binary resolution cache file for a given [`ResolvedCrate`].
    ///
    /// The cache key includes the crate identity (name, version, source) and the current platform.
    /// This ensures that binaries are cached per-platform, which is essential since pre-built
    /// binaries are platform-specific.
    fn binary_cache_path(&self, krate: &ResolvedCrate) -> Result<PathBuf> {
        let hash = Self::compute_binary_cache_hash(krate)?;
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
    /// - Current platform triple
    ///
    /// This ensures that the same crate on different platforms gets different cache entries.
    fn compute_binary_cache_hash(krate: &ResolvedCrate) -> Result<String> {
        #[derive(Serialize)]
        struct BinaryCacheKey<'a> {
            name: &'a str,
            version: &'a semver::Version,
            source: &'a ResolvedSource,
            platform: &'a str,
        }

        let key = BinaryCacheKey {
            name: &krate.name,
            version: &krate.version,
            source: &krate.source,
            platform: build_context::TARGET,
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
    fn compute_source_hash(source: &ResolvedSource) -> String {
        let mut hasher = DefaultHasher::new();
        match source {
            ResolvedSource::CratesIo => {
                "crates-io".hash(&mut hasher);
            }
            ResolvedSource::Registry { source: registry } => {
                "registry".hash(&mut hasher);
                match registry {
                    RegistrySource::Named(name) => name.hash(&mut hasher),
                    RegistrySource::IndexUrl(url) => url.as_str().hash(&mut hasher),
                }
            }
            ResolvedSource::Git { repo, commit } => {
                "git".hash(&mut hasher);
                repo.hash(&mut hasher);
                commit.hash(&mut hasher);
            }
            ResolvedSource::Forge { forge, commit } => {
                "forge".hash(&mut hasher);
                // Format Debug output of forge for hashing
                format!("{:?}", forge).hash(&mut hasher);
                commit.hash(&mut hasher);
            }
            ResolvedSource::LocalDir { .. } => {
                panic!("BUG: Should not compute hash for LocalDir sources");
            }
        }
        format!("{:016x}", hasher.finish())
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
        let mut hasher = DefaultHasher::new();

        // Sort features for consistency - order shouldn't matter for cache key
        let mut features = options.features.clone();
        features.sort();
        features.hash(&mut hasher);

        options.all_features.hash(&mut hasher);
        options.no_default_features.hash(&mut hasher);
        options.profile.hash(&mut hasher);
        options.target.hash(&mut hasher);
        options.build_target.hash(&mut hasher);
        options.toolchain.hash(&mut hasher);

        // locked affects dependency resolution, which affects the binary
        options.locked.hash(&mut hasher);

        // Explicitly NOT hashing these fields as they don't affect the binary output:
        // - offline: affects network access, not binary
        // - jobs: affects build parallelism, not binary
        // - ignore_rust_version: affects cargo checks, not binary

        format!("{:016x}", hasher.finish())
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
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                ..Default::default()
            };
            let options2 = BuildOptions {
                target: Some("aarch64-unknown-linux-gnu".to_string()),
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
            assert_eq!(hash.len(), 16, "Hash should be 16 hex chars");
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

        #[test]
        fn cache_path_format_crates_io() {
            let (cache, _temp) = test_cache();
            let resolved = test_resolved();

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("crates-io"));
            assert!(path_str.contains("serde"));
            assert!(path_str.contains("1.0.0"));
        }

        #[test]
        fn cache_path_format_git() {
            let (cache, _temp) = test_cache();
            let resolved = ResolvedCrate {
                name: "test".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::Git {
                    repo: "https://github.com/test/test.git".to_string(),
                    commit: "abc123".to_string(),
                },
            };

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("git"));
            assert!(path_str.contains("abc123"));
        }

        #[test]
        fn cache_path_format_github() {
            let (cache, _temp) = test_cache();
            let resolved = ResolvedCrate {
                name: "test".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "owner".to_string(),
                        repo: "repo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            };

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("github"));
            assert!(path_str.contains("owner"));
            assert!(path_str.contains("repo"));
            assert!(path_str.contains("abc123"));
        }

        #[test]
        fn cache_path_format_gitlab() {
            let (cache, _temp) = test_cache();
            let resolved = ResolvedCrate {
                name: "test".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::Forge {
                    forge: Forge::GitLab {
                        custom_url: None,
                        owner: "owner".to_string(),
                        repo: "repo".to_string(),
                    },
                    commit: "def456".to_string(),
                },
            };

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("gitlab"));
            assert!(path_str.contains("owner"));
            assert!(path_str.contains("repo"));
            assert!(path_str.contains("def456"));
        }

        #[test]
        fn cache_path_format_registry_named() {
            let (cache, _temp) = test_cache();
            let resolved = ResolvedCrate {
                name: "test".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::Registry {
                    source: RegistrySource::Named("my-registry".to_string()),
                },
            };

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("registry"));
            assert!(path_str.contains("my-registry"));
            assert!(path_str.contains("test"));
            assert!(path_str.contains("1.0.0"));
        }

        #[test]
        fn cache_path_format_registry_index_url() {
            let (cache, _temp) = test_cache();
            let index_url = url::Url::parse("https://example.com/index").unwrap();
            let resolved = ResolvedCrate {
                name: "test".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                source: ResolvedSource::Registry {
                    source: RegistrySource::IndexUrl(index_url),
                },
            };

            let path = cache.crate_source_cache_path(&resolved).unwrap();
            let path_str = path.to_string_lossy();

            assert!(path_str.contains("sources"));
            assert!(path_str.contains("registry-index"));
            assert!(path_str.contains("test"));
            assert!(path_str.contains("1.0.0"));
        }
    }
}
