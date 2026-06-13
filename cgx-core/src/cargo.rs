use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

pub(crate) use cargo_metadata::Metadata;
use snafu::{OptionExt, ResultExt};
use tracing::debug;

use crate::{
    Result,
    builder::{BuildOptions, BuildTarget},
    config::{Config, Verbosity},
    error,
    messages::{BuildMessage, MessageReporter},
};

/// Options for controlling cargo metadata invocation.
#[derive(Clone, Debug, Default)]
pub(crate) struct CargoMetadataOptions {
    /// Exclude dependency information from metadata output.
    /// Corresponds to `--no-deps` flag.
    /// Default: false (dependencies are included by default)
    pub no_deps: bool,

    /// Only include dependencies for the specified target platform.
    /// Corresponds to `--filter-platform TARGET` flag.
    pub filter_platform: Option<String>,

    /// Space or comma separated list of features to activate.
    /// Corresponds to `--features` flag.
    pub features: Vec<String>,

    /// Activate all available features.
    /// Corresponds to `--all-features` flag.
    pub all_features: bool,

    /// Do not activate the `default` feature.
    /// Corresponds to `--no-default-features` flag.
    pub no_default_features: bool,

    /// Run without accessing the network.
    /// Corresponds to `--offline` flag.
    pub offline: bool,

    /// Require Cargo.lock is up to date.
    /// Corresponds to `--locked` flag.
    pub locked: bool,

    /// Rust toolchain override to query metadata with (e.g., "nightly", "1.70.0", "stable").
    /// When set, `cargo metadata` is run using the specified toolchain.
    pub toolchain: Option<String>,
}

impl From<&BuildOptions> for CargoMetadataOptions {
    fn from(opts: &BuildOptions) -> Self {
        Self {
            no_deps: false,
            filter_platform: opts.target.clone(),
            features: opts.features.clone(),
            all_features: opts.all_features,
            no_default_features: opts.no_default_features,
            offline: opts.offline,
            locked: opts.locked,
            toolchain: opts.toolchain.clone(),
        }
    }
}

/// Rust wrapper around shelling out to `cargo` for building and running Rust projects.
///
/// Much as it pains me, sometimes we must shell out to `cargo` to do things.  That's ugly,
/// error-prone, and worst of all inelegant.  But it's also the only way to get certain things
/// done.
///
/// This type is mainly concerened with the surprisingly complex task of figuring out where `cargo`
/// is and how to invoke it, and secondarily with constructing its command lines and parsing the
/// resulting output.
pub(crate) trait CargoRunner: std::fmt::Debug + Send + Sync + 'static {
    /// Get cargo metadata for a source directory.
    ///
    /// Executes `cargo metadata` on the specified directory and returns the
    /// parsed metadata including workspace members, packages, and targets.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Path to directory containing Cargo.toml
    /// * `options` - Options controlling metadata invocation (deps, features, platform, etc.)
    fn metadata(&self, source_dir: &Path, options: &CargoMetadataOptions) -> Result<Metadata>;

    /// Build a binary from source.
    ///
    /// Executes cargo build with specified options and returns the absolute path
    /// to the compiled binary, determined by parsing `--message-format=json` output.
    ///
    /// It is assumed that either the only crate in the workspace is a binary, or that the crate
    /// `package` has a binary or example matching `options.build_target`.
    ///
    /// # Arguments
    ///
    /// * `source_dir` - Directory containing Cargo.toml
    /// * `package` - Package name for `-p` flag (required for multi-package workspaces)
    /// * `options` - Build configuration
    ///
    /// # Toolchain Handling
    ///
    /// If `options.toolchain` is specified:
    /// - Requires rustup (errors if unavailable)
    /// - Invokes via `rustup run {toolchain} cargo build ...`
    /// - This works regardless of whether cargo is a rustup proxy
    ///
    /// # Binary Location
    ///
    /// Uses `--message-format=json` to parse compiler artifacts and find the
    /// executable path from "compiler-artifact" messages. This handles:
    /// - Cross-compilation: target/{triple}/{profile}/...
    /// - Examples: target/{profile}/examples/...
    /// - Platform extensions: .exe on Windows
    ///
    /// # Errors
    ///
    /// - Cargo.toml not found in `source_dir`
    /// - Toolchain specified but rustup not found
    /// - Cargo build command fails
    /// - Expected binary not found in cargo's JSON output
    fn build(&self, source_dir: &Path, package: Option<&str>, options: &BuildOptions) -> Result<PathBuf>;
}

