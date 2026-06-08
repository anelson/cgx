use std::{borrow::Cow, path::PathBuf, sync::Arc};

use cargo_metadata::Target;
use snafu::ResultExt;

use crate::{
    Result,
    cache::Cache,
    cargo::{CargoMetadataOptions, CargoRunner, Metadata},
    config::Config,
    crate_resolver::ResolvedSource,
    cratespec::CrateSpec,
    downloader::DownloadedCrate,
    error,
};

/// Which executable within a crate to build.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BuildTarget {
    /// No specific target was requested. cgx builds Cargo's default binary target, using
    /// `default-run` when the package defines one.
    #[default]
    DefaultBin,

    /// A specific binary target to build.
    Bin(String),

    /// A specific example target to build.
    Example(String),
}

/// Build-related overrides (presumed to come from CLI arguments) that are merged with config
/// settings to produce final [`BuildOptions`] for building a crate.
#[derive(Clone, Debug, Default)]
pub struct BuildOverrides {
    /// Features to activate
    ///
    /// `None` means `--features` was not given; `Some(vec![])` means it was given but empty
    pub features: Option<Vec<String>>,

    /// Activate all available features
    pub all_features: bool,

    /// Do not activate the `default` feature
    pub no_default_features: bool,

    /// Build profile
    pub profile: Option<String>,

    /// Target triple for cross-compilation
    pub target: Option<String>,

    /// Number of parallel jobs for compilation
    pub jobs: Option<usize>,

    /// Ignore `rust-version` specification in packages
    pub ignore_rust_version: bool,

    /// Which executable within the crate to build
    pub target_selection: BuildTarget,

    /// Rust toolchain override
    pub toolchain: Option<String>,
}

/// Options that control how a crate is built.
///
/// These options map to flags passed to `cargo build`.
/// They are orthogonal to the crate identity and location (see [`crate::CrateSpec`]),
/// focusing instead on build configuration, feature selection, and compilation settings.
///
/// There is a somewhat blurry line between [`Config`] and [`BuildOptions`]; the intention is that
/// [`Config`] contains all parameters that can either be set via config file or overridden via CLI
/// argument, and encompasses parameters regulating all aspects of `cgx` behavior.  By contrast,
/// `BuildOptions` is specifically capturing options that effect how a crate is built; to a first
/// approximation you can think of this as a Rust struct that represents the args passed to `cargo
/// build` when building the user's desired crate from source.
///
/// There are some CLI args that are inherently crate-specifie (such as `--features`), which are
/// not present in [`Config`] and cannot be set in the config files; those naturally are captured
/// as part of `BuildOptions`.  However there are others like `--locked` and `--target` that can be
/// overridden for all crates via config file or applied to a specific invocation via CLI arg;
/// those are present here because they directly influence the command line passed to `cargo
/// build`, although they get populated from the [`Config`] struct which reflects settings in the
/// config files and any overrides of those settings that the user applied at the CLI.
///
/// Another way to reason about whether something should be in this struct or in [`Config`] is that
/// this struct implements [`Hash`], and that hash is used a cache key for caching build artifacts.
/// So if a field has a different value, should that invalidate any cached build artifacts and
/// cause a rebuild?  If so, then it probably belongs here.  If not, then it probably belongs in
/// [`Config`].  A good example of this is the `verbose` command, which literally translates into a
/// `cargo` command but is NOT considered a build option, because why would we rebuild a crate just
/// because the verbosity level is different?
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BuildOptions {
    /// Features to activate (corresponds to `--features`).
    pub features: Vec<String>,

    /// Activate all available features (corresponds to `--all-features`).
    pub all_features: bool,

    /// Do not activate the `default` feature (corresponds to `--no-default-features`).
    pub no_default_features: bool,

    /// Build profile to use (corresponds to `--profile`).
    ///
    /// When `None`, the default release profile is used.
    /// Use `Some("dev")` for debug builds.
    pub profile: Option<String>,

    /// Target triple for cross-compilation (corresponds to `--target`).
    pub target: Option<String>,

    /// Require that `Cargo.lock` remains unchanged (corresponds to `--locked`).
    pub locked: bool,

    /// Run without accessing the network (corresponds to `--offline`).
    pub offline: bool,

    /// Number of parallel jobs for compilation (corresponds to `-j`/`--jobs`).
    ///
    /// When `None`, cargo uses its default (number of CPUs).
    pub jobs: Option<usize>,

    /// Ignore `rust-version` specification in packages (corresponds to `--ignore-rust-version`).
    pub ignore_rust_version: bool,

    /// Which executable within the crate to build.
    pub build_target: BuildTarget,

    /// Rust toolchain override to use for this build (e.g., "nightly", "1.70.0", "stable").
    ///
    /// When set, Cargo is run through `rustup run <toolchain>`.
    pub toolchain: Option<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            profile: None,
            target: None,
            locked: true,
            offline: false,
            jobs: None,
            ignore_rust_version: false,
            build_target: BuildTarget::default(),
            toolchain: None,
        }
    }
}

impl BuildOptions {
    /// Merge build options from config and already-translated CLI overrides, with proper
    /// precedence.
    ///
    /// Config-handled settings (`locked`, `offline`, `toolchain`) come from [`Config`], which
    /// has already processed CLI overrides like `--locked`, `--unlocked`, `--frozen`, `--offline`.
    ///
    /// Crate-specific settings (features, profile, target, etc.) come from [`BuildOverrides`],
    /// which the CLI front-end has already produced from the raw arguments (tokenizing
    /// features, folding `--debug`, resolving `--bin`/`--example`).
    pub fn load(config: &Config, overrides: &BuildOverrides) -> Result<Self> {
        Ok(BuildOptions {
            // These come from the config settings, `Config` will already apply any CLI overrides
            locked: config.locked,
            offline: config.offline,

            // This can be set in the config file, but it can be overridden on the CLI
            toolchain: overrides.toolchain.clone().or_else(|| config.toolchain.clone()),

            // The rest of these come exclusively from the CLI overrides
            features: overrides.features.clone().unwrap_or_default(),
            all_features: overrides.all_features,
            no_default_features: overrides.no_default_features,
            profile: overrides.profile.clone(),
            target: overrides.target.clone(),
            jobs: overrides.jobs,
            ignore_rust_version: overrides.ignore_rust_version,
            build_target: overrides.target_selection.clone(),
        })
    }

