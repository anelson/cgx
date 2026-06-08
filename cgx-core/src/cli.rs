use std::{ffi::OsString, path::PathBuf};

use clap::{
    ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
    builder::TypedValueParser, error::ErrorKind,
};
use strum::VariantNames;

use crate::{
    builder::{BuildOverrides, BuildTarget},
    config::{BinaryProvider, ConfigOverrides, LockMode, UsePrebuiltBinaries, Verbosity},
    cratespec::{CrateRequest, Source},
    git::GitSelector,
};

/// A fully validated command, produced by parsing the CLI args with
/// [`Self::parse_from_cli_args`]
#[derive(Clone, Debug)]
pub enum Cli {
    /// Resolve a crate and list its runnable targets without building or executing.
    ListTargets(CrateArgs),
    /// Render the merged configured tools and aliases without resolving them.
    ListTools(ListTools),
    /// Prepare a crate without executing it or printing its path.
    Prefetch(CrateArgs),
    /// Prefetch every tool and alias configured in the `cgx` configuration.
    PrefetchAll(PrefetchAll),
    /// Prepare a crate without executing it; print the resolved binary path to stdout.
    NoExec(CrateArgs),
    /// Prepare a crate and execute it, forwarding `tool_args` to the executed binary.
    Run {
        args: CrateArgs,
        tool_args: Vec<OsString>,
    },
}

impl Cli {
    /// Parse the process command line into a validated [`Cli`] command.
    ///
    /// `version` is the string shown by `-V`/`--version`; clap prints it to stdout and exits. This
    /// uses clap, which will exit the process on `--help`, `--version`, or invalid arguments.
    pub fn parse_from_cli_args(version: String) -> Self {
        let args: Vec<String> = std::env::args().collect();
        let args = Self::strip_cargo_subcommand_arg(args);
        Self::parse_from_arg_strings(args, Some(version)).unwrap_or_else(|err| err.exit())
    }

    /// Translate the configuration-affecting arguments this command supplies into a
    /// [`ConfigOverrides`], leaving as default any options that the command doesn't accept.
    ///
    /// Not all commands even take all config options, but by translating each into a single
    /// [`ConfigOverrides`] representation of the sum total of CLI-based config settings it makes it
    /// easier to load and render the config in a single shared code path.
    pub fn to_config_overrides(&self) -> ConfigOverrides {
        match self {
            Cli::ListTargets(args) | Cli::Prefetch(args) | Cli::NoExec(args) | Cli::Run { args, .. } => {
                args.to_config_overrides()
            }
            Cli::ListTools(list_tools) => list_tools.to_config_overrides(),
            Cli::PrefetchAll(prefetch_all) => prefetch_all.to_config_overrides(),
        }
    }

    /// The structured-message format requested for this command, if any.
    pub fn message_format(&self) -> Option<MessageFormat> {
        self.reporting().message_format
    }

    /// The requested verbosity level for this command.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_count(self.reporting().verbose)
    }

    /// The reporting controls carried by this command's variant.
    fn reporting(&self) -> &ReportingArgs {
        match self {
            Cli::ListTargets(args) | Cli::Prefetch(args) | Cli::NoExec(args) | Cli::Run { args, .. } => {
                &args.reporting
            }
            Cli::ListTools(list_tools) => &list_tools.reporting,
            Cli::PrefetchAll(prefetch_all) => &prefetch_all.reporting,
        }
    }

    /// Shared parse pipeline: extract a leading `+toolchain`, invoke clap (with an optional version
    /// string for the `--version` action), and validate into a [`Cli`] via [`RawCli::render`].
    fn parse_from_arg_strings(args: Vec<String>, version: Option<String>) -> Result<Self, clap::Error> {
        let (toolchain, filtered_args) = Self::extract_toolchain(args);

        let mut command = RawCli::command();
        if let Some(version) = version {
            command = command.version(version);
        }

        let matches = command.try_get_matches_from(filtered_args)?;
        let mut raw = RawCli::from_arg_matches(&matches)?;
        raw.toolchain = toolchain;
        raw.render()
    }

    /// Strip the cargo subcommand argument when invoked as `cargo-cgx`.
    ///
    /// When cgx is invoked as a cargo subcommand (via the `cargo-cgx` binary),
    /// cargo invokes it with argv like: `["cargo-cgx", "cgx", ...user_args]`.
    /// This function detects that pattern and removes the redundant "cgx" argument.
    ///
    /// This pre-processing happens before all other argument parsing to ensure
    /// that subsequent parsing logic sees the same argument structure regardless
    /// of whether the user invoked `cgx` or `cargo cgx`.
    ///
    /// The function checks if the binary name (argv\[0\] or `std::env::current_exe()`)
    /// contains "cargo-cgx". If so, and if argv\[1\] equals "cgx", then argv\[1\] is removed.
    ///
    /// # Examples
    ///
    /// ```text
    /// Input:  ["cargo-cgx", "cgx", "ripgrep", "--help"]
    /// Output: ["cargo-cgx", "ripgrep", "--help"]
    ///
    /// Input:  ["cgx", "ripgrep", "--help"]
    /// Output: ["cgx", "ripgrep", "--help"]
    ///
    /// Input:  ["/usr/bin/cargo-cgx", "cgx", "+nightly", "just"]
    /// Output: ["/usr/bin/cargo-cgx", "+nightly", "just"]
    /// ```
    fn strip_cargo_subcommand_arg<I, T>(args: I) -> Vec<String>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(|s| s.into()).collect();

        if args.is_empty() {
            return args;
        }

        let is_cargo_subcommand = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("cargo-cgx")))
            .unwrap_or(false);

        if is_cargo_subcommand && args.len() > 1 && args[1] == "cgx" {
            let mut filtered = vec![args[0].clone()];
            filtered.extend_from_slice(&args[2..]);
            filtered
        } else {
            args
        }
    }

    /// Extract `+toolchain` syntax from the first positional argument.
    ///
    /// This method performs pre-processing to extract cargo/rustup-style toolchain overrides
    /// before clap parses the arguments. This is necessary because:
    ///
    /// 1. The `+toolchain` syntax must appear as the first argument (after the binary name)
    /// 2. It uses a `+` prefix which conflicts with clap's normal argument parsing
    /// 3. It's a modifier that applies globally, not a flag or positional argument
    /// 4. This matches how rustup handles toolchain selection for cargo
    ///
    /// clap has no native support for this pattern, so we extract it manually and then
    /// pass the filtered arguments to clap for normal parsing.
    ///
    /// # Arguments
    ///
    /// * `args` - The raw command line arguments including the binary name at position 0
    ///
    /// # Returns
    ///
    /// A tuple of `(Option<String>, Vec<String>)` where:
    /// - The first element is `Some(toolchain)` if `+toolchain` was found, `None` otherwise
    /// - The second element is the filtered argument list with `+toolchain` removed
    fn extract_toolchain<I, T>(args: I) -> (Option<String>, Vec<String>)
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let args = args.into_iter().map(|s| s.into()).collect::<Vec<String>>();
        if args.len() > 1 && args[1].starts_with('+') && args[1].len() > 1 {
            let toolchain = args[1][1..].to_string();

            let mut filtered = vec![args[0].clone()];
            filtered.extend_from_slice(&args[2..]);

            (Some(toolchain), filtered)
        } else {
            (None, args)
        }
    }

    /// Parse an arbitrary argument iterator for tests, panicking on parse or validation errors.
    #[cfg(test)]
    pub fn parse_from_test_args<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from_test_args(args).unwrap()
    }

    /// Try to parse an arbitrary argument iterator for tests, running the same preprocessing and
    /// [`RawCli::render`] validation as the real entry point.
    #[cfg(test)]
    pub fn try_parse_from_test_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        // Prepend the executable name, as clap expects, so callers don't have to.
        let args = std::iter::once(OsString::from("cgx")).chain(args.into_iter().map(|s| s.into()));
        let args: Vec<String> = args.map(|s| s.to_string_lossy().to_string()).collect();
        Self::parse_from_arg_strings(args, None)
    }

    /// Borrow the [`CrateArgs`] of a crate-level command, panicking on commands that don't
    /// take crate args.
    ///
    /// Test helper for asserting on parsed crate arguments.
    #[cfg(test)]
    pub fn crate_args(&self) -> &CrateArgs {
        match self {
            Cli::ListTargets(args) | Cli::Prefetch(args) | Cli::NoExec(args) | Cli::Run { args, .. } => args,
            other @ (Cli::ListTools(_) | Cli::PrefetchAll(_)) => {
                panic!("expected a crate-level command, got {other:?}")
            }
        }
    }

    /// The trailing tool arguments of a [`Cli::Run`] command (empty for any other). Test helper.
    #[cfg(test)]
    pub fn tool_args(&self) -> &[OsString] {
        match self {
            Cli::Run { tool_args, .. } => tool_args,
            _ => &[],
        }
    }
}