/// Locate cargo and construct a runner instance that will use it.
pub(crate) fn create_cargo_runner(config: Config, reporter: MessageReporter) -> Result<impl CargoRunner> {
    // Locate cargo and rustup executables.
    //
    // Searches for cargo in priority order:
    // 1. `CARGO` environment variable (cargo's own convention)
    // 2. `cargo` in PATH (via `which` crate)
    // 3. `$CARGO_HOME/bin/cargo` where CARGO_HOME defaults to ~/.cargo
    //
    // Also searches for rustup (needed for `rustup run {toolchain}`).
    // Rustup not found is non-fatal - only errors when toolchain specified.

    let cargo_path = find_executable("cargo", "CARGO")?;
    let rustup_path = find_executable("rustup", "RUSTUP").ok();

    Ok(RealCargoRunner {
        cargo_path,
        rustup_path,
        reporter,
        verbosity: config.verbosity,
    })
}

#[derive(Debug, Clone)]
struct RealCargoRunner {
    cargo_path: PathBuf,
    rustup_path: Option<PathBuf>,
    reporter: MessageReporter,
    verbosity: Verbosity,
}

impl RealCargoRunner {
    /// Construct the complete `cargo metadata` command for the given options.
    ///
    /// When [`CargoMetadataOptions::toolchain`] is set, the command is routed through
    /// `rustup run <toolchain> cargo`.  This is the same technique used for `cargo build`
    /// elsewhere in this module.
    fn metadata_command(&self, source_dir: &Path, options: &CargoMetadataOptions) -> Result<Command> {
        let mut cmd = cargo_metadata::MetadataCommand::new();
        cmd.cargo_path(&self.cargo_path).current_dir(source_dir);

        // Only exclude deps if explicitly requested
        if options.no_deps {
            cmd.no_deps();
        }

        // Handle feature flags
        if options.all_features {
            cmd.features(cargo_metadata::CargoOpt::AllFeatures);
        } else {
            if options.no_default_features {
                cmd.features(cargo_metadata::CargoOpt::NoDefaultFeatures);
            }
            if !options.features.is_empty() {
                cmd.features(cargo_metadata::CargoOpt::SomeFeatures(options.features.clone()));
            }
        }

        // Build other_options for flags that don't have dedicated MetadataCommand methods
        let mut other_args = Vec::new();

        // Always filter by platform when resolving dependencies to avoid getting
        // deps for all platforms mixed together. Default to current platform if not specified.
        let platform: Option<&str> = if options.no_deps {
            // When not resolving deps, platform filtering doesn't matter
            options.filter_platform.as_deref()
        } else {
            // When resolving deps, MUST filter by platform
            // Default to current platform if not specified
            Some(
                options
                    .filter_platform
                    .as_deref()
                    .unwrap_or(build_context::TARGET),
            )
        };

        if let Some(platform_str) = platform {
            other_args.push("--filter-platform".to_string());
            other_args.push(platform_str.to_string());
        }

        if options.offline {
            other_args.push("--offline".to_string());
        }

        if options.locked {
            other_args.push("--locked".to_string());
        }

        if !other_args.is_empty() {
            cmd.other_options(other_args);
        }

        // If no toolchain is specified then we're done, otherwise we need to construct a `rustup`
        // invocation to set the toolchain.
        let inner = cmd.cargo_command();
        let Some(toolchain) = &options.toolchain else {
            return Ok(inner);
        };

        let rustup_path = self
            .rustup_path
            .as_ref()
            .with_context(|| error::RustupNotFoundSnafu {
                toolchain: toolchain.clone(),
            })?;

        // The rendered cargo command's working directory does not transfer to the wrapping
        // command, so set it again.
        let mut wrapped = Command::new(rustup_path);
        wrapped.args(["run", toolchain, "cargo"]);
        wrapped.args(inner.get_args());
        wrapped.current_dir(source_dir);
        Ok(wrapped)
    }

    /// Run a fully constructed `cargo metadata` command and parse its output.
    ///
    /// Mirrors [`cargo_metadata::MetadataCommand::exec`], which cannot be used directly because the
    /// command may be wrapped in `rustup run`.
    fn run_metadata_command(cmd: &mut Command) -> std::result::Result<Metadata, cargo_metadata::Error> {
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(cargo_metadata::Error::CargoMetadata {
                stderr: String::from_utf8(output.stderr)?,
            });
        }
        let stdout = std::str::from_utf8(&output.stdout)?
            .lines()
            .find(|line| line.starts_with('{'))
            .ok_or(cargo_metadata::Error::NoJson)?;
        cargo_metadata::MetadataCommand::parse(stdout)
    }
}