    /// Load the build options for a specific crate.
    ///
    /// This will respect the `[tools]` section in the cgx config TOML, if there is an entry for
    /// this crate then the options specified for that crate will be applied unless they have been
    /// overridden in `BuildOverrides`.
    pub fn load_for_crate(
        config: &Config,
        overrides: &BuildOverrides,
        crate_spec: &CrateSpec,
    ) -> Result<Self> {
        // Load the standard build options from the CLI/config files, without any crate-specific
        // options.
        let mut options = Self::load(config, overrides)?;

        // Look up the tool-specific options for this crate, if any, and apply them if they are not
        // overridden by the build overrides from the CLI.
        //
        // At the moment the only option that can be specified in the `[tools]` section that has
        // any bearing on building is the selected features.  So, if features haven't been explicitly
        // overridden on the command line, but features were specified in the config for this crate,
        // then use those features.
        if let Some(crate_name) = crate_spec.configured_tool_name() {
            if let Some(tool_config) = config.tools.get(crate_name) {
                if overrides.features.is_none() {
                    if let Some(features) = tool_config.features() {
                        options.features = features.to_vec();
                    }
                }
            }
        }

        Ok(options)
    }

    /// The resolved Rust target triple this build targets: the explicit `--target`, or cgx's own
    /// host triple ([`build_context::TARGET`]) when none was given.
    ///
    /// This matches how cgx resolves the platform everywhere else (pre-built binary lookup, cargo
    /// metadata filtering, and the build cache), so it reliably names the triple the binary is for
    /// even on a default build where cargo writes to `target/debug` rather than a triple subdir.
    pub(crate) fn target_platform(&self) -> String {
        self.target
            .clone()
            .unwrap_or_else(|| build_context::TARGET.to_string())
    }
}

pub trait CrateBuilder {
    /// List the targets in the given crate that can be built using [`Self::build`].
    ///
    /// [`Self::build`] can build any bin or example target in the crate.
    ///
    /// Returns a tuple of:
    /// - The package's explicit `default-run` target, if any
    /// - A list of all binary targets
    /// - A list of all example targets
    fn list_targets(
        &self,
        krate: &DownloadedCrate,
        options: &BuildOptions,
    ) -> Result<(Option<Target>, Vec<Target>, Vec<Target>)>;

    /// Produce a compiled binary from the given crate, using the specified build options.
    ///
    /// Builds from registry and git sources can be cached. Local directory builds run directly from
    /// the local source tree.  So this may or may not actually compile anything,
    /// depending on the crate source, the state of the cache, and the config.
    ///
    /// Returns the full path to the compiled binary and the concrete [`BuildTarget`] that was built
    /// (a `DefaultBin` request is resolved to the actual `Bin`/`Example` here, even on a cache
    /// hit).
    fn build(&self, krate: &DownloadedCrate, options: &BuildOptions) -> Result<(PathBuf, BuildTarget)>;
}

pub(crate) fn create_builder(
    config: Config,
    cache: Cache,
    cargo_runner: Arc<dyn CargoRunner>,
) -> impl CrateBuilder {
    RealCrateBuilder {
        config,
        cache,
        cargo_runner,
    }
}

/// Builder which is responsible for compiling a specific binary target in a crate, from source.
struct RealCrateBuilder {
    config: Config,
    cache: Cache,
    cargo_runner: Arc<dyn CargoRunner>,
}

impl CrateBuilder for RealCrateBuilder {
    fn list_targets(
        &self,
        krate: &DownloadedCrate,
        options: &BuildOptions,
    ) -> Result<(Option<Target>, Vec<Target>, Vec<Target>)> {
        let metadata = self
            .cargo_runner
            .metadata(&krate.crate_path, &CargoMetadataOptions::from(options))?;

        Self::list_targets_internal(krate, &metadata)
    }

    fn build(&self, krate: &DownloadedCrate, options: &BuildOptions) -> Result<(PathBuf, BuildTarget)> {
        // Gather metadata about the crate in its current source form.
        // The act of building will re-gather the metadata after the build, but this is needed to
        // resolve target and package information before building.
        let metadata = self
            .cargo_runner
            .metadata(&krate.crate_path, &CargoMetadataOptions::from(options))?;

        // If the user has not specified an explicit binary target, attempt to resolve it now.
        // If the crate has multiple (or no) binary targets, this is the time to fail fast.
        // Plus the cache needs to know the actual binary name, not DefaultBin.
        let options: Cow<'_, BuildOptions> = if matches!(options.build_target, BuildTarget::DefaultBin) {
            Cow::Owned(BuildOptions {
                build_target: Self::resolve_binary_target(krate, options, &metadata)?,
                ..options.clone()
            })
        } else {
            Cow::Borrowed(options)
        };

        // The build target (ie, which binary or example to run) is now known, whether resolved
        // just above or supplied explicitly. Report it alongside the binary, including on a cache
        // hit.
        let built_target = options.build_target.clone();

        // Crates resolved from local sources are, by definition, local.  Not only does that mean
        // that they are on a local filesystem (and presumably fast to access), but it also means
        // that their source contents are mutable.  Even if we wanted to cache them, we would need
        // a way to detect if any changes had occurred since the last build (basically what `cargo
        // build` does), and that doesn't seem worth it.  So local crates are always built directly
        // from their sources, and never cached
        if matches!(krate.resolved.source, ResolvedSource::LocalDir { .. }) {
            let (binary_path, _sbom) = self.build_uncached(krate, options.as_ref(), &metadata)?;
            return Ok((binary_path, built_target));
        }

        let binary_path = self
            .cache
            .get_or_build_binary(&krate.resolved, options.as_ref(), || {
                self.build_uncached(krate, options.as_ref(), &metadata)
            })?;
        Ok((binary_path, built_target))
    }
}