/// The parsed command-line arguments for preparing or running a single crate.
///
/// Not all possible commands involve running a single crate so not all commands will include
/// these crate args.
#[derive(Clone, Debug)]
pub struct CrateArgs {
    /// The crate spec (name, optionally with an `@VERSION` suffix), or `None` when the name is
    /// discovered from the source (e.g. `--git`/`--path`).
    pub crate_spec: Option<String>,
    /// Version requirement from `--crate-version`.
    pub crate_version: Option<String>,
    /// Where to obtain the crate.
    pub source: Source,
    /// Which git ref to use, for git-backed sources.
    pub git_selector: GitSelector,
    /// Build options passed through to cargo.
    pub build_options: BuildOptionsArgs,
    /// Cargo behavior flags (lockfile / offline / refresh).
    pub cargo: CargoBehaviorArgs,
    /// Pre-built binary lookup options.
    pub prebuilt: PrebuiltBinaryArgs,
    /// Config file discovery options.
    pub config: ConfigArgs,
    /// HTTP tuning options.
    pub http: HttpArgs,
    /// Output and diagnostic controls.
    pub reporting: ReportingArgs,
    /// Toolchain from a leading `+toolchain` token.
    pub toolchain: Option<String>,
}

impl CrateArgs {
    /// Translate these arguments into a [`CrateRequest`] for [`crate::cratespec::CrateSpec::load`].
    ///
    /// This also performs some validation checking for conflicting args that `clap` is not
    /// expressive enough to handle on its own, therefore it's fallible.
    pub fn crate_request(&self) -> crate::Result<CrateRequest> {
        // Split the CLI-only `name@version` convention into name and version-suffix string.
        let (name, at_version) = match &self.crate_spec {
            Some(spec) => match spec.split_once('@') {
                Some((name, version)) => (Some(name.to_string()), Some(version.to_string())),
                None => (Some(spec.clone()), None),
            },
            None => (None, None),
        };

        // Users can either specify a semver version req with the `name@version` syntax in the
        // crate name, or they can specify the version with `--crate-version`, but they cannot
        // provide both. (of course they can also not specify any version at all to default to the
        // latest suitable version)
        let version = match (at_version, self.crate_version.clone()) {
            (Some(at_version), Some(flag_version)) => {
                return crate::error::ConflictingVersionsSnafu {
                    at_version,
                    flag_version,
                }
                .fail();
            }
            (Some(version), None) | (None, Some(version)) => Some(version),
            (None, None) => None,
        };

        Ok(CrateRequest {
            name,
            version,
            source: self.source.clone(),
            git_ref: self.git_selector.clone(),
        })
    }

    /// Translate a subset of these arguments into [`BuildOverrides`] for
    /// [`crate::builder::BuildOptions::load`].
    pub fn to_build_overrides(&self) -> BuildOverrides {
        let build = &self.build_options;
        BuildOverrides {
            features: build.features.as_deref().map(Self::parse_features),
            all_features: build.all_features,
            no_default_features: build.no_default_features,
            profile: if build.debug {
                Some("dev".to_string())
            } else {
                build.profile.clone()
            },
            target: build.target.clone(),
            jobs: build.jobs,
            ignore_rust_version: build.ignore_rust_version,
            target_selection: build.build_target(),
            toolchain: self.toolchain.clone(),
        }
    }

    /// Translate a subset of these arguments into [`ConfigOverrides`]
    pub fn to_config_overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            http_timeout: self.http.http_timeout.clone(),
            http_retries: self.http.http_retries,
            http_proxy: self.http.http_proxy.clone(),
            lockfile: self.cargo.lock_mode(),
            offline: self.cargo.offline,
            refresh: self.cargo.refresh,
            prebuilt_binary: self.prebuilt.prebuilt_binary,
            prebuilt_binary_sources: self.prebuilt.prebuilt_binary_sources.clone(),
            prebuilt_binary_no_verify_checksums: self.prebuilt.prebuilt_binary_no_verify_checksums,
            prebuilt_binary_no_verify_signatures: self.prebuilt.prebuilt_binary_no_verify_signatures,
            verbosity: Verbosity::from_count(self.reporting.verbose),
            ..self.config.to_config_overrides()
        }
    }

    /// Tokenize a `--features` string into individual feature names.
    ///
    /// Features may be separated by commas or whitespace; empty tokens are dropped, so an empty
    /// input yields an empty vector (distinct from `--features` being absent, which the caller
    /// represents as `None`).
    fn parse_features(features_str: &str) -> Vec<String> {
        features_str
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }
}

/// Arguments for `--prefetch-all`
#[derive(Clone, Debug, Default)]
pub struct PrefetchAll {
    /// Config file discovery overrides.
    pub config: ConfigArgs,
    /// HTTP tuning options.
    pub http: HttpArgs,
    /// Output and diagnostic controls.
    pub reporting: ReportingArgs,
    /// Run without accessing the network.
    pub offline: bool,
    /// Force refresh of all cached data.
    pub refresh: bool,
    /// Number of parallel jobs.
    pub jobs: Option<usize>,
    /// Ignore `rust-version` specifications in packages.
    pub ignore_rust_version: bool,
    /// Toolchain from a leading `+toolchain` token.
    pub toolchain: Option<String>,
}

impl PrefetchAll {
    /// The build overrides applied to every tool prefetched by `--prefetch-all`.
    ///
    /// `--prefetch-all` doesn't take per-crate compilation options, so only the generic build knobs
    /// it does accept are set; the rest take the defaults or can be set in the config file.
    pub fn to_build_overrides(&self) -> BuildOverrides {
        BuildOverrides {
            jobs: self.jobs,
            ignore_rust_version: self.ignore_rust_version,
            toolchain: self.toolchain.clone(),
            ..BuildOverrides::default()
        }
    }

    /// The config overrides applied when loading config for the `--prefetch-all` run.
    pub fn to_config_overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            http_timeout: self.http.http_timeout.clone(),
            http_retries: self.http.http_retries,
            http_proxy: self.http.http_proxy.clone(),
            offline: self.offline,
            refresh: self.refresh,
            ..self.config.to_config_overrides()
        }
    }
}

/// Arguments for `--list-tools`
#[derive(Clone, Debug, Default)]
pub struct ListTools {
    /// Config file discovery overrides.
    pub config: ConfigArgs,
    /// Output and diagnostic controls.
    pub reporting: ReportingArgs,
}

impl ListTools {
    /// The config overrides applied when loading config for the `--list-tools` run.
    pub fn to_config_overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            verbosity: Verbosity::from_count(self.reporting.verbose),
            ..self.config.to_config_overrides()
        }
    }
}

/// CLI arguments that are crate-specific and passed through to cargo build.
///
/// These args are segreated from the other CLI args to make the semantic distinction more
/// explicit.  Most CLI args can also be set in config files to apply globally, but these are
/// always crate-specific.
#[derive(Clone, Debug, Default, Args)]
pub struct BuildOptionsArgs {
    /// Space or comma separated list of features to activate
    #[arg(short = 'F', long, value_name = "FEATURES")]
    pub features: Option<String>,

    /// Activate all available features
    #[arg(long)]
    pub all_features: bool,

    /// Do not activate the default features
    #[arg(long)]
    pub no_default_features: bool,

    /// Build with the specified profile
    #[arg(long, value_name = "PROFILE-NAME", conflicts_with = "debug")]
    pub profile: Option<String>,

    /// Build in debug mode (with the 'dev' profile) instead of release mode
    #[arg(long)]
    pub debug: bool,

    /// Build for the target triple
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Number of parallel jobs, defaults to # of CPUs
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Ignore `rust-version` specification in packages
    #[arg(long)]
    pub ignore_rust_version: bool,

    /// Install only the specified binary
    #[arg(long, value_name = "NAME", conflicts_with = "example")]
    pub bin: Option<String>,

    /// Install only the specified example
    #[arg(long, value_name = "NAME")]
    pub example: Option<String>,
}

impl BuildOptionsArgs {
    /// True if any compilation-affecting option is set, such that a pre-built binary is not
    /// suitable for this request.
    fn has_compilation_options(&self) -> bool {
        self.features.is_some()
            || self.all_features
            || self.no_default_features
            || self.profile.is_some()
            || self.debug
            || self.target.is_some()
            || self.bin.is_some()
            || self.example.is_some()
    }

    /// True if any build option at all is set.
    fn is_present(&self) -> bool {
        self.has_compilation_options() || self.jobs.is_some() || self.ignore_rust_version
    }