impl CargoRunner for RealCargoRunner {
    fn metadata(&self, source_dir: &Path, options: &CargoMetadataOptions) -> Result<Metadata> {
        let mut cmd = self.metadata_command(source_dir, options)?;
        let program = PathBuf::from(cmd.get_program());

        Self::run_metadata_command(&mut cmd).with_context(|_| error::CargoMetadataSnafu {
            cargo_path: program,
            source_dir: source_dir.to_path_buf(),
        })
    }

    fn build(&self, source_dir: &Path, package: Option<&str>, options: &BuildOptions) -> Result<PathBuf> {
        // Verify Cargo.toml exists
        if !source_dir.join("Cargo.toml").exists() {
            return error::CargoTomlNotFoundSnafu {
                source_dir: source_dir.to_path_buf(),
            }
            .fail();
        }

        self.reporter.report(|| BuildMessage::started(options));

        // Build the command
        let mut cmd = if let Some(toolchain) = &options.toolchain {
            // If toolchain is specified, we need rustup
            let rustup_path = self
                .rustup_path
                .as_ref()
                .with_context(|| error::RustupNotFoundSnafu {
                    toolchain: toolchain.clone(),
                })?;

            let mut cmd = Command::new(rustup_path);
            cmd.args(["run", toolchain, "cargo"]);
            cmd
        } else {
            Command::new(&self.cargo_path)
        };

        // Add cargo build command and flags
        cmd.arg("build");
        cmd.current_dir(source_dir);
        cmd.arg("--message-format=json");

        // Profile (default to release)
        if let Some(profile) = &options.profile {
            cmd.args(["--profile", profile]);
        } else {
            cmd.arg("--release");
        }

        // Package selection for workspaces
        if let Some(pkg) = package {
            cmd.args(["-p", pkg]);
        }

        // Features
        if options.all_features {
            cmd.arg("--all-features");
        } else {
            if options.no_default_features {
                cmd.arg("--no-default-features");
            }
            if !options.features.is_empty() {
                cmd.arg("--features");
                cmd.arg(options.features.join(","));
            }
        }

        // Target triple for cross-compilation
        if let Some(target) = &options.target {
            cmd.args(["--target", target]);
        }

        // Build target (bin/example)
        match &options.build_target {
            BuildTarget::DefaultBin => {
                // No specific flag needed, cargo will build the default binary
            }
            BuildTarget::Bin(name) => {
                cmd.args(["--bin", name]);
            }
            BuildTarget::Example(name) => {
                cmd.args(["--example", name]);
            }
        }

        // Other flags
        if options.offline {
            cmd.arg("--offline");
        }
        if let Some(jobs) = options.jobs {
            cmd.args(["-j", &jobs.to_string()]);
        }
        if options.ignore_rust_version {
            cmd.arg("--ignore-rust-version");
        }
        if options.locked {
            cmd.arg("--locked");
        }

        // Translate the verbosity level to cargo's repeated `-v` flag.
        match self.verbosity {
            Verbosity::Normal => {}
            Verbosity::Verbose => {
                cmd.arg("-v");
            }
            Verbosity::VeryVerbose => {
                cmd.arg("-vv");
            }
            Verbosity::ExtremelyVerbose => {
                cmd.arg("-vvv");
            }
        }

        // Configure pipes for streaming
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Spawn the process
        let mut child = cmd.spawn().context(error::CommandExecutionSnafu)?;

        // Take ownership of stdout and stderr pipes
        let stdout = child
            .stdout
            .take()
            .with_context(|| error::BinaryNotFoundInOutputSnafu)?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| error::BinaryNotFoundInOutputSnafu)?;

        // Clone reporter for threads
        let stdout_reporter = self.reporter.clone();
        let stderr_reporter = self.reporter.clone();

        // Clone build target for stdout thread
        let build_target = options.build_target.clone();