impl RealCrateBuilder {
    /// List the targets in the given crate that can be build using [`Self::build`].
    ///
    /// Unlike the public [`CrateBuilder::list_targets`], this internal version takes the cargo
    /// metadata as an argument, allowing it to be reused and avoid redundant metadata queries.
    fn list_targets_internal(
        krate: &DownloadedCrate,
        metadata: &Metadata,
    ) -> Result<(Option<Target>, Vec<Target>, Vec<Target>)> {
        // Find the crate package in metadata
        let package = metadata
            .packages
            .iter()
            .find(|p| p.name.as_str() == krate.resolved.name)
            .ok_or_else(|| {
                error::PackageNotFoundInWorkspaceSnafu {
                    name: krate.resolved.name.clone(),
                    available: metadata
                        .packages
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect::<Vec<_>>(),
                }
                .build()
            })?;

        // Get all bin and example targets in the package, since those are the only kinds that we
        // support running with `cgx`
        let bin_targets: Vec<_> = package
            .targets
            .iter()
            .filter(|t| {
                t.kind
                    .iter()
                    .any(|k| matches!(k, cargo_metadata::TargetKind::Bin))
            })
            .cloned()
            .collect();
        let example_targets: Vec<_> = package
            .targets
            .iter()
            .filter(|t| {
                t.kind
                    .iter()
                    .any(|k| matches!(k, cargo_metadata::TargetKind::Example))
            })
            .cloned()
            .collect();

        // If an explicit bin was specified in `default_run`, use that as the default target
        let default = package.default_run.as_ref().and_then(|default_run| {
            bin_targets
                .iter()
                .find(|t| t.name == default_run.as_str())
                .cloned()
        });

        Ok((default, bin_targets, example_targets))
    }

    /// Resolve [`BuildTarget`] to an actual binary name before building or caching.
    ///
    /// This not only validates that, if an explicit target was specified, that it actually exists,
    /// but also resolves the `DefaultBin` case to a specific binary name.
    ///
    /// Returns an explicit [`BuildTarget`] guaranteed not to be `DefaultBin`, or an error if
    /// resolution fails.
    fn resolve_binary_target(
        krate: &DownloadedCrate,
        options: &BuildOptions,
        metadata: &Metadata,
    ) -> Result<BuildTarget> {
        let (default, bins, examples) = Self::list_targets_internal(krate, metadata)?;

        // If no explicit target was specified but the crate package has `default_run`, use that
        let build_target = if matches!(options.build_target, BuildTarget::DefaultBin) {
            if let Some(default) = default {
                BuildTarget::Bin(default.name.clone())
            } else {
                BuildTarget::DefaultBin
            }
        } else {
            options.build_target.clone()
        };

        // Select a specific build target.  There are a few possible permutations here:
        // - The user didn't explicitly ask for a particular target, but the package has a
        // `default_run`, so act like the user specified that explicitly and proceed further.
        // - The user specified an explicit bin or example; just need to verify that it's in the
        // runnable targets, fail if it's not, then we're good
        // - The user didn't explicitly ask for a particular target, and the package does not have
        // a `default_run`.  If the package has exactly one binary, use that.  If it has no
        // binaries, fail.  If it has multiple binaries, fail.

        match build_target {
            BuildTarget::DefaultBin => {
                // No explicit target, no default_run - must have exactly one binary
                match bins.len() {
                    0 => {
                        // No binary targets - this will fail later when cargo tries to build
                        error::NoPackageBinariesSnafu {
                            krate: krate.resolved.name.clone(),
                        }
                        .fail()
                    }
                    1 => {
                        // Exactly one binary, use it
                        Ok(BuildTarget::Bin(bins[0].name.clone()))
                    }
                    _ => {
                        // Multiple binaries - ambiguous
                        error::AmbiguousBinaryTargetSnafu {
                            package: krate.resolved.name.clone(),
                            available: bins.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                        }
                        .fail()
                    }
                }
            }
            BuildTarget::Bin(ref name) => {
                // Explicit binary target - verify it exists
                if bins.iter().any(|t| t.name == *name) {
                    Ok(build_target)
                } else {
                    error::RunnableTargetNotFoundSnafu {
                        kind: "binary",
                        package: krate.resolved.name.clone(),
                        target: name.clone(),
                        available: bins.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    }
                    .fail()
                }
            }
            BuildTarget::Example(ref name) => {
                // Explicit example target - verify it exists
                if examples.iter().any(|t| t.name == *name) {
                    Ok(build_target)
                } else {
                    error::RunnableTargetNotFoundSnafu {
                        kind: "example",
                        package: krate.resolved.name.clone(),
                        target: name.clone(),
                        available: bins.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    }
                    .fail()
                }
            }
        }
    }

    /// Build the crate from source as-is, as well as the SBOM for the as-built crate, without any
    /// caching.
    ///
    /// Uses metadata previously gathered from the crate to resolve the package name containing the
    /// crate, but beware that the act of building can and often does modify the metadata,
    /// particularly if there is no Cargo.lock in the source package or if it's out of date and
    /// needs to be updated.
    ///
    /// ## Cargo.lock handling
    ///
    /// We control whether cargo uses locked dependencies via two mechanisms:
    ///
    /// - File presence (`prepare_build_dir`): If options.locked is false, we delete Cargo.lock
    ///   before building, forcing cargo to resolve dependencies fresh.
    ///
    /// - --locked flag (passed to `cargo build` in cargo.rs): If options.locked is true, cargo.rs
    ///   passes --locked to `cargo build`, making it strictly honor the Cargo.lock and fail if
    ///   inconsistent.
    ///
    /// This two-part approach mimics `cargo install` behavior:
    /// - `cargo install --locked`: keeps Cargo.lock + enforces strict adherence (via
    ///   `ws.set_ignore_lock(false)`)
    /// - `cargo install`: ignores/regenerates Cargo.lock with latest compatible versions
    ///
    /// ## Returns
    ///
    /// Returns a tuple of (`binary_path`, `sbom`) where `sbom` is generated from metadata
    /// read from the build directory AFTER the build completes. This ensures the SBOM
    /// reflects the actual dependencies that were resolved and built, not what was
    /// in the source directory's Cargo.lock.
    fn build_uncached(
        &self,
        krate: &DownloadedCrate,
        options: &BuildOptions,
        metadata: &Metadata,
    ) -> Result<(PathBuf, crate::sbom::CycloneDx)> {
        let build_dir = self.prepare_build_dir(krate, options)?;

        let package_name = Self::resolve_package_name(metadata, &krate.resolved.name)?;

        let binary_path = self
            .cargo_runner
            .build(&build_dir, package_name.as_deref(), options)?;

        // Re-read metadata from the build directory AFTER building. This is critical for accurate
        // SBOM generation: if --unlocked was used, Cargo.lock was deleted from the build dir and
        // cargo created a new one with freshly resolved dependencies. Even absent `--unlocked`, if
        // the crate didn't ship with a Cargo.lock or it was outdated, the act of building will
        // update the lock file and resolve potentially different dependencies when building the
        // crate.  Since the SBOM must reflect those actual dependencies, not the stale ones from
        // the source directory, we need to re-read the metadata here.
        let metadata = self
            .cargo_runner
            .metadata(&build_dir, &CargoMetadataOptions::from(options))?;

        // Generate SBOM from the post-build metadata
        let sbom = crate::sbom::generate_sbom(&metadata, &krate.resolved, options)?;

        Ok((binary_path, sbom))
    }