    /// Resolve the mutually-exclusive `--bin`/`--example` flags into a [`BuildTarget`].
    fn build_target(&self) -> BuildTarget {
        match (&self.bin, &self.example) {
            (Some(_), Some(_)) => {
                unreachable!("BUG: clap should enforce mutual exclusivity");
            }
            (Some(bin_name), None) => BuildTarget::Bin(bin_name.clone()),
            (None, Some(example_name)) => BuildTarget::Example(example_name.clone()),
            (None, None) => BuildTarget::default(),
        }
    }
}
/// Lockfile and cache behavior flags that affect dependency resolution and cache identity for one
/// tool.
///
/// The mutually-exclusive `--locked`/`--frozen`/`--unlocked` flags share a `lockfile`
/// arg-group
#[derive(Clone, Debug, Default, Args)]
pub struct CargoBehaviorArgs {
    /// Honor Cargo.lock from the crate, equivalent to passing `--locked` to `cargo install`
    #[arg(long, group = "lockfile")]
    pub locked: bool,

    /// Equivalent to specifying both --locked and --offline
    #[arg(long, group = "lockfile")]
    pub frozen: bool,

    /// Ignore Cargo.lock and resolve dependencies fresh
    #[arg(long, group = "lockfile")]
    pub unlocked: bool,

    /// Run without accessing the network
    #[arg(long)]
    pub offline: bool,

    /// Force refresh of all cached data for this crate.
    #[arg(long)]
    pub refresh: bool,
}

impl CargoBehaviorArgs {
    /// Collapse the mutually-exclusive `--locked`/`--frozen`/`--unlocked` flags into a
    /// [`LockMode`].
    ///
    /// The `lockfile` clap arg-group guarantees at most one is set.
    fn lock_mode(&self) -> LockMode {
        if self.unlocked {
            LockMode::Unlocked
        } else if self.frozen {
            LockMode::Frozen
        } else if self.locked {
            LockMode::Locked
        } else {
            LockMode::Default
        }
    }
}

/// Creates a clap value parser that uses strum's [`VariantNames`] for possible values
/// and strum's [`FromStr`](std::str::FromStr) for parsing. This ensures:
/// - `--help` shows valid values (from `VARIANTS`)
/// - Parsing uses the same logic as config files (strum's [`EnumString`](strum::EnumString))
macro_rules! strum_value_parser {
    ($t:ty) => {
        clap::builder::PossibleValuesParser::new(<$t>::VARIANTS).map(|s| s.parse::<$t>().unwrap())
    };
}

/// CLI config overrides for prebuilt binaries
#[derive(Clone, Debug, Default, Args)]
pub struct PrebuiltBinaryArgs {
    /// Control use of pre-built binaries: never, always, or auto.
    #[arg(long, value_name = "WHEN", value_parser = strum_value_parser!(UsePrebuiltBinaries))]
    pub prebuilt_binary: Option<UsePrebuiltBinaries>,

    /// Override the binary providers to check for pre-built binaries.
    #[arg(
        long,
        value_name = "SOURCES",
        value_delimiter = ',',
        value_parser = strum_value_parser!(BinaryProvider)
    )]
    pub prebuilt_binary_sources: Option<Vec<BinaryProvider>>,

    /// Disable checksum verification when downloading pre-built binaries.
    #[arg(long)]
    pub prebuilt_binary_no_verify_checksums: bool,

    /// Disable signature verification when downloading pre-built binaries.
    #[arg(long)]
    pub prebuilt_binary_no_verify_signatures: bool,
}

impl PrebuiltBinaryArgs {
    /// True if any prebuilt-binary override was given on the command line.
    fn is_present(&self) -> bool {
        self.prebuilt_binary.is_some()
            || self.prebuilt_binary_sources.is_some()
            || self.prebuilt_binary_no_verify_checksums
            || self.prebuilt_binary_no_verify_signatures
    }
}

/// Options that control which configuration files are loaded before executing a command.
///
/// Every command that respects config files accepts these args.
#[derive(Clone, Debug, Default, Args)]
pub struct ConfigArgs {
    /// Read configuration options from the given TOML file only, bypassing the usual config search
    /// paths.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = ["system_config_dir", "app_dir", "user_config_dir"]
    )]
    pub config_file: Option<PathBuf>,

    /// Override the system config directory location.
    #[arg(long, value_name = "PATH", env = "CGX_SYSTEM_CONFIG_DIR")]
    pub system_config_dir: Option<PathBuf>,

    /// Override the base application directory.
    #[arg(long, value_name = "PATH", env = "CGX_APP_DIR")]
    pub app_dir: Option<PathBuf>,

    /// Override the user config directory location.
    #[arg(long, value_name = "PATH", env = "CGX_USER_CONFIG_DIR")]
    pub user_config_dir: Option<PathBuf>,
}

impl ConfigArgs {
    /// Create a new [`ConfigOverrides`] consisting of default values except those that are
    /// overridden by options specified in this struct.
    fn to_config_overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            config_file: self.config_file.clone(),
            system_config_dir: self.system_config_dir.clone(),
            app_dir: self.app_dir.clone(),
            user_config_dir: self.user_config_dir.clone(),
            ..ConfigOverrides::default()
        }
    }
}

/// Network tuning options used by commands that may resolve, download, or build crates.
#[derive(Clone, Debug, Default, Args)]
pub struct HttpArgs {
    /// HTTP request timeout (e.g., "30s", "2m").
    #[arg(long, value_name = "DURATION", env = "CGX_HTTP_TIMEOUT")]
    pub http_timeout: Option<String>,

    /// Maximum number of retries for transient HTTP failures (0 = no retries).
    #[arg(long, value_name = "N", env = "CGX_HTTP_RETRIES")]
    pub http_retries: Option<usize>,

    /// HTTP or SOCKS5 proxy URL for all HTTP requests.
    #[arg(long, value_name = "URL", env = "CGX_HTTP_PROXY")]
    pub http_proxy: Option<String>,
}

impl HttpArgs {
    /// True if any HTTP tuning option was given on the command line.
    fn is_present(&self) -> bool {
        self.http_timeout.is_some() || self.http_retries.is_some() || self.http_proxy.is_some()
    }
}

/// Output and diagnostic controls for commands that may produce operational messages.
#[derive(Clone, Debug, Default, Args)]
pub struct ReportingArgs {
    /// Use verbose output (-vv very verbose/build.rs output)
    #[arg(short = 'v', long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Do not print cargo log messages
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Coloring: auto, always, never
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,

    /// Output structured messages in the specified format.
    #[arg(long, value_name = "FMT")]
    pub message_format: Option<MessageFormat>,
}

/// Output format for structured messages.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MessageFormat {
    /// JSON format, one message per line
    Json,
}

// Raw clap parse result.
//
// Private on purpose: the public surface is [`Cli`], produced by [`Self::resolve`], which enforces
// the per-mode rules clap cannot express structurally and collapses the flat flags into typed
// command variants. The mutually-exclusive mode flags share a `mode` ArgGroup, so clap rejects two
// modes natively without a `conflicts_with` explosion.
//
// This is how we can treat things like `--prefetch` and `--list-tools` as if they were clap
// subcommands, and still properly handle `cgx foo` for literally any `foo` as an invocation of
// crate `foo`.
//
// This uses a regular comment, not a doc comment, so the explanation does not leak into `--help`
// output as the command's long description.
#[derive(Clone, Debug, Parser)]
#[command(name = "cgx")]
#[command(about = "Rust equivalent of uvx or npx, for running Rust crates")]
#[command(
    after_help = "To run a crate, pass its name (optionally with @VERSION) followed by any arguments for \
                  the tool, e.g. `cgx ripgrep --color=always`. cgx's own options must come before the crate \
                  name; everything after it is forwarded to the tool. Use `cgx cargo <subcommand>` to run a \
                  cargo plugin (e.g. `cgx cargo deny` is equivalent to `cgx cargo-deny`)."
)]
struct RawCli {
    /// Build the binary but do not execute it; print its path to stdout instead.
    #[arg(long, group = "mode")]
    no_exec: bool,

    /// Prepare the crate binary and exit without printing or executing it.
    #[arg(long, group = "mode")]
    prefetch: bool,

    /// Prefetch all tools configured in the `cgx` configuration.
    #[arg(long, group = "mode")]
    prefetch_all: bool,

    /// List the crate's executable targets (bins and examples) without building or executing.
    #[arg(long, group = "mode")]
    list_targets: bool,

    /// List all configured tools and aliases in the `cgx` configuration.
    #[arg(long, group = "mode")]
    list_tools: bool,

    /// Version requirement of the crate to run (alternative to the `@VERSION` suffix).
    ///
    /// Must appear before the crate name (e.g. `cgx --crate-version 1.0 ripgrep`); a
    /// `--crate-version` after the crate name is passed through to the tool. The `@VERSION` suffix
    /// (`cgx ripgrep@1.0`) is the preferred form.
    #[arg(long, value_name = "REQ")]
    crate_version: Option<String>,