        // Spawn stdout parsing thread
        let stdout_handle = thread::spawn(move || {
            debug!("stdout parser thread starting");
            let reader = BufReader::new(stdout);
            let mut binary_path = None;

            for line_result in reader.lines() {
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => break,
                };

                if let Ok(cargo_msg) = serde_json::from_str::<cargo_metadata::Message>(&line) {
                    stdout_reporter.report(|| BuildMessage::cargo_message(cargo_msg.clone()));

                    if let cargo_metadata::Message::CompilerArtifact(artifact) = &cargo_msg {
                        let kinds = &artifact.target.kind;
                        let name = &artifact.target.name;

                        let matches = match &build_target {
                            BuildTarget::DefaultBin => {
                                kinds.iter().any(|k| *k == cargo_metadata::TargetKind::Bin)
                            }
                            BuildTarget::Bin(bin_name) => {
                                kinds.iter().any(|k| *k == cargo_metadata::TargetKind::Bin)
                                    && name == bin_name
                            }
                            BuildTarget::Example(ex_name) => {
                                kinds.iter().any(|k| *k == cargo_metadata::TargetKind::Example)
                                    && name == ex_name
                            }
                        };

                        if matches {
                            if let Some(exe) = &artifact.executable {
                                binary_path = Some(exe.clone().into_std_path_buf());
                            }
                        }
                    }
                }
            }

            debug!("stdout parser thread exiting");
            binary_path
        });

        // Spawn stderr chunk reading thread
        let stderr_handle = thread::spawn(move || {
            debug!("stderr reader thread starting");
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = buffer[..n].to_vec();
                        stderr_reporter.report(|| BuildMessage::cargo_stderr(chunk));
                    }
                }
            }

            debug!("stderr reader thread exiting");
        });

        // Wait for process completion
        let status = child.wait().context(error::CommandExecutionSnafu)?;

        // Join both threads after wait() returns
        let binary_path = stdout_handle.join().expect("stdout thread panicked");
        stderr_handle.join().expect("stderr thread panicked");

        if !status.success() {
            return error::CargoBuildFailedSnafu {
                exit_code: status.code(),
            }
            .fail();
        }

        match binary_path {
            Some(path) => {
                self.reporter.report(|| BuildMessage::completed(&path));
                Ok(path)
            }
            None => error::BinaryNotFoundInOutputSnafu.fail(),
        }
    }
}

/// Find an executable by name, checking environment variable, PATH, and default locations.
fn find_executable(name: &str, env_var: &str) -> Result<PathBuf> {
    // Check environment variable
    if let Ok(path) = std::env::var(env_var) {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    // Check PATH using `which` crate
    if let Ok(path) = which::which(name) {
        return Ok(path);
    }

    // Check $CARGO_HOME/bin/{name}
    let cargo_home = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home::cargo_home().ok());

    if let Some(cargo_home) = cargo_home {
        let path = cargo_home.join("bin").join(name);
        if path.exists() {
            return Ok(path);
        }
    }

    error::ExecutableNotFoundSnafu {
        name: name.to_string(),
    }
    .fail()
}