    /// Prepare a build directory from which the crate can be built.
    ///
    /// If the crate is in a local path, then that path is returned directly, meaning what we will
    /// do is equivalent to running `cargo build --release` in that directory.
    ///
    /// For all other crates (e.g., from crates.io or git), a temporary directory is created in the
    /// build dir, and the crate's source files are copied there.  This ensures that any build
    /// artifacts (e.g., `target` directory) are created in a location that is not under the
    /// user's source tree. The temporary directory is not automatically deleted, but is left
    /// for inspection.
    ///
    /// TODO: Fix this so that build dirs are cleaned up after successful builds.
    fn prepare_build_dir(&self, krate: &DownloadedCrate, options: &BuildOptions) -> Result<PathBuf> {
        if let ResolvedSource::LocalDir { .. } = krate.resolved.source {
            return Ok(krate.crate_path.clone());
        }

        std::fs::create_dir_all(&self.config.build_dir).with_context(|_| error::IoSnafu {
            path: self.config.build_dir.clone(),
        })?;

        let temp_dir = tempfile::Builder::new()
            .prefix(&format!("cgx-build-{}", &krate.resolved.name))
            .tempdir_in(&self.config.build_dir)
            .with_context(|_| error::TempDirCreationSnafu {
                parent: self.config.build_dir.clone(),
            })?;

        let temp_path = temp_dir.path().to_path_buf();
        crate::helpers::copy_source_tree(&krate.crate_path, &temp_path)?;

        // If locked is false (--unlocked was passed), delete Cargo.lock from copied source builds
        // to force Cargo to resolve dependencies fresh.
        if !options.locked {
            let lock_path = temp_path.join("Cargo.lock");
            if lock_path.exists() {
                std::fs::remove_file(&lock_path).with_context(|_| error::IoSnafu { path: lock_path })?;
            }
        }

        let _ = temp_dir.keep();
        Ok(temp_path)
    }