    #[command(flatten)]
    source: SourceArgs,

    #[command(flatten)]
    build_options: BuildOptionsArgs,

    #[command(flatten)]
    cargo: CargoBehaviorArgs,

    #[command(flatten)]
    prebuilt: PrebuiltBinaryArgs,

    #[command(flatten)]
    config: ConfigArgs,

    #[command(flatten)]
    http: HttpArgs,

    #[command(flatten)]
    reporting: ReportingArgs,

    /// The crate to run plus any trailing tool arguments, captured raw via an external subcommand.
    ///
    /// This is the trick we use to be able to capture anything other than a recognized argument as
    /// the name of a crate followed by args to that crate.
    #[command(subcommand)]
    invocation: Option<Invocation>,

    /// Toolchain extracted from a leading `+toolchain` token before clap parsing.
    ///
    /// Populated by [`Cli::extract_toolchain`], not parsed directly from the command line.
    #[arg(skip)]
    toolchain: Option<String>,
}

impl RawCli {
    /// Split the captured external-subcommand vector into a crate spec and trailing tool arguments.
    fn split_invocation(invocation: Option<Invocation>) -> (Option<String>, Vec<OsString>) {
        let Some(Invocation::Crate(mut parts)) = invocation else {
            return (None, Vec::new());
        };
        if parts.is_empty() {
            return (None, Vec::new());
        }

        let crate_spec = parts.remove(0).to_string_lossy().into_owned();
        let (crate_spec, mut tool_args) = if crate_spec == "cargo" && !parts.is_empty() {
            let subcommand = parts.remove(0);
            (format!("cargo-{}", subcommand.to_string_lossy()), parts)
        } else {
            (crate_spec, parts)
        };

        // A leading `--` immediately after the crate is the conventional argument separator; drop it
        // so `cgx rg -- --flag` forwards `--flag` to the tool, matching the no-separator
        // `cgx rg --flag`.
        if matches!(tool_args.first().and_then(|arg| arg.to_str()), Some("--")) {
            tool_args.remove(0);
        }

        (Some(crate_spec), tool_args)
    }

    /// Validate the raw parse and render it into a typed [`Cli`] command.
    ///
    /// This is the single place semantic rules live: the mode arg-group already guarantees at most
    /// one mode flag, and here we enforce which other flags each mode permits, whether a crate is
    /// required or forbidden, and whether trailing tool arguments are allowed.
    ///
    /// It would be nice if more of the rules about which args are allowed with which modes could
    /// be expressed usign `clap` proc macros, but we're already pushing the limits of clap as it
    /// is.  An earlier attempt at CLI parsing was even more horrifyingly manual and
    /// stringly-typed.
    fn render(self) -> Result<Cli, clap::Error> {
        let RawCli {
            no_exec,
            prefetch,
            prefetch_all,
            list_targets,
            list_tools,
            crate_version,
            source,
            build_options,
            cargo,
            prebuilt,
            config,
            http,
            reporting,
            invocation,
            toolchain,
        } = self;

        let (crate_spec, tool_args) = Self::split_invocation(invocation);

        if prefetch_all {
            let mode = "--prefetch-all";
            Self::ensure_no_crate(&crate_spec, &tool_args, mode)?;
            if source.is_present() {
                return Err(Self::forbidden(mode, "source selectors"));
            }
            if prebuilt.is_present() {
                return Err(Self::forbidden(mode, "--prebuilt-binary options"));
            }
            if cargo.locked || cargo.frozen || cargo.unlocked {
                return Err(Self::forbidden(mode, "--locked/--frozen/--unlocked"));
            }
            if crate_version.is_some() {
                return Err(Self::forbidden(mode, "--crate-version"));
            }
            if build_options.has_compilation_options() {
                return Err(Self::forbidden(mode, "build/compilation options"));
            }
            return Ok(Cli::PrefetchAll(PrefetchAll {
                config,
                http,
                reporting,
                offline: cargo.offline,
                refresh: cargo.refresh,
                jobs: build_options.jobs,
                ignore_rust_version: build_options.ignore_rust_version,
                toolchain,
            }));
        }

        if list_tools {
            let mode = "--list-tools";
            Self::ensure_no_crate(&crate_spec, &tool_args, mode)?;
            if source.is_present() {
                return Err(Self::forbidden(mode, "source selectors"));
            }
            if prebuilt.is_present() {
                return Err(Self::forbidden(mode, "--prebuilt-binary options"));
            }
            if cargo.locked || cargo.frozen || cargo.unlocked || cargo.offline || cargo.refresh {
                return Err(Self::forbidden(mode, "cargo behavior flags"));
            }
            if crate_version.is_some() {
                return Err(Self::forbidden(mode, "--crate-version"));
            }
            if build_options.is_present() {
                return Err(Self::forbidden(mode, "build options"));
            }
            if http.is_present() {
                return Err(Self::forbidden(mode, "--http-* options"));
            }
            if toolchain.is_some() {
                return Err(Self::forbidden(mode, "+toolchain"));
            }
            return Ok(Cli::ListTools(ListTools { config, reporting }));
        }

        // By this point we know that the command is one that operates on a specific crate, so
        // crate-level args are supported as well
        let git_selector = source.git_selector();
        let source = source.to_source();

        let mode = if prefetch {
            "--prefetch"
        } else if list_targets {
            "--list-targets"
        } else if no_exec {
            "--no-exec"
        } else {
            "cgx"
        };

        if crate_spec.is_none() && !source.allows_crate_discovery() {
            return Err(clap::Error::raw(
                ErrorKind::MissingRequiredArgument,
                format!("{mode} requires a crate name, or a discoverable source such as --git or --path\n"),
            ));
        }

        let crate_args = CrateArgs {
            crate_spec,
            crate_version,
            source,
            git_selector,
            build_options,
            cargo,
            prebuilt,
            config,
            http,
            reporting,
            toolchain,
        };

        if prefetch {
            Self::ensure_no_crate_args(&tool_args, "--prefetch")?;
            Ok(Cli::Prefetch(crate_args))
        } else if list_targets {
            Self::ensure_no_crate_args(&tool_args, "--list-targets")?;
            Ok(Cli::ListTargets(crate_args))
        } else if no_exec {
            Ok(Cli::NoExec(crate_args))
        } else {
            Ok(Cli::Run {
                args: crate_args,
                tool_args,
            })
        }
    }

    /// Build a clap error reporting that `what` is not allowed in the `mode` command.
    fn forbidden(mode: &str, what: &str) -> clap::Error {
        clap::Error::raw(
            ErrorKind::ArgumentConflict,
            format!("{what} cannot be used with {mode}\n"),
        )
    }

    /// Reject a crate spec or trailing arguments for the config-level commands that take neither.
    fn ensure_no_crate(
        crate_spec: &Option<String>,
        tool_args: &[OsString],
        mode: &str,
    ) -> Result<(), clap::Error> {
        if crate_spec.is_some() || !tool_args.is_empty() {
            Err(clap::Error::raw(
                ErrorKind::UnknownArgument,
                format!("{mode} does not accept a crate or trailing arguments\n"),
            ))
        } else {
            Ok(())
        }
    }

    /// Reject trailing arguments for crate-level commands that prepare but never execute a crate.
    fn ensure_no_crate_args(tool_args: &[OsString], mode: &str) -> Result<(), clap::Error> {
        if tool_args.is_empty() {
            Ok(())
        } else {
            Err(clap::Error::raw(
                ErrorKind::UnknownArgument,
                format!("{mode} cannot be used with trailing tool arguments\n"),
            ))
        }
    }
}

// External-subcommand capture for when the user has not specified one of the subcommands and is
// just running a crate
//
// Modeling the crate-and-arguments case as an external subcommand makes clap split the crate spec
// from the trailing tool arguments natively, with no `--` separator required and no reserved
// words.  It's a clever hack to get around the fact that we want `cgx foo` for literally any `foo`
// to refer to running crate `foo` but we also have some options like `--prefetch` and the like
// that really work like clap subcommands.
//
// NOTE: This deliberately is a regular comment, not a doc comment, so it does not become the
// command's `--help` text.
#[derive(Clone, Debug, Subcommand)]
enum Invocation {
    #[command(external_subcommand)]
    Crate(Vec<OsString>),
}

/// Source selectors for commands that operate on one concrete crate invocation, to specify the
/// source of a crate.
///
/// The mutually-exclusive selectors share clap arg-groups (`source` for the
/// forge/registry/path selectors, `git_ref` for the branch/tag/rev selectors) so clap rejects
/// invalid combinations natively, and [`Self::to_source`] collapses the flat flags into the
/// [`Source`] enum.
#[derive(Clone, Debug, Default, Args)]
struct SourceArgs {
    /// Find crate in git repository at the given URL
    #[arg(long, group = "source")]
    git: Option<String>,