/// Testing a wrapper around `cargo` thoroughly is out of the scope of simple unit tests, however
/// we at least need to verify basic functionality and correctness.
///
/// By definition, if these tests are running, `cargo` must be present, so we've made some tests
/// that operate on this project itself as test data.  Of course this isn't adequate coverage for
/// all various scenarios, but it's better than nothing.
#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;
    use crate::{builder::BuildTarget, testdata::CrateTestCase};

    /// Get the path to the cgx workspace root directory.
    fn cgx_project_root() -> PathBuf {
        // CARGO_MANIFEST_DIR points to cgx-core, we need the workspace root (parent directory)
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("cgx-core should have a parent directory (workspace root)")
            .to_path_buf()
    }

    #[test]
    fn create_cargo_runner_succeeds() {
        crate::logging::init_test_logging();

        // This test verifies that we can locate cargo on the system.
        // This should always succeed since cargo is required to run the tests.
        let _cargo = create_cargo_runner(Config::default(), MessageReporter::null()).unwrap();
    }

    #[test]
    fn metadata_reads_cgx_crate() {
        crate::logging::init_test_logging();

        let cargo = create_cargo_runner(Config::default(), MessageReporter::null()).unwrap();
        let cgx_root = cgx_project_root();

        let metadata = cargo
            .metadata(
                &cgx_root,
                &CargoMetadataOptions {
                    no_deps: true,
                    ..Default::default()
                },
            )
            .unwrap();

        // Verify we found the cgx package
        let cgx_pkg = metadata
            .packages
            .iter()
            .find(|p| p.name.as_str() == "cgx")
            .unwrap();

        assert_eq!(cgx_pkg.name.as_str(), "cgx");

        // Verify version is valid semver
        assert!(!cgx_pkg.version.to_string().is_empty());

        // Verify we have at least one binary target
        let has_bin = cgx_pkg
            .targets
            .iter()
            .any(|t| t.kind.iter().any(|k| k.to_string() == "bin"));
        assert!(has_bin, "cgx should have a binary target");
    }

    #[test]
    fn build_compiles_cgx_in_tempdir() {
        crate::logging::init_test_logging();

        let cargo = create_cargo_runner(Config::default(), MessageReporter::null()).unwrap();
        let cgx_root = cgx_project_root();
        let temp_dir = tempfile::tempdir().unwrap();

        // Copy source to temp directory
        crate::helpers::copy_source_tree(&cgx_root, temp_dir.path()).unwrap();

        // Verify Cargo.toml was copied
        assert!(
            temp_dir.path().join("Cargo.toml").exists(),
            "Cargo.toml should be copied"
        );

        // Build in dev mode (faster than release)
        let options = BuildOptions {
            profile: Some("dev".to_string()),
            build_target: BuildTarget::DefaultBin,
            ..Default::default()
        };

        let binary_path = cargo.build(temp_dir.path(), Some("cgx"), &options).unwrap();

        // Verify binary exists and is a file
        assert!(binary_path.exists(), "Binary should exist at {:?}", binary_path);
        assert!(binary_path.is_file(), "Binary should be a file");

        // Verify it's named correctly (cgx or cgx.exe on Windows)
        let file_name = binary_path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(
            file_name == "cgx" || file_name == "cgx.exe",
            "Binary should be named cgx or cgx.exe, got {}",
            file_name
        );
    }

    #[test]
    fn metadata_command_uses_cargo_directly_without_toolchain() {
        let cargo = RealCargoRunner {
            cargo_path: PathBuf::from("/fake/cargo"),
            rustup_path: None,
            reporter: MessageReporter::null(),
            verbosity: Verbosity::Normal,
        };

        let cmd = cargo
            .metadata_command(Path::new("/src"), &CargoMetadataOptions::default())
            .unwrap();

        assert_eq!(cmd.get_program().to_string_lossy(), "/fake/cargo");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["metadata", "--format-version", "1"]);
        assert!(!args.contains(&"run".to_string()), "args: {args:?}");
    }

    #[test]
    fn metadata_command_wraps_cargo_with_rustup_for_toolchain() {
        let cargo = RealCargoRunner {
            cargo_path: PathBuf::from("/fake/cargo"),
            rustup_path: Some(PathBuf::from("/fake/rustup")),
            reporter: MessageReporter::null(),
            verbosity: Verbosity::Normal,
        };

        let cmd = cargo
            .metadata_command(
                Path::new("/src"),
                &CargoMetadataOptions {
                    locked: true,
                    toolchain: Some("nightly".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(cmd.get_program().to_string_lossy(), "/fake/rustup");
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..6],
            ["run", "nightly", "cargo", "metadata", "--format-version", "1"]
        );
        assert!(args.contains(&"--locked".to_string()), "args: {args:?}");
    }

    #[test]
    fn build_options_toolchain_carries_into_metadata_options() {
        let build_options = BuildOptions {
            toolchain: Some("nightly".to_string()),
            ..Default::default()
        };

        let metadata_options = CargoMetadataOptions::from(&build_options);
        assert_eq!(metadata_options.toolchain.as_deref(), Some("nightly"));
    }

    /// Metadata queries follow the same toolchain contract as builds: a requested toolchain
    /// requires rustup, rather than being silently resolved with the default cargo.
    #[test]
    fn metadata_with_toolchain_requires_rustup() {
        crate::logging::init_test_logging();

        let cargo = RealCargoRunner {
            cargo_path: find_executable("cargo", "CARGO").unwrap(),
            rustup_path: None,
            reporter: MessageReporter::null(),
            verbosity: Verbosity::Normal,
        };

        let result = cargo.metadata(
            &cgx_project_root(),
            &CargoMetadataOptions {
                no_deps: true,
                toolchain: Some("nightly".to_string()),
                ..Default::default()
            },
        );

        assert_matches!(
            result,
            Err(error::Error::RustupNotFound { ref toolchain }) if toolchain == "nightly"
        );
    }

    #[test]
    fn metadata_loads_all_testcases() {
        crate::logging::init_test_logging();

        let cargo = create_cargo_runner(Config::default(), MessageReporter::null()).unwrap();

        for testcase in CrateTestCase::all() {
            let result = cargo.metadata(testcase.path(), &CargoMetadataOptions::default());

            assert!(
                result.is_ok(),
                "Failed to load metadata for {}: {:?}",
                testcase.name,
                result.err()
            );
        }
    }
}