    /// Given metadata for a workspace and the name of a crate, determine the appropriate
    /// `--package` argument to pass to cargo, if any.
    ////
    /// If the workspace has zero or one members, then no `--package` argument is needed, so
    /// `Ok(None)` is returned.  If the workspace has multiple members, then the crate name must
    /// match one of them, and `Ok(Some(name))` is returned.  If it does not match any, then an
    /// error is returned.
    fn resolve_package_name(metadata: &Metadata, crate_name: &str) -> Result<Option<String>> {
        let workspace_members: Vec<_> = metadata
            .workspace_packages()
            .iter()
            .map(|p| p.name.as_str())
            .collect();

        match workspace_members.len() {
            0 | 1 => Ok(None),
            _ => {
                if workspace_members.iter().any(|name| *name == crate_name) {
                    Ok(Some(crate_name.to_string()))
                } else {
                    error::PackageNotFoundInWorkspaceSnafu {
                        name: crate_name.to_string(),
                        available: workspace_members
                            .into_iter()
                            .map(String::from)
                            .collect::<Vec<_>>(),
                    }
                    .fail()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use assert_matches::assert_matches;
    use semver::Version;

    use super::*;
    use crate::{
        cargo::create_cargo_runner,
        crate_resolver::{ResolvedCrate, ResolvedSource},
        error::Error,
        testdata::CrateTestCase,
    };

    fn test_builder() -> (RealCrateBuilder, tempfile::TempDir) {
        crate::logging::init_test_logging();

        let (temp_dir, config) = crate::config::create_test_env();

        fs::create_dir_all(&config.cache_dir).unwrap();
        fs::create_dir_all(&config.bin_dir).unwrap();
        fs::create_dir_all(&config.build_dir).unwrap();

        let cache = Cache::new(config.clone(), crate::messages::MessageReporter::null());
        let cargo_runner =
            Arc::new(create_cargo_runner(config.clone(), crate::messages::MessageReporter::null()).unwrap());

        let builder = RealCrateBuilder {
            config,
            cache,
            cargo_runner,
        };

        (builder, temp_dir)
    }

    /// Type of fake source to create for testing
    #[derive(Debug, Clone)]
    enum FakeSourceType {
        Registry { version: String },
        Git { url: String, rev: String },
        LocalDir,
    }

    /// Create a fake [`DownloadedCrate`] from a [`CrateTestCase`] for testing different source
    /// types
    fn fake_downloaded_crate(
        tc: &CrateTestCase,
        source_type: FakeSourceType,
        package_name: Option<&str>,
    ) -> DownloadedCrate {
        let (resolved_source, crate_path) = match &source_type {
            FakeSourceType::Registry { .. } => {
                // Registry sources only contain the specific crate, not the whole workspace
                let path = if let Some(pkg) = package_name {
                    tc.path().join(pkg)
                } else {
                    tc.path().to_path_buf()
                };
                (ResolvedSource::CratesIo, path)
            }
            FakeSourceType::Git { url, rev } => {
                // Git sources can contain workspaces
                (
                    ResolvedSource::Git {
                        repo: url.clone(),
                        commit: rev.clone(),
                    },
                    tc.path().to_path_buf(),
                )
            }
            FakeSourceType::LocalDir => {
                // LocalDir sources use the path directly
                let path = tc.path().to_path_buf();
                (ResolvedSource::LocalDir { path: path.clone() }, path)
            }
        };

        let name = package_name.unwrap_or(tc.name).to_string();
        let version = match &source_type {
            FakeSourceType::Registry { version } => Version::parse(version).unwrap(),
            _ => Version::parse("0.1.0").unwrap(),
        };

        DownloadedCrate {
            resolved: ResolvedCrate {
                name,
                version,
                source: resolved_source,
            },
            crate_path,
        }
    }

    /// Return the SBOM path for a built binary in the cache.
    fn read_sbom_for_binary(binary_path: &Path) -> PathBuf {
        // SBOM is stored at same level as binary with name "sbom.cyclonedx.json"
        binary_path.parent().unwrap().join("sbom.cyclonedx.json")
    }

    /// Get the expected binary name for the current platform.
    ///
    /// On Windows, appends ".exe" extension. On Unix, returns the name unchanged.
    fn expected_bin_name(base_name: &str) -> String {
        format!("{}{}", base_name, std::env::consts::EXE_SUFFIX)
    }

    /// Assert that two builds resulted in a cache hit (same path, same mtime)
    fn assert_cache_hit(path1: &Path, path2: &Path) {
        assert_eq!(
            path1,
            path2,
            "Cache hit expected: paths should be identical\n  path1: {}\n  path2: {}",
            path1.display(),
            path2.display()
        );

        let mtime1 = fs::metadata(path1).unwrap().modified().unwrap();
        let mtime2 = fs::metadata(path2).unwrap().modified().unwrap();

        assert_eq!(
            mtime1,
            mtime2,
            "Cache hit expected: modification times should be identical\n  path1: {}\n  path2: {}",
            path1.display(),
            path2.display()
        );
    }

    /// Assert that two builds resulted in a cache miss (different path OR different mtime)
    fn assert_cache_miss(path1: &Path, path2: &Path) {
        let paths_differ = path1 != path2;
        let mtimes_differ = if path1.exists() && path2.exists() {
            let mtime1 = fs::metadata(path1).unwrap().modified().unwrap();
            let mtime2 = fs::metadata(path2).unwrap().modified().unwrap();
            mtime1 != mtime2
        } else {
            true
        };

        assert!(
            paths_differ || mtimes_differ,
            "Cache miss expected: paths or mtimes should differ\n  path1: {}\n  path2: {}\n  paths_differ: \
             {}\n  mtimes_differ: {}",
            path1.display(),
            path2.display(),
            paths_differ,
            mtimes_differ
        );
    }

    /// Output from running the timestamp test binary.
    #[derive(Debug)]
    struct TimestampOutput {
        build_timestamp: String,
        features: Vec<String>,
    }

    /// Run the timestamp binary and parse its output.
    fn run_timestamp_binary(path: &Path) -> TimestampOutput {
        let output = std::process::Command::new(path)
            .output()
            .unwrap_or_else(|e| panic!("Failed to execute timestamp binary at {}: {}", path.display(), e));

        assert!(
            output.status.success(),
            "Timestamp binary failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut build_timestamp = None;
        let mut features = Vec::new();

        for line in stdout.lines() {
            if let Some(ts) = line.strip_prefix("Built at: ") {
                build_timestamp = Some(ts.to_string());
            }
            if let Some(feat_str) = line.strip_prefix("Features enabled: ") {
                if feat_str != "none" {
                    features = feat_str.split(", ").map(|s| s.to_string()).collect();
                }
            }
        }

        TimestampOutput {
            build_timestamp: build_timestamp.expect("No 'Built at:' line in timestamp output"),
            features,
        }
    }

    /// Assert that two builds hit cache by comparing timestamps (should be identical).
    fn assert_cache_hit_by_timestamp(output1: &TimestampOutput, output2: &TimestampOutput) {
        assert_eq!(
            output1.build_timestamp, output2.build_timestamp,
            "Cache hit expected: build timestamps should match\n  ts1: {}\n  ts2: {}",
            output1.build_timestamp, output2.build_timestamp
        );
    }

    /// Assert that two builds missed cache by comparing timestamps (should differ).
    fn assert_cache_miss_by_timestamp(output1: &TimestampOutput, output2: &TimestampOutput) {
        assert_ne!(
            output1.build_timestamp, output2.build_timestamp,
            "Cache miss expected: build timestamps should differ\n  ts1: {}\n  ts2: {}",
            output1.build_timestamp, output2.build_timestamp
        );
    }

    mod smoke_tests {
        use super::*;

        #[test]
        fn builds_all_testcases_with_bins() {
            let (builder, _temp) = test_builder();
            let cargo_runner =
                create_cargo_runner(Config::default(), crate::messages::MessageReporter::null()).unwrap();

            for tc in CrateTestCase::all() {
                let metadata_opts = CargoMetadataOptions::default();
                let metadata = cargo_runner.metadata(tc.path(), &metadata_opts).unwrap();

                let workspace_pkgs = metadata.workspace_packages();
                let buildable_packages: Vec<_> = workspace_pkgs
                    .iter()
                    .filter(|pkg| {
                        pkg.targets.iter().any(|t| {
                            t.kind
                                .iter()
                                .any(|k| matches!(k, cargo_metadata::TargetKind::Bin))
                        })
                    })
                    .collect();

                if buildable_packages.is_empty() {
                    continue;
                }

                for pkg in buildable_packages {
                    let krate = fake_downloaded_crate(
                        &tc,
                        FakeSourceType::Registry {
                            version: "1.0.0".to_string(),
                        },
                        Some(&pkg.name),
                    );

                    let options = BuildOptions {
                        profile: Some("dev".to_string()),
                        ..Default::default()
                    };

                    let result = builder.build(&krate, &options);

                    if let Ok((binary, _target)) = result {
                        assert!(binary.exists(), "Binary missing for {}/{}", tc.name, pkg.name);

                        let binary_name = binary.file_name().unwrap().to_str().unwrap();

                        // Determine expected binary name based on package metadata
                        let bin_targets: Vec<_> = pkg
                            .targets
                            .iter()
                            .filter(|t| {
                                t.kind
                                    .iter()
                                    .any(|k| matches!(k, cargo_metadata::TargetKind::Bin))
                            })
                            .collect();

                        let expected_name = if bin_targets.len() == 1 {
                            // Single binary - use its name
                            bin_targets[0].name.as_str()
                        } else if let Some(ref default_run) = pkg.default_run {
                            // Multiple binaries with default - use default
                            default_run.as_str()
                        } else {
                            // Multiple binaries without default - should have failed
                            panic!(
                                "Build succeeded for {}/{} but should have failed due to ambiguous binary \
                                 target",
                                tc.name, pkg.name
                            );
                        };

                        assert_eq!(
                            binary_name,
                            expected_bin_name(expected_name),
                            "Wrong binary name for {}/{}: expected '{}', got '{}'",
                            tc.name,
                            pkg.name,
                            expected_name,
                            binary_name
                        );
                    }
                }
            }
        }

        #[test]
        fn simple_bin_no_deps_from_registry() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::simple_bin_no_deps();
            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();

            assert!(binary.exists());
            assert!(binary.is_file());
            assert!(binary.starts_with(&builder.config.bin_dir));

            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("simple-bin-no-deps"));
        }
    }

    mod binary_selection {
        use super::*;

        #[test]
        fn default_bin_selected_automatically() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::single_crate_multiple_bins_with_default();
            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                build_target: BuildTarget::DefaultBin,
                ..Default::default()
            };

            let (binary, target) = builder.build(&krate, &options).unwrap();
            assert!(binary.exists());
            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                binary_name,
                expected_bin_name("bin1"),
                "Should build bin1 or the crate's default binary, got: {}",
                binary_name
            );

            // The `DefaultBin` request is resolved to the concrete target that was built.
            assert_eq!(target, BuildTarget::Bin("bin1".to_string()));
        }

        #[test]
        fn explicit_bin_overrides_default() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::single_crate_multiple_bins_with_default();
            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                build_target: BuildTarget::Bin("bin2".to_string()),
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            assert!(binary.exists());
            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("bin2"));
        }

        #[test]
        fn multiple_bins_without_default_fails() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::single_crate_multiple_bins();
            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let result = builder.build(&krate, &options);

            assert_matches!(
                result,
                Err(Error::AmbiguousBinaryTarget { ref package, ref available })
                    if package == "single-crate-multiple-bins"
                        && available.len() == 2
                        && available.contains(&"bin1".to_string())
                        && available.contains(&"bin2".to_string())
            );
        }
    }

    mod workspace_handling {
        use super::*;

        #[test]
        fn workspace_with_correct_package_succeeds() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::workspace_multiple_bin_crates();
            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Git {
                    url: "https://github.com/example/test.git".to_string(),
                    rev: "abc123".to_string(),
                },
                Some("bin1"),
            );

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            assert!(binary.exists());

            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("bin1"));
        }

        #[test]
        fn workspace_with_wrong_package_fails() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::workspace_multiple_bin_crates();

            let krate = DownloadedCrate {
                resolved: ResolvedCrate {
                    name: "nonexistent-package".to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    source: ResolvedSource::CratesIo,
                },
                crate_path: tc.path().to_path_buf(),
            };

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let result = builder.build(&krate, &options);

            assert_matches!(
                result,
                Err(Error::PackageNotFoundInWorkspace { ref name, ref available })
                    if name == "nonexistent-package" && !available.is_empty()
            );
        }
    }

    mod cache_functional {
        use super::*;

        #[test]
        fn identical_builds_hit_cache() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::timestamp();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary1, _target) = builder.build(&krate1, &options).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("timestamp"));
            let output1 = run_timestamp_binary(&binary1);

            std::thread::sleep(std::time::Duration::from_millis(100));

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let (binary2, _target) = builder.build(&krate2, &options).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("timestamp"));
            let output2 = run_timestamp_binary(&binary2);

            assert_cache_hit_by_timestamp(&output1, &output2);
            assert_cache_hit(&binary1, &binary2);
        }

        #[test]
        fn different_profile_cache_miss() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::timestamp();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options1 = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };
            let (binary1, _target) = builder.build(&krate1, &options1).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("timestamp"));
            let output1 = run_timestamp_binary(&binary1);

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options2 = BuildOptions {
                profile: Some("release".to_string()),
                ..Default::default()
            };
            let (binary2, _target) = builder.build(&krate2, &options2).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("timestamp"));
            let output2 = run_timestamp_binary(&binary2);

            assert_cache_miss_by_timestamp(&output1, &output2);
            assert_cache_miss(&binary1, &binary2);
        }

        #[test]
        fn different_target_cache_miss() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::simple_bin_no_deps();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options1 = BuildOptions {
                profile: Some("dev".to_string()),
                target: None,
                ..Default::default()
            };
            let (binary1, _target) = builder.build(&krate1, &options1).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("simple-bin-no-deps"));

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options2 = BuildOptions {
                profile: Some("dev".to_string()),
                target: Some(build_context::TARGET.to_string()),
                ..Default::default()
            };
            let (binary2, _target) = builder.build(&krate2, &options2).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("simple-bin-no-deps"));

            assert_cache_miss(&binary1, &binary2);
        }
    }

    mod dependency_resolution {
        use super::*;
        use crate::sbom::tests::get_sbom_component_version;

        #[test]
        fn locked_vs_unlocked_produces_different_cache_entries() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::stale_serde();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options1 = BuildOptions {
                profile: Some("dev".to_string()),
                locked: true,
                ..Default::default()
            };
            let (binary1, _target) = builder.build(&krate1, &options1).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("stale-serde"));
            let sbom1 = read_sbom_for_binary(&binary1);

            assert_eq!(
                get_sbom_component_version(&sbom1, "serde"),
                Some("1.0.5".to_string()),
                "With --locked, should use old serde from Cargo.lock"
            );

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options2 = BuildOptions {
                profile: Some("dev".to_string()),
                locked: false,
                ..Default::default()
            };
            let (binary2, _target) = builder.build(&krate2, &options2).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("stale-serde"));
            let sbom2 = read_sbom_for_binary(&binary2);

            let version = get_sbom_component_version(&sbom2, "serde").unwrap();
            assert_ne!(
                version, "1.0.5",
                "Without --locked, should resolve to newer serde"
            );
            assert!(version.starts_with("1.0."), "Should still be serde 1.0.x");

            crate::sbom::tests::assert_sboms_ne(&sbom1, &sbom2);
            assert_cache_miss(&binary1, &binary2);
        }

        #[test]
        fn same_locked_flag_produces_cache_hit() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::stale_serde();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                locked: true,
                ..Default::default()
            };

            let (binary1, _target) = builder.build(&krate1, &options).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("stale-serde"));

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );

            let (binary2, _target) = builder.build(&krate2, &options).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("stale-serde"));

            assert_cache_hit(&binary1, &binary2);
        }

        #[test]
        fn different_features_different_dependencies() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::timestamp();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options1 = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };
            let (binary1, _target) = builder.build(&krate1, &options1).unwrap();
            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("timestamp"));
            let sbom1 = read_sbom_for_binary(&binary1);
            let output1 = run_timestamp_binary(&binary1);

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options2 = BuildOptions {
                profile: Some("dev".to_string()),
                features: vec!["frobnulator".to_string()],
                no_default_features: true,
                ..Default::default()
            };
            let (binary2, _target) = builder.build(&krate2, &options2).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("timestamp"));
            let sbom2 = read_sbom_for_binary(&binary2);
            let output2 = run_timestamp_binary(&binary2);

            assert!(output1.features.contains(&"gonkolator".to_string()));
            assert!(output2.features.contains(&"frobnulator".to_string()));

            crate::sbom::tests::assert_sboms_ne(&sbom1, &sbom2);
            assert_cache_miss_by_timestamp(&output1, &output2);
        }

        #[test]
        fn all_features_includes_all_dependencies() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::timestamp();

            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                all_features: true,
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("timestamp"));
            let output = run_timestamp_binary(&binary);

            assert!(
                output.features.contains(&"gonkolator".to_string()),
                "Should have gonkolator"
            );
            assert!(
                output.features.contains(&"frobnulator".to_string()),
                "Should have frobnulator"
            );
        }

        #[test]
        fn default_is_locked_true() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::stale_serde();

            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions::default();

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            let sbom = read_sbom_for_binary(&binary);

            assert_eq!(
                get_sbom_component_version(&sbom, "serde"),
                Some("1.0.5".to_string()),
                "Default (locked=true) should honor Cargo.lock"
            );
        }

        #[test]
        fn frozen_honors_cargo_lock_and_is_offline() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::stale_serde();

            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                locked: true,
                offline: true,
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            let sbom = read_sbom_for_binary(&binary);

            assert_eq!(
                get_sbom_component_version(&sbom, "serde"),
                Some("1.0.5".to_string()),
                "Frozen should honor Cargo.lock"
            );

            assert!(options.offline, "Frozen should set offline mode");
        }
    }

    mod source_types {
        use super::*;

        #[test]
        fn local_dir_never_cached() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::simple_bin_no_deps();

            let krate = fake_downloaded_crate(&tc, FakeSourceType::LocalDir, None);

            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();

            assert!(!binary.starts_with(&builder.config.bin_dir));
            assert!(binary.starts_with(tc.path()));

            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("simple-bin-no-deps"));

            let sbom_path = read_sbom_for_binary(&binary);
            assert!(!sbom_path.exists());
        }

        #[test]
        fn registry_source_cached_with_sbom() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::simple_bin_no_deps();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary1, _target) = builder.build(&krate1, &options).unwrap();

            assert!(binary1.starts_with(&builder.config.bin_dir));

            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("simple-bin-no-deps"));

            let sbom_path = read_sbom_for_binary(&binary1);
            assert!(sbom_path.exists());

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let (binary2, _target) = builder.build(&krate2, &options).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("simple-bin-no-deps"));

            assert_cache_hit(&binary1, &binary2);
        }

        #[test]
        fn git_source_cached_with_sbom() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::simple_bin_no_deps();

            let krate1 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Git {
                    url: "https://github.com/example/test.git".to_string(),
                    rev: "abc123".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary1, _target) = builder.build(&krate1, &options).unwrap();

            assert!(binary1.starts_with(&builder.config.bin_dir));

            let binary1_name = binary1.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary1_name, expected_bin_name("simple-bin-no-deps"));

            let sbom_path = read_sbom_for_binary(&binary1);
            assert!(sbom_path.exists());

            let krate2 = fake_downloaded_crate(
                &tc,
                FakeSourceType::Git {
                    url: "https://github.com/example/test.git".to_string(),
                    rev: "abc123".to_string(),
                },
                None,
            );
            let (binary2, _target) = builder.build(&krate2, &options).unwrap();
            let binary2_name = binary2.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary2_name, expected_bin_name("simple-bin-no-deps"));

            assert_cache_hit(&binary1, &binary2);
        }
    }

    mod proc_macro_detection {
        use super::*;

        #[test]
        fn proc_macro_marked_as_build_dep() {
            let (builder, _temp) = test_builder();
            let tc = CrateTestCase::proc_macro_dep();

            let krate = fake_downloaded_crate(
                &tc,
                FakeSourceType::Registry {
                    version: "1.0.0".to_string(),
                },
                None,
            );
            let options = BuildOptions {
                profile: Some("dev".to_string()),
                ..Default::default()
            };

            let (binary, _target) = builder.build(&krate, &options).unwrap();
            let binary_name = binary.file_name().unwrap().to_str().unwrap();
            assert_eq!(binary_name, expected_bin_name("proc-macro-dep"));

            let sbom_path = read_sbom_for_binary(&binary);

            let json_str = fs::read_to_string(&sbom_path).unwrap();
            let bom: serde_cyclonedx::cyclonedx::v_1_4::CycloneDx = serde_json::from_str(&json_str).unwrap();

            let components = bom.components.unwrap();
            let serde_derive = components
                .iter()
                .find(|c| c.name.as_str() == "serde_derive")
                .expect("serde_derive should be in components");

            if let Some(ref props) = serde_derive.properties {
                let has_build_kind = props.iter().any(|p| {
                    p.name.as_deref() == Some("cdx:rustc:dependency_kind")
                        && p.value.as_deref() == Some("build")
                });
                assert!(has_build_kind, "proc-macro should be marked as build dependency");
            } else {
                panic!("proc-macro should have dependency_kind property");
            }
        }
    }

    mod build_options {
        use super::*;
        use crate::cli::Cli;

        mod features_parsing {
            use super::*;

            /// Test that an empty features string produces an empty vec.
            #[test]
            fn empty_features_string() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", "", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.features.is_empty());
            }

            /// Test parsing a single feature.
            #[test]
            fn single_feature() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", "feat1", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.features, vec!["feat1"]);
            }

            /// Test parsing comma-separated features.
            #[test]
            fn comma_separated_features() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", "feat1,feat2", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.features, vec!["feat1", "feat2"]);
            }

            /// Test parsing space-separated features.
            #[test]
            fn space_separated_features() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", "feat1 feat2", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.features, vec!["feat1", "feat2"]);
            }

            /// Test parsing features with mixed separators (commas and spaces).
            #[test]
            fn mixed_separator_features() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", "feat1, feat2 feat3", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.features, vec!["feat1", "feat2", "feat3"]);
            }

            /// Test that leading and trailing whitespace is handled correctly.
            #[test]
            fn whitespace_handling() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--features", " feat1 , feat2 ", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.features, vec!["feat1", "feat2"]);
            }

            /// Test that when no features flag is provided, features vec is empty.
            #[test]
            fn no_features_flag() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.features.is_empty());
            }
        }

        mod profile_selection {
            use super::*;

            /// Test that `--debug` flag maps to "dev" profile.
            #[test]
            fn debug_flag_maps_to_dev() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--debug", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.profile, Some("dev".to_string()));
            }

            /// Test that `--profile` flag sets the profile explicitly.
            #[test]
            fn explicit_profile() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--profile", "custom", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.profile, Some("custom".to_string()));
            }

            /// Test that when neither flag is provided, profile is None.
            #[test]
            fn no_profile_specified() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.profile, None);
            }
        }

        mod build_target_selection {
            use super::*;

            /// Test that no flags produces [`BuildTarget::DefaultBin`].
            #[test]
            fn default_bin_when_no_flags() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.build_target, BuildTarget::DefaultBin);
            }

            /// Test that `--bin` flag produces [`BuildTarget::Bin`].
            #[test]
            fn explicit_bin() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--bin", "foo", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.build_target, BuildTarget::Bin("foo".to_string()));
            }

            /// Test that `--example` flag produces [`BuildTarget::Example`].
            #[test]
            fn explicit_example() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--example", "bar", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.build_target, BuildTarget::Example("bar".to_string()));
            }
        }

        mod locked_offline_from_config {
            use super::*;

            /// BuildOptions reads locked/offline from Config.
            ///
            /// CLI override tests (--locked, --unlocked, --frozen, --offline) belong in config.rs
            /// since that's where the CLI-to-Config override logic lives.
            #[test]
            fn reads_default_locked_true() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.locked, "Should read locked=true from default Config");
                assert!(!options.offline, "Should read offline=false from default Config");
            }

            #[test]
            fn reads_config_locked_false() {
                let config = Config {
                    locked: false,
                    ..Default::default()
                };
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(!options.locked, "Should read locked=false from Config");
            }

            #[test]
            fn reads_config_offline_true() {
                let config = Config {
                    offline: true,
                    ..Default::default()
                };
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.offline, "Should read offline=true from Config");
            }

            #[test]
            fn reads_config_both_values() {
                let config = Config {
                    locked: false,
                    offline: true,
                    ..Default::default()
                };
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(!options.locked, "Should read locked=false from Config");
                assert!(options.offline, "Should read offline=true from Config");
            }
        }

        mod toolchain_from_config {
            use super::*;

            /// BuildOptions reads toolchain from Config.
            ///
            /// CLI override tests (+toolchain syntax) belong in config.rs since that's where
            /// the CLI-to-Config override logic lives.

            #[test]
            fn reads_default_none() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(
                    options.toolchain, None,
                    "Should read toolchain=None from default Config"
                );
            }

            #[test]
            fn reads_config_toolchain() {
                let config = Config {
                    toolchain: Some("stable".to_string()),
                    ..Default::default()
                };
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(
                    options.toolchain,
                    Some("stable".to_string()),
                    "Should read toolchain from Config"
                );
            }

            #[test]
            fn reads_config_nightly() {
                let config = Config {
                    toolchain: Some("nightly".to_string()),
                    ..Default::default()
                };
                let cli = Cli::parse_from_test_args(["tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(
                    options.toolchain,
                    Some("nightly".to_string()),
                    "Should read toolchain from Config"
                );
            }
        }

        mod direct_passthrough {
            use super::*;

            /// Test that `--all-features` flag is passed through.
            #[test]
            fn all_features() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--all-features", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.all_features);
            }

            /// Test that `--no-default-features` flag is passed through.
            #[test]
            fn no_default_features() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--no-default-features", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.no_default_features);
            }

            /// Test that `--target` flag is passed through.
            #[test]
            fn target() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--target", "x86_64-unknown-linux-gnu", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.target, Some("x86_64-unknown-linux-gnu".to_string()));
            }

            /// Test that `--jobs` flag is passed through.
            #[test]
            fn jobs() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--jobs", "4", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert_eq!(options.jobs, Some(4));
            }

            /// Test that `--ignore-rust-version` flag is passed through.
            #[test]
            fn ignore_rust_version() {
                let config = Config::default();
                let cli = Cli::parse_from_test_args(["--ignore-rust-version", "tool"]);
                let options = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();

                assert!(options.ignore_rust_version);
            }
        }
    }
}