    /// Name of registry (configured in .cargo/config.toml) in which to find crate
    #[arg(long, group = "source")]
    registry: Option<String>,

    /// Filesystem path to local crate to install from
    #[arg(long, group = "source")]
    path: Option<PathBuf>,

    /// Find crate in GitHub repository (format: owner/repo)
    #[arg(long, group = "source")]
    github: Option<String>,

    /// Find crate in GitLab repository (format: owner/repo)
    #[arg(long, group = "source")]
    gitlab: Option<String>,

    /// Registry index URL to use
    #[arg(long, group = "source", value_name = "INDEX")]
    index: Option<String>,

    /// Custom GitHub instance URL (for GitHub Enterprise)
    #[arg(long, requires = "github")]
    github_url: Option<String>,

    /// Custom GitLab instance URL (for self-hosted GitLab)
    #[arg(long, requires = "gitlab")]
    gitlab_url: Option<String>,

    /// Branch to use when installing from a git repo
    #[arg(long, group = "git_ref")]
    branch: Option<String>,

    /// Tag to use when installing from a git repo
    #[arg(long, group = "git_ref")]
    tag: Option<String>,

    /// Specific commit to use when installing from a git repo
    #[arg(long, group = "git_ref")]
    rev: Option<String>,
}

impl SourceArgs {
    /// True when any source or git-ref selector was given on the command line.
    fn is_present(&self) -> bool {
        self.git.is_some()
            || self.registry.is_some()
            || self.path.is_some()
            || self.github.is_some()
            || self.gitlab.is_some()
            || self.index.is_some()
            || self.github_url.is_some()
            || self.gitlab_url.is_some()
            || self.branch.is_some()
            || self.tag.is_some()
            || self.rev.is_some()
    }

    /// Collapse the mutually-exclusive `--branch`/`--tag`/`--rev` flags into a [`GitSelector`].
    fn git_selector(&self) -> GitSelector {
        match (&self.branch, &self.tag, &self.rev) {
            (Some(branch), None, None) => GitSelector::Branch(branch.clone()),
            (None, Some(tag), None) => GitSelector::Tag(tag.clone()),
            (None, None, Some(rev)) => GitSelector::Commit(rev.clone()),
            (None, None, None) => GitSelector::DefaultBranch,
            _ => unreachable!("BUG: the `git_ref` ArgGroup enforces mutual exclusivity"),
        }
    }

