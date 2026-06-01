mod providers;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use providers::{BinstallProvider, GithubProvider, GitlabProvider, Provider, QuickinstallProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snafu::ResultExt;

use crate::{
    Result,
    builder::{BuildOptions, BuildTarget},
    cache::Cache,
    config::{BinaryProvider, Config, UsePrebuiltBinaries},
    crate_resolver::{ResolvedCrate, ResolvedSource},
    cratespec::RegistrySource,
    downloader::DownloadedCrate,
    error,
    http::HttpClient,
    messages::PrebuiltBinaryMessage,
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
    /// - `Ok(None)` - No pre-built binary available, or pre-built binaries are
    ///   disabled/disqualified
    /// - `Err(...)` - Resolution failed in a way that should stop execution
    fn resolve(
        &self,
        krate: &DownloadedCrate,
        build_options: &BuildOptions,
    ) -> Result<Option<ResolvedBinary>>;
}

/// Create the default [`BinaryResolver`] implementation, repecting the given config and using the
/// provided cache.
pub(crate) fn create_resolver(
    config: Config,
    cache: Cache,
    reporter: crate::messages::MessageReporter,
    http_client: HttpClient,
) -> impl BinaryResolver {
    let inner = DefaultBinaryResolver::new(config, reporter.clone(), http_client);
    CachingResolver::new(inner, cache, reporter)
}

struct DefaultBinaryResolver {
    config: Config,
    reporter: crate::messages::MessageReporter,
    http_client: HttpClient,
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

impl DefaultBinaryResolver {
    fn new(config: Config, reporter: crate::messages::MessageReporter, http_client: HttpClient) -> Self {
        Self {
            config,
            reporter,
            http_client,
        }
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
        // Compute source hash based on the resolved crate source
        let source_hash = Self::compute_source_hash(&krate.source);

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

        let binary_name = binary.path.file_name().ok_or_else(|| error::Error::Io {
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

    /// Compute a hash of the source for use in the `bin_dir` structure.
    fn compute_source_hash(source: &ResolvedSource) -> String {
        let mut hasher = Sha256::new();

        match source {
            ResolvedSource::CratesIo => {
                hasher.update(b"crates-io");
            }
            ResolvedSource::Registry { source } => {
                hasher.update(b"registry:");
                match source {
                    RegistrySource::Named(name) => {
                        hasher.update(b"named:");
                        hasher.update(name.as_bytes());
                    }
                    RegistrySource::IndexUrl(url) => {
                        hasher.update(b"index:");
                        hasher.update(url.as_str().as_bytes());
                    }
                }
            }
            ResolvedSource::Git { repo, commit } => {
                hasher.update(b"git:");
                hasher.update(repo.as_bytes());
                hasher.update(b":");
                hasher.update(commit.as_bytes());
            }
            ResolvedSource::Forge { forge, commit } => {
                hasher.update(b"forge:");
                hasher.update(format!("{:?}", forge).as_bytes());
                hasher.update(b":");
                hasher.update(commit.as_bytes());
            }
            ResolvedSource::LocalDir { path } => {
                hasher.update(b"local:");
                hasher.update(path.to_string_lossy().as_bytes());
            }
        }

        crate::helpers::format_hex_lower(hasher.finalize())[..16].to_string()
    }
}

impl BinaryResolver for DefaultBinaryResolver {
    fn resolve(
        &self,
        krate: &DownloadedCrate,
        _build_options: &BuildOptions,
    ) -> Result<Option<ResolvedBinary>> {
        let resolved = &krate.resolved;

        tracing::debug!(
            "BinaryResolver::resolve called for {}@{}",
            resolved.name,
            resolved.version
        );

        if self.config.prebuilt_binaries.binary_providers.is_empty() {
            return error::NoProvidersConfiguredSnafu.fail();
        }

        // Always use the build target platform for pre-built binaries
        // If the user overrides this by specifying a custom target, execution is not supposed to
        // make it to this point.
        let platform: &'static str = build_context::TARGET;

        let reporter = &self.reporter;
        let cache_dir = &self.config.cache_dir;
        let verify = self.config.prebuilt_binaries.verify_checksums;

        for provider_type in &self.config.prebuilt_binaries.binary_providers {
            reporter.report(|| PrebuiltBinaryMessage::checking_provider(resolved, *provider_type));

            let result = match provider_type {
                BinaryProvider::Binstall => BinstallProvider::new(
                    reporter.clone(),
                    cache_dir.clone(),
                    verify,
                    self.http_client.clone(),
                )
                .try_resolve(krate, platform),
                BinaryProvider::GithubReleases => GithubProvider::new(
                    reporter.clone(),
                    cache_dir.clone(),
                    verify,
                    self.http_client.clone(),
                )
                .try_resolve(krate, platform),
                BinaryProvider::GitlabReleases => GitlabProvider::new(
                    reporter.clone(),
                    cache_dir.clone(),
                    verify,
                    self.http_client.clone(),
                )
                .try_resolve(krate, platform),
                BinaryProvider::Quickinstall => {
                    QuickinstallProvider::new(reporter.clone(), cache_dir.clone(), self.http_client.clone())
                        .try_resolve(krate, platform)
                }
            };

            match result {
                Ok(Some(binary)) => {
                    let relocated_binary = self.relocate_to_bin_dir(binary, resolved, platform)?;
                    reporter.report(|| PrebuiltBinaryMessage::resolved(&relocated_binary));
                    return Ok(Some(relocated_binary));
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!("Provider {:?} error: {:?}", provider_type, e);
                    continue;
                }
            }
        }

        if self.config.prebuilt_binaries.use_prebuilt_binaries == UsePrebuiltBinaries::Always {
            return error::PrebuiltBinaryRequiredSnafu {
                name: resolved.name.clone(),
                version: resolved.version.to_string(),
            }
            .fail();
        }

        self.reporter.report(|| {
            PrebuiltBinaryMessage::no_binary_found(
                resolved,
                vec!["no binary found from any configured provider".to_string()],
            )
        });

        Ok(None)
    }
}

struct CachingResolver<R: BinaryResolver> {
    inner: R,
    cache: Cache,
    reporter: crate::messages::MessageReporter,
}

impl<R: BinaryResolver> CachingResolver<R> {
    fn new(inner: R, cache: Cache, reporter: crate::messages::MessageReporter) -> Self {
        Self {
            inner,
            cache,
            reporter,
        }
    }
}

impl<R: BinaryResolver> BinaryResolver for CachingResolver<R> {
    fn resolve(
        &self,
        krate: &DownloadedCrate,
        build_options: &BuildOptions,
    ) -> Result<Option<ResolvedBinary>> {
        // Check build options disqualification BEFORE touching cache
        if let Some(reason) = is_disqualified(build_options) {
            self.reporter
                .report(|| PrebuiltBinaryMessage::disqualified_due_to_customization(reason));
            return Ok(None);
        }

        // Delegate to cache (which handles Never mode and caching), keyed on the resolved crate
        self.cache
            .get_or_resolve_binary(&krate.resolved, || self.inner.resolve(krate, build_options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{BuildOptions, BuildTarget};

    /// Test that default build options are not disqualified
    #[test]
    fn test_disqualification_default_options_ok() {
        let options = BuildOptions::default();
        assert_eq!(is_disqualified(&options), None);
    }

    /// Test that explicit --bin flag disqualifies pre-built binaries
    #[test]
    fn test_disqualification_explicit_bin() {
        let mut options = BuildOptions::default();
        options.build_target = BuildTarget::Bin("specific-bin".to_string());
        assert_eq!(
            is_disqualified(&options),
            Some("explicit --bin or --example specified")
        );
    }

    /// Test that explicit --example flag disqualifies pre-built binaries
    #[test]
    fn test_disqualification_explicit_example() {
        let mut options = BuildOptions::default();
        options.build_target = BuildTarget::Example("my-example".to_string());
        assert_eq!(
            is_disqualified(&options),
            Some("explicit --bin or --example specified")
        );
    }

    /// Test that custom features disqualify pre-built binaries
    #[test]
    fn test_disqualification_custom_features() {
        let mut options = BuildOptions::default();
        options.features = vec!["serde".to_string(), "json".to_string()];
        assert_eq!(is_disqualified(&options), Some("custom features specified"));
    }

    /// Test that --all-features disqualifies pre-built binaries
    #[test]
    fn test_disqualification_all_features() {
        let mut options = BuildOptions::default();
        options.all_features = true;
        assert_eq!(is_disqualified(&options), Some("--all-features specified"));
    }

    /// Test that --no-default-features disqualifies pre-built binaries
    #[test]
    fn test_disqualification_no_default_features() {
        let mut options = BuildOptions::default();
        options.no_default_features = true;
        assert_eq!(is_disqualified(&options), Some("--no-default-features specified"));
    }

    /// Test that custom profile disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_profile() {
        let mut options = BuildOptions::default();
        options.profile = Some("release-with-debug".to_string());
        assert_eq!(is_disqualified(&options), Some("custom profile specified"));
    }

    /// Test that custom target disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_target() {
        let mut options = BuildOptions::default();
        options.target = Some("x86_64-unknown-linux-musl".to_string());
        assert_eq!(is_disqualified(&options), Some("custom target specified"));
    }

    /// Test that custom toolchain disqualifies pre-built binaries
    #[test]
    fn test_disqualification_custom_toolchain() {
        let mut options = BuildOptions::default();
        options.toolchain = Some("nightly".to_string());
        assert_eq!(is_disqualified(&options), Some("custom toolchain specified"));
    }
}