    /// Collapse the mutually-exclusive source selectors into the typed [`Source`] enum.
    fn to_source(&self) -> Source {
        if let Some(url) = &self.git {
            Source::Git { url: url.clone() }
        } else if let Some(name) = &self.registry {
            Source::Registry { name: name.clone() }
        } else if let Some(url) = &self.index {
            Source::Index { url: url.clone() }
        } else if let Some(path) = &self.path {
            Source::Path { path: path.clone() }
        } else if let Some(repo) = &self.github {
            Source::GitHub {
                repo: repo.clone(),
                custom_url: self.github_url.clone(),
            }
        } else if let Some(repo) = &self.gitlab {
            Source::GitLab {
                repo: repo.clone(),
                custom_url: self.gitlab_url.clone(),
            }
        } else {
            Source::Default
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use clap::CommandFactory;

    use super::*;
    use crate::{
        Result,
        builder::{BuildOptions, BuildTarget},
        config::{Config, ToolConfig},
        cratespec::{CrateSpec, Forge, RegistrySource},
        git::GitSelector,
    };

    /// Using `clap`'s built in afforance, assert that the `clap` definition is valid and won't
    /// panic at runtime when attempting to parse arguments
    #[test]
    fn verify_cli() {
        RawCli::command().debug_assert();
    }

    /// The repeated `-v` flag maps to a [`Verbosity`] level, saturating at the most verbose.
    #[test]
    fn verbosity_reflects_repeated_v_flag() {
        let cases = [
            (vec!["tool"], Verbosity::Normal),
            (vec!["-v", "tool"], Verbosity::Verbose),
            (vec!["-vv", "tool"], Verbosity::VeryVerbose),
            (vec!["-vvv", "tool"], Verbosity::ExtremelyVerbose),
            (vec!["-vvvv", "tool"], Verbosity::ExtremelyVerbose),
        ];
        for (args, expected) in cases {
            let verbosity = Cli::parse_from_test_args(args).verbosity();
            assert_eq!(verbosity, expected);
        }
    }

    mod cratespec {
        use super::*;
        fn parse_cratespec_from_args(args: &[&str]) -> Result<CrateSpec> {
            let cli = Cli::parse_from_test_args(args);
            let config = Config::default();
            let request = cli.crate_args().crate_request()?;
            CrateSpec::load(&config, &request)
        }

        #[test]
        fn test_simple_crate() {
            let cr = parse_cratespec_from_args(&["ripgrep"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: None } if name == "ripgrep"
            );
        }

        #[test]
        fn test_crate_with_at_version() {
            let cr = parse_cratespec_from_args(&["ripgrep@14"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: Some(ref v) }
                if name == "ripgrep" && v == &semver::VersionReq::parse("14").unwrap()
            );
        }

        #[test]
        fn test_crate_with_flag_version() {
            let cr = parse_cratespec_from_args(&["--crate-version", "14", "ripgrep"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: Some(ref v) }
                if name == "ripgrep" && v == &semver::VersionReq::parse("14").unwrap()
            );
        }

        #[test]
        fn test_crate_with_conflicting_versions() {
            // Users can only specify the crate semver version req one way, either
            // `--crate-version` or as part of the crate name.  It doesn't matter if they specify
            // the same version in both, or different versions, it's not allowed.
            let result = parse_cratespec_from_args(&["--crate-version", "14", "ripgrep@14"]);
            assert_matches!(result, Err(crate::error::Error::ConflictingVersions { .. }));

            let result = parse_cratespec_from_args(&["--crate-version", "15", "ripgrep@14"]);
            assert_matches!(result, Err(crate::error::Error::ConflictingVersions { .. }));
        }

        #[test]
        fn test_cargo_subcommand() {
            let cr = parse_cratespec_from_args(&["cargo", "deny"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: None } if name == "cargo-deny"
            );
        }

        #[test]
        fn test_cargo_subcommand_with_version() {
            let cr = parse_cratespec_from_args(&["cargo", "deny@1"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: Some(ref v) }
                if name == "cargo-deny" && v == &semver::VersionReq::parse("1").unwrap()
            );
        }

        #[test]
        fn test_git_source() {
            let cr = parse_cratespec_from_args(&["--git", "https://github.com/foo/bar", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "foo" && repo == "bar" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_with_branch() {
            let cr = parse_cratespec_from_args(&[
                "--git",
                "https://github.com/foo/bar",
                "--branch",
                "main",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::Branch(ref b),
                    ref name,
                    version: None
                } if owner == "foo" && repo == "bar" && b == "main" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_with_tag() {
            let cr = parse_cratespec_from_args(&[
                "--git",
                "https://github.com/foo/bar",
                "--tag",
                "v1.0",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::Tag(ref t),
                    ref name,
                    version: None
                } if owner == "foo" && repo == "bar" && t == "v1.0" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_with_rev() {
            let cr = parse_cratespec_from_args(&[
                "--git",
                "https://github.com/foo/bar",
                "--rev",
                "abc123",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::Commit(ref c),
                    ref name,
                    version: None
                } if owner == "foo" && repo == "bar" &&
                     c == "abc123" &&
                     name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_github_https_url() {
            let cr =
                parse_cratespec_from_args(&["--git", "https://github.com/owner/repo", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" && repo == "repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_github_https_url_with_git_suffix() {
            let cr = parse_cratespec_from_args(&["--git", "https://github.com/owner/repo.git", "mycrate"])
                .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" && repo == "repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_gitlab_https_url() {
            let cr =
                parse_cratespec_from_args(&["--git", "https://gitlab.com/owner/repo", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitLab {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" && repo == "repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_scheme_not_transformed() {
            let cr = parse_cratespec_from_args(&["--git", "git://github.com/owner/repo", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Git { ref repo, selector: GitSelector::DefaultBranch, ref name, version: None }
                if repo == "git://github.com/owner/repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_custom_domain_not_transformed() {
            let cr =
                parse_cratespec_from_args(&["--git", "https://github.enterprise.com/owner/repo", "mycrate"])
                    .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Git { ref repo, selector: GitSelector::DefaultBranch, ref name, version: None }
                if repo == "https://github.enterprise.com/owner/repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_github_url_with_extra_path_not_transformed() {
            let cr =
                parse_cratespec_from_args(&["--git", "https://github.com/owner/repo/pull/15", "mycrate"])
                    .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Git { ref repo, selector: GitSelector::DefaultBranch, ref name, version: None }
                if repo == "https://github.com/owner/repo/pull/15" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_git_github_url_with_tree_path_not_transformed() {
            let cr = parse_cratespec_from_args(&[
                "--git",
                "https://github.com/owner/repo/tree/master/some/path",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Git { ref repo, selector: GitSelector::DefaultBranch, ref name, version: None }
                if repo == "https://github.com/owner/repo/tree/master/some/path" &&
                   name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_registry() {
            let cr = parse_cratespec_from_args(&["--registry", "my-registry", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Registry {
                    source: RegistrySource::Named(ref registry),
                    ref name,
                    version: None
                } if registry == "my-registry" && name == "mycrate"
            );
        }

        #[test]
        fn test_index() {
            let cr =
                parse_cratespec_from_args(&["--index", "https://my-index.com/git/index", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Registry {
                    source: RegistrySource::IndexUrl(ref index),
                    ref name,
                    version: None
                } if index.as_str() == "https://my-index.com/git/index" && name == "mycrate"
            );
        }

        #[test]
        fn test_index_with_version() {
            let cr = parse_cratespec_from_args(&["--index", "sparse+https://my-index.com/", "mycrate@1.0"])
                .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Registry {
                    source: RegistrySource::IndexUrl(ref index),
                    ref name,
                    version: Some(ref v)
                } if index.as_str() == "sparse+https://my-index.com/" &&
                     name == "mycrate" &&
                     v == &semver::VersionReq::parse("1.0").unwrap()
            );
        }

        #[test]
        fn test_local_path() {
            let cr = parse_cratespec_from_args(&["--path", "./my-crate", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::LocalDir { ref path, ref name, version: None }
                if path.to_str().unwrap() == "./my-crate" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_github() {
            let cr = parse_cratespec_from_args(&["--github", "owner/repo", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" && repo == "repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_github_with_custom_url() {
            let cr = parse_cratespec_from_args(&[
                "--github",
                "owner/repo",
                "--github-url",
                "https://github.mycorp.com",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: Some(ref url),
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" &&
                     repo == "repo" &&
                     name.as_deref() == Some("mycrate") &&
                     url.as_str() == "https://github.mycorp.com/"
            );
        }

        #[test]
        fn test_github_with_branch() {
            let cr = parse_cratespec_from_args(&["--github", "owner/repo", "--branch", "develop", "mycrate"])
                .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::Branch(ref b),
                    ref name,
                    version: None
                } if owner == "owner" &&
                     repo == "repo" &&
                     b == "develop" &&
                     name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_gitlab() {
            let cr = parse_cratespec_from_args(&["--gitlab", "owner/repo", "mycrate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitLab {
                        custom_url: None,
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" && repo == "repo" && name.as_deref() == Some("mycrate")
            );
        }

        #[test]
        fn test_gitlab_with_custom_url() {
            let cr = parse_cratespec_from_args(&[
                "--gitlab",
                "owner/repo",
                "--gitlab-url",
                "https://gitlab.mycorp.com",
                "mycrate",
            ])
            .unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitLab {
                        custom_url: Some(ref url),
                        ref owner,
                        ref repo
                    },
                    selector: GitSelector::DefaultBranch,
                    ref name,
                    version: None
                } if owner == "owner" &&
                     repo == "repo" &&
                     name.as_deref() == Some("mycrate") &&
                     url.as_str() == "https://gitlab.mycorp.com/"
            );
        }

        #[test]
        fn test_git_selector_without_git_source() {
            let result = parse_cratespec_from_args(&["--branch", "main", "mycrate"]);
            assert_matches!(result, Err(crate::error::Error::GitSelectorWithoutGitSource));
        }

        #[test]
        fn test_invalid_repo_format() {
            let result = parse_cratespec_from_args(&["--github", "invalid-repo", "mycrate"]);
            assert_matches!(result, Err(crate::error::Error::InvalidRepoFormat { .. }));
        }

        #[test]
        fn test_invalid_version() {
            let result = parse_cratespec_from_args(&["ripgrep@not-a-version"]);
            assert_matches!(result, Err(crate::error::Error::InvalidVersionReq { .. }));
        }

        #[test]
        fn test_invalid_index_url() {
            let result = parse_cratespec_from_args(&["--index", "not-a-valid-url", "mycrate"]);
            assert_matches!(result, Err(crate::error::Error::InvalidUrl { .. }));
        }

        #[test]
        fn test_git_without_crate_name() {
            let cr = parse_cratespec_from_args(&["--git", "https://github.com/foo/bar"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::DefaultBranch,
                    name: None,
                    version: None
                } if owner == "foo" && repo == "bar"
            );
        }

        #[test]
        fn test_github_without_crate_name() {
            let cr = parse_cratespec_from_args(&["--github", "owner/repo"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::DefaultBranch,
                    name: None,
                    version: None
                } if owner == "owner" && repo == "repo"
            );
        }

        #[test]
        fn test_gitlab_without_crate_name() {
            let cr = parse_cratespec_from_args(&["--gitlab", "owner/repo"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::Forge {
                    forge: Forge::GitLab { custom_url: None, ref owner, ref repo },
                    selector: GitSelector::DefaultBranch,
                    name: None,
                    version: None
                } if owner == "owner" && repo == "repo"
            );
        }

        #[test]
        fn test_path_without_crate_name() {
            let cr = parse_cratespec_from_args(&["--path", "./my-crate"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::LocalDir { ref path, name: None, version: None }
                if path.to_str().unwrap() == "./my-crate"
            );
        }
    }

    mod build_options {
        use super::*;

        fn parse_build_options_from_args(args: &[&str]) -> Result<BuildOptions> {
            let cli = Cli::parse_from_test_args(args);
            let config = Config::default();
            BuildOptions::load(&config, &cli.crate_args().to_build_overrides())
        }

        #[test]
        fn test_features_parsing_comma_separated() {
            let opts = parse_build_options_from_args(&["--features", "foo,bar,baz", "ripgrep"]).unwrap();
            assert_eq!(opts.features, vec!["foo", "bar", "baz"]);
        }

        #[test]
        fn test_features_parsing_space_separated() {
            let opts = parse_build_options_from_args(&["--features", "foo bar baz", "ripgrep"]).unwrap();
            assert_eq!(opts.features, vec!["foo", "bar", "baz"]);
        }

        #[test]
        fn test_features_parsing_mixed_separators() {
            let opts = parse_build_options_from_args(&["--features", "foo, bar baz", "ripgrep"]).unwrap();
            assert_eq!(opts.features, vec!["foo", "bar", "baz"]);
        }

        #[test]
        fn test_features_parsing_with_extra_whitespace() {
            let opts = parse_build_options_from_args(&["--features", "  foo  ,  bar  ", "ripgrep"]).unwrap();
            assert_eq!(opts.features, vec!["foo", "bar"]);
        }

        #[test]
        fn test_all_features() {
            let opts = parse_build_options_from_args(&["--all-features", "ripgrep"]).unwrap();
            assert!(opts.all_features);
        }

        #[test]
        fn test_no_default_features() {
            let opts = parse_build_options_from_args(&["--no-default-features", "ripgrep"]).unwrap();
            assert!(opts.no_default_features);
        }

        #[test]
        fn test_debug_maps_to_dev_profile() {
            let opts = parse_build_options_from_args(&["--debug", "ripgrep"]).unwrap();
            assert_eq!(opts.profile, Some("dev".to_string()));
        }

        #[test]
        fn test_profile_custom() {
            let opts =
                parse_build_options_from_args(&["--profile", "release-with-debug", "ripgrep"]).unwrap();
            assert_eq!(opts.profile, Some("release-with-debug".to_string()));
        }

        #[test]
        fn test_config_locked_and_offline_both_true() {
            // BuildOptions reads locked/offline from Config, not CLI args.
            // CLI override tests (--locked, --unlocked, --frozen, --offline) belong in config.rs
            // since that's where CLI-to-Config override logic lives.
            let cli = Cli::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: true,
                offline: true,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();
            assert!(opts.locked);
            assert!(opts.offline);
        }

        #[test]
        fn test_config_locked_without_offline() {
            let cli = Cli::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: true,
                offline: false,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();
            assert!(opts.locked);
            assert!(!opts.offline);
        }

        #[test]
        fn test_config_offline_without_locked() {
            let cli = Cli::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: false,
                offline: true,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();
            assert!(!opts.locked);
            assert!(opts.offline);
        }

        #[test]
        fn test_target() {
            let opts =
                parse_build_options_from_args(&["--target", "x86_64-unknown-linux-musl", "ripgrep"]).unwrap();
            assert_eq!(opts.target, Some("x86_64-unknown-linux-musl".to_string()));
        }

        #[test]
        fn test_jobs() {
            let opts = parse_build_options_from_args(&["-j", "4", "ripgrep"]).unwrap();
            assert_eq!(opts.jobs, Some(4));
        }

        #[test]
        fn test_ignore_rust_version() {
            let opts = parse_build_options_from_args(&["--ignore-rust-version", "ripgrep"]).unwrap();
            assert!(opts.ignore_rust_version);
        }

        #[test]
        fn test_build_options_defaults() {
            let opts = parse_build_options_from_args(&["ripgrep"]).unwrap();
            assert_eq!(opts, Default::default());
        }

        #[test]
        fn test_bin_flag() {
            let opts = parse_build_options_from_args(&["--bin", "mybinary", "ripgrep"]).unwrap();
            assert_eq!(opts.build_target, BuildTarget::Bin("mybinary".to_string()));
            assert_eq!(opts.build_target, BuildTarget::Bin("mybinary".to_string()));
        }

        #[test]
        fn test_example_flag() {
            let opts = parse_build_options_from_args(&["--example", "myexample", "ripgrep"]).unwrap();
            assert_eq!(opts.build_target, BuildTarget::Example("myexample".to_string()));
        }
    }

    mod toolchain_tests {
        use super::*;

        #[test]
        fn test_extract_toolchain_nightly() {
            let args = vec!["cgx", "+nightly", "ripgrep"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, Some("nightly".to_string()));
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_extract_toolchain_specific_version() {
            let args = vec!["cgx", "+1.70.0", "ripgrep"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain.as_deref(), Some("1.70.0"));
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_extract_toolchain_stable() {
            let args = vec!["cgx", "+stable", "ripgrep"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, Some("stable".to_string()));
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_extract_toolchain_with_other_flags() {
            let args = vec![
                "cgx",
                "+nightly",
                "--git",
                "https://github.com/foo/bar",
                "mycrate",
            ];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, Some("nightly".to_string()));
            assert_eq!(
                filtered,
                vec!["cgx", "--git", "https://github.com/foo/bar", "mycrate"]
            );
        }

        #[test]
        fn test_no_toolchain() {
            let args = vec!["cgx", "ripgrep"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_bare_plus() {
            let args = vec!["cgx", "+", "ripgrep"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "+", "ripgrep"]);
        }

        #[test]
        fn test_plus_in_middle_not_toolchain() {
            let args = vec!["cgx", "ripgrep", "+something"];
            let (toolchain, filtered) = Cli::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "ripgrep", "+something"]);
        }

        #[test]
        fn test_toolchain_with_version_flag() {
            // `--version` after the crate name is a tool argument, not cgx's: the crate is ripgrep
            // and `--version 14` is forwarded to it.
            let args = vec!["+nightly", "ripgrep", "--version", "14"];
            let cli = Cli::parse_from_test_args(args);

            let invocation = cli.crate_args();
            assert_eq!(invocation.toolchain.as_deref(), Some("nightly"));
            assert_eq!(invocation.crate_spec.as_deref(), Some("ripgrep"));

            let tool_args: Vec<&str> = cli.tool_args().iter().map(|arg| arg.to_str().unwrap()).collect();
            assert_eq!(tool_args, ["--version", "14"]);
        }

        #[test]
        fn test_toolchain_propagates_from_config_to_build_options() {
            // BuildOptions reads toolchain from Config, not from CLI args directly.
            // CLI toolchain override (+nightly) is applied by Config::load_from_dir().
            let args = vec!["ripgrep"];
            let cli = Cli::parse_from_test_args(args);

            let config = Config {
                toolchain: Some("nightly".to_string()),
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();
            assert_eq!(opts.toolchain, Some("nightly".to_string()));
        }

        #[test]
        fn test_no_toolchain_in_build_options() {
            let args = vec!["ripgrep"];
            let cli = Cli::parse_from_test_args(args);

            let config = Config::default();
            let opts = BuildOptions::load(&config, &cli.crate_args().to_build_overrides()).unwrap();
            assert_eq!(opts.toolchain, None);
        }
    }

    mod config_overrides {
        use super::*;

        #[test]
        fn test_config_overrides_can_combine() {
            // Verify that system-config-dir, app-dir, and user-config-dir work together.
            let inputs = Cli::parse_from_test_args([
                "--system-config-dir",
                "/tmp/system",
                "--app-dir",
                "/tmp/app",
                "--user-config-dir",
                "/tmp/user",
                "ripgrep",
            ])
            .to_config_overrides();
            assert_eq!(inputs.system_config_dir, Some(PathBuf::from("/tmp/system")));
            assert_eq!(inputs.app_dir, Some(PathBuf::from("/tmp/app")));
            assert_eq!(inputs.user_config_dir, Some(PathBuf::from("/tmp/user")));
        }
    }

    mod mode_validation {
        use super::*;

        #[test]
        fn prefetch_accepts_single_tool_build_options() {
            let cli = Cli::try_parse_from_test_args([
                "--prefetch",
                "--features",
                "frobnulator",
                "--bin",
                "timestamp",
                "timestamp",
            ])
            .unwrap();

            assert_matches!(&cli, Cli::Prefetch(_));
            let invocation = cli.crate_args();
            assert_eq!(invocation.crate_spec.as_deref(), Some("timestamp"));
            assert_eq!(invocation.build_options.features.as_deref(), Some("frobnulator"));
            assert!(cli.tool_args().is_empty());
        }

        #[test]
        fn prefetch_rejects_trailing_args() {
            let result = Cli::try_parse_from_test_args(["--prefetch", "timestamp", "--help"]);
            assert_matches!(result, Err(_));
        }

        #[test]
        fn prefetch_rejects_no_exec() {
            let result = Cli::try_parse_from_test_args(["--prefetch", "--no-exec", "timestamp"]);
            assert_matches!(result, Err(_));
        }

        #[test]
        fn prefetch_all_accepts_allowed_controls() {
            let cli = Cli::try_parse_from_test_args([
                "--prefetch-all",
                "--offline",
                "--refresh",
                "--jobs",
                "2",
                "--ignore-rust-version",
                "--http-timeout",
                "5s",
                "--system-config-dir",
                "/tmp/system",
                "--app-dir",
                "/tmp/app",
                "--user-config-dir",
                "/tmp/user",
                "--message-format",
                "json",
            ])
            .unwrap();

            let Cli::PrefetchAll(prefetch_all) = cli else {
                panic!("expected a prefetch-all command");
            };
            assert!(prefetch_all.offline);
            assert!(prefetch_all.refresh);
            assert_eq!(prefetch_all.jobs, Some(2));
            assert!(prefetch_all.ignore_rust_version);
            assert_eq!(prefetch_all.http.http_timeout.as_deref(), Some("5s"));
            assert_eq!(
                prefetch_all.config.system_config_dir,
                Some(PathBuf::from("/tmp/system"))
            );
            assert_eq!(prefetch_all.config.app_dir, Some(PathBuf::from("/tmp/app")));
            assert_eq!(
                prefetch_all.config.user_config_dir,
                Some(PathBuf::from("/tmp/user"))
            );
        }

        #[test]
        fn prefetch_all_controls_reach_loaders() {
            // The parse test above checks the raw flags properly map to `PrefetchAll`; this checks they
            // actually flow through into `BuildOptions` and `Config`
            let cli = Cli::parse_from_test_args([
                "--prefetch-all",
                "--offline",
                "--refresh",
                "--jobs",
                "2",
                "--ignore-rust-version",
            ]);
            let Cli::PrefetchAll(prefetch_all) = &cli else {
                panic!("expected a prefetch-all command");
            };

            // Build knobs reach BuildOptions.
            let build_options =
                BuildOptions::load(&Config::default(), &prefetch_all.to_build_overrides()).unwrap();
            assert_eq!(build_options.jobs, Some(2));
            assert!(build_options.ignore_rust_version);

            // Network/refresh knobs reach Config, loaded from an isolated tree so host config
            // cannot interfere with the assertion.
            let temp = tempfile::tempdir().unwrap();
            let mut overrides = prefetch_all.to_config_overrides();
            overrides.system_config_dir = Some(temp.path().join("system"));
            overrides.user_config_dir = Some(temp.path().join("user"));
            let config = Config::load_from_dir(temp.path(), &overrides).unwrap();
            assert!(config.offline);
            assert!(config.refresh);
        }

        #[test]
        fn prefetch_all_rejects_crate_source_build_and_trailing_args() {
            for args in [
                vec!["--prefetch-all", "timestamp"],
                vec!["--prefetch-all", "--path", "."],
                vec!["--prefetch-all", "--features", "frobnulator"],
                vec!["--prefetch-all", "--all-features"],
                vec!["--prefetch-all", "--locked"],
                vec!["--prefetch-all", "--prebuilt-binary", "never"],
                vec!["--prefetch-all", "--", "--help"],
            ] {
                let result = Cli::try_parse_from_test_args(args);
                assert_matches!(result, Err(_));
            }
        }

        #[test]
        fn list_tools_accepts_config_discovery_and_message_format() {
            let cli = Cli::try_parse_from_test_args([
                "--list-tools",
                "--config-file",
                "cgx.toml",
                "--message-format",
                "json",
            ])
            .unwrap();

            let Cli::ListTools(list_tools) = cli else {
                panic!("expected a list-tools command");
            };
            assert_eq!(list_tools.config.config_file, Some(PathBuf::from("cgx.toml")));
            assert_matches!(list_tools.reporting.message_format, Some(MessageFormat::Json));

            let cli = Cli::try_parse_from_test_args([
                "--list-tools",
                "--system-config-dir",
                "/tmp/system",
                "--app-dir",
                "/tmp/app",
                "--user-config-dir",
                "/tmp/user",
                "--message-format",
                "json",
            ])
            .unwrap();

            let Cli::ListTools(list_tools) = cli else {
                panic!("expected a list-tools command");
            };
            assert_eq!(
                list_tools.config.system_config_dir,
                Some(PathBuf::from("/tmp/system"))
            );
            assert_eq!(list_tools.config.app_dir, Some(PathBuf::from("/tmp/app")));
            assert_eq!(
                list_tools.config.user_config_dir,
                Some(PathBuf::from("/tmp/user"))
            );
            assert_matches!(list_tools.reporting.message_format, Some(MessageFormat::Json));
        }

        #[test]
        fn list_tools_rejects_operational_flags() {
            for args in [
                vec!["--list-tools", "timestamp"],
                vec!["--list-tools", "--offline"],
                vec!["--list-tools", "--features", "frobnulator"],
                vec!["--list-tools", "--prefetch"],
            ] {
                let result = Cli::try_parse_from_test_args(args);
                assert_matches!(result, Err(_));
            }
        }

        /// If `--features` isn't present on the command line and the config TOML has an entry for
        /// the crate in the `[tools]` section that specifies features, those features are applied
        /// to the build options for that crate.
        #[test]
        fn config_features_apply_when_cli_features_absent() {
            let cli = Cli::parse_from_test_args(["timestamp"]);
            let mut config = Config::default();
            config.tools.insert(
                "timestamp".to_string(),
                ToolConfig::Detailed {
                    version: None,
                    features: Some(vec!["frobnulator".to_string()]),
                    registry: None,
                    git: None,
                    branch: None,
                    tag: None,
                    rev: None,
                    path: None,
                },
            );

            let options = BuildOptions::load_for_crate(
                &config,
                &cli.crate_args().to_build_overrides(),
                &CrateSpec::CratesIo {
                    name: "timestamp".to_string(),
                    version: None,
                },
            )
            .unwrap();

            assert_eq!(options.features, vec!["frobnulator"]);
        }

        /// When the config TOML contains a crate in the `[tools]` section that specifies features,
        /// that is overridden by any features specified on the CLI.
        #[test]
        fn cli_features_replace_config_features() {
            let cli = Cli::parse_from_test_args(["--features", "gonkolator", "timestamp"]);
            let mut config = Config::default();
            config.tools.insert(
                "timestamp".to_string(),
                ToolConfig::Detailed {
                    version: None,
                    features: Some(vec!["frobnulator".to_string()]),
                    registry: None,
                    git: None,
                    branch: None,
                    tag: None,
                    rev: None,
                    path: None,
                },
            );

            let options = BuildOptions::load_for_crate(
                &config,
                &cli.crate_args().to_build_overrides(),
                &CrateSpec::CratesIo {
                    name: "timestamp".to_string(),
                    version: None,
                },
            )
            .unwrap();

            assert_eq!(options.features, vec!["gonkolator"]);
        }
    }

    mod run_invocation {
        use super::*;

        /// Parse run-path args (without the leading executable name) into a [`Cli`].
        fn run(args: &[&str]) -> Cli {
            Cli::parse_from_test_args(args)
        }

        /// The forwarded tool arguments, as owned strings for convenient comparison.
        fn tool_args(cli: &Cli) -> Vec<String> {
            cli.tool_args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn crate_then_tool_args_pass_through_without_dashdash() {
            let cli = run(&["ripgrep", "--color=always", "-i"]);
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("ripgrep"));
            assert_eq!(tool_args(&cli), ["--color=always", "-i"]);
        }

        #[test]
        fn cgx_flags_before_crate_are_not_tool_args() {
            let cli = run(&["--features", "foo", "ripgrep", "--color=always", "-i"]);
            let invocation = cli.crate_args();
            assert_eq!(invocation.crate_spec.as_deref(), Some("ripgrep"));
            assert_eq!(invocation.build_options.features.as_deref(), Some("foo"));
            assert_eq!(tool_args(&cli), ["--color=always", "-i"]);
        }

        #[test]
        fn same_flag_before_and_after_crate_is_split_by_position() {
            // `-F x` is cgx's features flag; `-F y` after the crate is forwarded to the tool.
            let cli = run(&["-F", "x", "ripgrep", "-F", "y"]);
            assert_eq!(cli.crate_args().build_options.features.as_deref(), Some("x"));
            assert_eq!(tool_args(&cli), ["-F", "y"]);
        }

        #[test]
        fn version_flag_after_crate_is_a_tool_arg() {
            // The hard requirement: `cgx <crate> --version` runs the tool with `--version` and must
            // NOT print cgx's own version.
            let cli = run(&["ripgrep", "--version"]);
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("ripgrep"));
            assert_eq!(tool_args(&cli), ["--version"]);

            let cli = run(&["ripgrep", "-V"]);
            assert_eq!(tool_args(&cli), ["-V"]);
        }

        #[test]
        fn explicit_dashdash_forwards_following_args() {
            let cli = run(&["ripgrep", "--", "--version"]);
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("ripgrep"));
            assert_eq!(tool_args(&cli), ["--version"]);
        }

        #[test]
        fn at_version_suffix_with_tool_version_flag() {
            let cli = run(&["eza@=0.23.1", "--version"]);
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("eza@=0.23.1"));
            assert_eq!(tool_args(&cli), ["--version"]);
        }

        #[test]
        fn cargo_subcommand_is_normalized_and_remaining_args_forwarded() {
            let cli = run(&["cargo", "deny", "--all"]);
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("cargo-deny"));
            assert_eq!(tool_args(&cli), ["--all"]);
        }

        #[test]
        fn crate_named_like_a_mode_flag_runs_that_crate() {
            // `prefetch` (no dashes) is a crate name, not the `--prefetch` mode.
            let cli = run(&["prefetch"]);
            assert_matches!(&cli, Cli::Run { .. });
            assert_eq!(cli.crate_args().crate_spec.as_deref(), Some("prefetch"));
        }
    }

    mod http_args {
        use super::*;

        #[test]
        fn test_http_timeout_cli_arg() {
            let cli = Cli::parse_from_test_args(["--http-timeout", "2m", "test-crate"]);
            assert_eq!(cli.crate_args().http.http_timeout, Some("2m".to_string()));
        }

        #[test]
        fn test_http_retries_cli_arg() {
            let cli = Cli::parse_from_test_args(["--http-retries", "5", "test-crate"]);
            assert_eq!(cli.crate_args().http.http_retries, Some(5));
        }

        #[test]
        fn test_http_proxy_cli_arg() {
            let cli = Cli::parse_from_test_args(["--http-proxy", "socks5://localhost:1080", "test-crate"]);
            assert_eq!(
                cli.crate_args().http.http_proxy,
                Some("socks5://localhost:1080".to_string())
            );
        }

        #[test]
        fn test_http_args_default_none() {
            let cli = Cli::parse_from_test_args(["test-crate"]);
            assert_eq!(cli.crate_args().http.http_timeout, None);
            assert_eq!(cli.crate_args().http.http_retries, None);
            assert_eq!(cli.crate_args().http.http_proxy, None);
        }

        #[test]
        fn test_http_args_after_crate_spec_are_binary_args() {
            let cli = Cli::parse_from_test_args(["test-crate", "--http-timeout", "5s"]);
            assert_eq!(cli.crate_args().http.http_timeout, None);
            let tool_args: Vec<&str> = cli.tool_args().iter().map(|arg| arg.to_str().unwrap()).collect();
            assert_eq!(tool_args, ["--http-timeout", "5s"]);
        }
    }

    mod strip_cargo_subcommand_arg {
        use super::*;

        #[test]
        fn test_leaves_normal_invocation_unchanged() {
            let args = vec!["cgx", "ripgrep", "--help"];
            let result = Cli::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_leaves_cargo_without_cgx_unchanged() {
            let args = vec!["cargo-cgx", "ripgrep", "--help"];
            let result = Cli::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_empty_args() {
            let args: Vec<String> = vec![];
            let result = Cli::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_single_arg() {
            let args = vec!["cargo-cgx"];
            let result = Cli::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }
    }
}
