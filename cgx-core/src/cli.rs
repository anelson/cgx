use std::{collections::HashSet, path::PathBuf};

use clap::{ArgAction, CommandFactory, Parser, ValueEnum, builder::TypedValueParser};
use strum::VariantNames;

use crate::config::{BinaryProvider, UsePrebuiltBinaries};

/// Creates a clap value parser that uses strum's [`VariantNames`] for possible values
/// and strum's [`FromStr`](std::str::FromStr) for parsing. This ensures:
/// - `--help` shows valid values (from `VARIANTS`)
/// - Parsing uses the same logic as config files (strum's [`EnumString`](strum::EnumString))
macro_rules! strum_value_parser {
    ($t:ty) => {
        clap::builder::PossibleValuesParser::new(<$t>::VARIANTS).map(|s| s.parse::<$t>().unwrap())
    };
}

/// Output format for structured messages.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MessageFormat {
    /// JSON format, one message per line
    Json,
}

/// CLI arguments that are crate-specific and passed through to cargo build.
///
/// These args are segreated from the other CLI args to make the semantic distinction more
/// explicit.  Most CLI args can also be set in config files to apply globally, but these are
/// always crate-specific.
#[derive(Clone, Debug, Default, Parser)]
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

#[derive(Clone, Debug, Parser)]
#[command(name = "cgx")]
#[command(about = "Rust equivalent of uvx or npx, for use with Rust crates")]
#[command(disable_version_flag = true)]
#[non_exhaustive]
pub struct CliArgs {
    /// Rust toolchain to use for building (e.g., +nightly, +stable, +1.70.0)
    ///
    /// This field is populated via pre-processing before clap parsing and is not directly
    /// parsed from command line arguments.
    #[arg(skip)]
    pub toolchain: Option<String>,

    /// Find crate in git repository at the given URL
    #[arg(long, conflicts_with_all = ["registry", "path", "github", "gitlab", "index"])]
    pub git: Option<String>,

    /// Name of registry (configured in .cargo/config.toml) in which to find crate
    #[arg(long, conflicts_with_all = ["git", "path", "github", "gitlab", "index"])]
    pub registry: Option<String>,

    /// Filesystem path to local crate to install from
    #[arg(long, conflicts_with_all = ["git", "registry", "github", "gitlab", "index"])]
    pub path: Option<PathBuf>,

    /// Find crate in GitHub repository (format: owner/repo)
    #[arg(long, conflicts_with_all = ["git", "registry", "path", "gitlab", "index"])]
    pub github: Option<String>,

    /// Find crate in GitLab repository (format: owner/repo)
    #[arg(long, conflicts_with_all = ["git", "registry", "path", "github", "index"])]
    pub gitlab: Option<String>,

    /// Registry index URL to use
    #[arg(long, conflicts_with_all = ["git", "registry", "path", "github", "gitlab"], value_name = "INDEX")]
    pub index: Option<String>,

    /// Custom GitHub instance URL (for GitHub Enterprise)
    #[arg(long, requires = "github")]
    pub github_url: Option<String>,

    /// Custom GitLab instance URL (for self-hosted GitLab)
    #[arg(long, requires = "gitlab")]
    pub gitlab_url: Option<String>,

    /// Branch to use when installing from a git repo
    #[arg(long, conflicts_with_all = ["tag", "rev"])]
    pub branch: Option<String>,

    /// Tag to use when installing from a git repo
    #[arg(long, conflicts_with_all = ["branch", "rev"])]
    pub tag: Option<String>,

    /// Specific commit to use when installing from a git repo
    #[arg(long, conflicts_with_all = ["branch", "tag"])]
    pub rev: Option<String>,

    /// Print version information, or specify a crate version to install.
    ///
    /// When used without a value (e.g., `cgx --version`), prints the version of cgx itself.
    /// When used with a value (e.g., `cgx foo --version 1.0`), specifies the version of the
    /// crate to install (alternative to @VERSION suffix in crate name).
    #[arg(short = 'V', long, num_args = 0..=1, default_missing_value = "", value_name = "VERSION")]
    pub version: Option<String>,

    /// Build-specific options that are passed through to cargo.
    #[command(flatten)]
    pub build_options: BuildOptionsArgs,

    /// Honor Cargo.lock from the crate, equivalent to passing `--locked` to `cargo install`
    ///
    /// This is enabled by default; the command-line flag is present only for compatibility with
    /// `cargo install` command lines.
    ///
    /// Unlike `cargo install`, `cgx` defaults to `--locked` behavior because this is almost always
    /// preferable to re-resoving dependencies to versions that the binary crate author possibly
    /// didn't test with.
    ///
    /// Use --unlocked to ignore Cargo.lock entirely, which is what `cargo install` does if not
    /// passed the `--locked` flag.
    #[arg(long, conflicts_with = "unlocked")]
    pub locked: bool,

    /// Equivalent to specifying both --locked and --offline
    #[arg(long, conflicts_with = "unlocked")]
    pub frozen: bool,

    /// Ignore Cargo.lock and resolve dependencies fresh
    ///
    /// Deletes Cargo.lock from build directory before building,
    /// forcing fresh dependency resolution. This mimics `cargo install` (without --locked).
    #[arg(long, conflicts_with_all = ["locked", "frozen"])]
    pub unlocked: bool,

    /// Run without accessing the network
    #[arg(long)]
    pub offline: bool,

    /// Use verbose output (-vv very verbose/build.rs output)
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Do not print cargo log messages
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Coloring: auto, always, never
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,

    /// Read configuration options from the given TOML file only, bypassing the usual config search
    /// paths.
    ///
    /// By default, cgx will look for a file in the current directory called `cgx.toml`, if not
    /// found it will check the parent, and the grandparent, up to the root.
    ///
    /// It will also read a `cgx.toml` file in the user's config directory, and it will read a
    /// system-level `cgx.toml` at `/etc/cgx.toml`, or the equivalent on other OSes.
    ///
    /// All config files' options are merged, with highest priority given to the file closest to
    /// the current directory.
    ///
    /// Specifying a config file with this option disables that logic, and
    /// reads the config only from the specified file.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = ["system_config_dir", "app_dir", "user_config_dir"]
    )]
    pub config_file: Option<PathBuf>,

    /// Override the system config directory location.
    ///
    /// When set, cgx will look for `cgx.toml` in this directory instead of the default
    /// system location (`/etc` on Unix, `%ProgramData%\cgx` on Windows).
    ///
    /// This is primarily useful for testing and CI environments where you need complete
    /// control over config file locations without modifying system directories.
    ///
    /// Can also be set via the `CGX_SYSTEM_CONFIG_DIR` environment variable, with the
    /// command-line argument taking precedence.
    #[arg(long, value_name = "PATH", env = "CGX_SYSTEM_CONFIG_DIR")]
    pub system_config_dir: Option<PathBuf>,

    /// Override the base application directory.
    ///
    /// When set, cgx uses this as the root for all application data:
    /// - Config: `<app-dir>/config/cgx.toml`
    /// - Cache: `<app-dir>/cache/`
    /// - Binaries: `<app-dir>/bins/`
    /// - Build artifacts: `<app-dir>/build/`
    ///
    /// This provides complete isolation of cgx's data, useful for testing, CI, or
    /// managing multiple independent cgx environments.
    ///
    /// Can also be set via the `CGX_APP_DIR` environment variable, with the
    /// command-line argument taking precedence.
    #[arg(long, value_name = "PATH", env = "CGX_APP_DIR")]
    pub app_dir: Option<PathBuf>,

    /// Override the user config directory location.
    ///
    /// When set, cgx will look for `cgx.toml` in this directory instead of the default
    /// user config location (typically `$XDG_CONFIG_HOME/cgx` or platform equivalent).
    ///
    /// This option is more specific than `--app-dir`: when both are set, `--user-config-dir`
    /// determines where cgx looks for the user config file, while `--app-dir` determines
    /// the locations for cache, bins, and build directories.
    ///
    /// Can also be set via the `CGX_USER_CONFIG_DIR` environment variable, with the
    /// command-line argument taking precedence.
    #[arg(long, value_name = "PATH", env = "CGX_USER_CONFIG_DIR")]
    pub user_config_dir: Option<PathBuf>,

    /// HTTP request timeout (e.g., "30s", "2m").
    ///
    /// Controls the per-request timeout for registry queries, binary downloads,
    /// and API calls.
    ///
    /// For git operations over HTTP/S, this timeout is also used by gix for both
    /// connection timeout and stalled-transfer timeout detection.
    ///
    /// If not set, cgx honors the Cargo environment variable `CARGO_HTTP_TIMEOUT`
    /// (integer seconds). If not set, defaults to 30s.
    #[arg(long, value_name = "DURATION", env = "CGX_HTTP_TIMEOUT")]
    pub http_timeout: Option<String>,

    /// Maximum number of retries for transient HTTP failures (0 = no retries).
    ///
    /// When an HTTP request fails due to a transient error (rate limiting, server
    /// errors, connection issues), cgx will retry up to this many times with
    /// exponential backoff.
    ///
    /// If not set, cgx honors the Cargo environment variable `CARGO_NET_RETRY`
    /// (integer). If not set, defaults to 2.
    #[arg(long, value_name = "N", env = "CGX_HTTP_RETRIES")]
    pub http_retries: Option<usize>,

    /// HTTP or SOCKS5 proxy URL for all HTTP requests.
    ///
    /// Routes all HTTP requests through the specified proxy, including git
    /// operations over HTTP/S. Supports http://, https://, and socks5:// URL
    /// schemes. For proxy authentication, embed credentials in the URL:
    /// `http://user:password@host:port`.
    ///
    /// If not set, cgx honors the Cargo environment variable `CARGO_HTTP_PROXY`,
    /// followed by standard proxy variables (`HTTPS_PROXY`, `https_proxy`,
    /// `http_proxy`). If none are set, no proxy is used.
    #[arg(long, value_name = "URL", env = "CGX_HTTP_PROXY")]
    pub http_proxy: Option<String>,

    /// Build the binary but do not execute it; print its path to stdout instead.
    ///
    /// Performs all normal operations (resolve, download, build) but instead of executing
    /// the binary at the end, prints its absolute path to stdout and exits with code 0.
    /// All diagnostic output goes to stderr, making stdout clean for scripting.
    ///
    /// Useful for testing, scripting (e.g., `tool=$(cgx --no-exec ripgrep)`), or obtaining
    /// a binary to run through debuggers/profilers.
    #[arg(long)]
    pub no_exec: bool,

    /// Force refresh of all cached data for this crate.
    ///
    /// When set, cgx will bypass all cache lookups and perform fresh resolution, download, and
    /// build operations. This also disables the fallback to stale cache entries on network errors,
    /// so cgx will fail if a network error occurs rather than using potentially outdated cached
    /// data.
    #[arg(long)]
    pub refresh: bool,

    /// Control use of pre-built binaries: never (always build from source), always (fail if no
    /// prebuilt binary found), or auto (use if available, fallback to build).
    ///
    /// When set to 'auto' (the default), cgx will attempt to download pre-built binaries from
    /// configured providers and fall back to building from source if none are found. When set to
    /// 'always', cgx will fail if no pre-built binary is found. When set to 'never', cgx will
    /// always build from source and never look for pre-built binaries.
    #[arg(long, value_name = "WHEN", value_parser = strum_value_parser!(UsePrebuiltBinaries))]
    pub prebuilt_binary: Option<UsePrebuiltBinaries>,

    /// Override the binary providers to check for pre-built binaries.
    ///
    /// Accepts a comma-separated list of providers to enable.
    #[arg(
        long,
        value_name = "SOURCES",
        value_delimiter = ',',
        value_parser = strum_value_parser!(BinaryProvider)
    )]
    pub prebuilt_binary_sources: Option<Vec<BinaryProvider>>,

    /// Disable checksum verification when downloading pre-built binaries.
    ///
    /// By default, cgx verifies downloaded binaries against checksums when available.
    /// This flag disables that verification, which may be useful for debugging or
    /// when checksums are known to be incorrect.
    #[arg(long)]
    pub prebuilt_binary_no_verify_checksums: bool,

    /// Disable signature verification when downloading pre-built binaries.
    ///
    /// By default, cgx verifies downloaded binaries against signatures when available.
    /// This flag disables that verification, which may be useful when minisign is not
    /// installed or for debugging purposes.
    #[arg(long)]
    pub prebuilt_binary_no_verify_signatures: bool,

    /// Output structured messages in the specified format.
    ///
    /// When set to "json", cgx will output machine-readable JSON messages to stdout describing
    /// its operations: cache lookups, resolution results, downloads, builds, and execution plans.
    /// This is useful for debugging, integration testing, and building tooling on top of cgx.
    ///
    /// All tracing/log output goes to stderr, keeping stdout clean for structured messages.
    /// Each message is a single line of JSON.
    ///
    /// NOTE: The format of the JSON messages is considered unstable and may change in future
    /// releases. This option is primarily intended for debugging and testing purposes.
    #[arg(long, value_name = "FMT")]
    pub message_format: Option<MessageFormat>,

    /// List the crate's executable targets (bins and examples) without building or executing.
    ///
    /// Performs resolve and download operations, then inspects the crate's Cargo.toml
    /// metadata to list all binary and example targets. Indicates which binary is the
    /// default (if specified via default-run field).
    ///
    /// This can be useful for discovering what targets are available in a crate, or in the
    /// (somewhat rare) case that a crate has multiple binaries and you need to know what they are
    /// called in order to select one with `--bin`.
    ///
    /// Returns an error if the crate contains no executable targets (is library-only).
    #[arg(long)]
    pub list_targets: bool,

    /// The crate to run (optionally with @VERSION suffix).
    ///
    /// This is optional when using `--path`, `--git`, `--github`, or `--gitlab`, as the crate
    /// name can be discovered from the source (if it contains exactly one crate).
    ///
    /// Special case: if this is "cargo" and no source flags are present, then the first
    /// element of `args` is treated as a cargo subcommand name, and "cargo-" is prepended
    /// to form the actual crate name (e.g., `cgx cargo deny` runs the crate `cargo-deny`).
    #[arg(value_name = "CRATE[@VERSION]",
        required_unless_present_any = ["version", "path", "git", "github", "gitlab"])]
    pub crate_spec: Option<String>,

    /// Arguments to pass to the executed tool.
    ///
    /// If `crate_spec` is "cargo" and no source flags are present, the first element is
    /// the cargo subcommand name, and remaining elements are passed to the tool.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl CliArgs {
    /// Parse CLI args from the current process's command line into a `CliArgs` struct.
    ///
    /// This simply spares a caller from having to have the [`clap::Parser`] trait in scope.
    ///
    /// Be advised that this uses `clap` which will exit the process if the args are invalid or
    /// after printing `--help` output.
    pub fn parse_from_cli_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let args = Self::strip_cargo_subcommand_arg(args);
        let (toolchain, filtered_args) = Self::extract_toolchain(&args);
        let (cgx_args, binary_args) = Self::split_at_crate_spec(filtered_args);

        let mut cli = Self::parse_from(cgx_args);
        cli.args = binary_args;
        cli.toolchain = toolchain;
        cli
    }

    /// Parse the CLI args from an arbitary iterator of strings, useful for constructing
    /// [`CLiArgs`] values for testing.
    #[cfg(test)]
    pub fn parse_from_test_args<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        // Prepend the name of the executable, as clap will be expecting.
        // No reason to make every test have to remember to do this
        let args = std::iter::once(std::ffi::OsString::from("cgx")).chain(args.into_iter().map(|s| s.into()));
        let args: Vec<String> = args.map(|s| s.to_string_lossy().to_string()).collect();
        let (toolchain, filtered_args) = Self::extract_toolchain(&args);
        let (cgx_args, binary_args) = Self::split_at_crate_spec(filtered_args);

        let mut cli = Self::parse_from(cgx_args);
        cli.args = binary_args;
        cli.toolchain = toolchain;
        cli
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

    /// Split command-line arguments into cgx arguments and binary arguments.
    ///
    /// This function separates arguments that should be parsed by cgx from arguments
    /// that should be passed through to the executed binary. The split point is the
    /// crate spec (the first positional argument).
    ///
    /// # Why Manual Splitting?
    ///
    /// While clap provides `trailing_var_arg = true` to capture trailing arguments,
    /// it still parses flags globally across the entire command line. This means
    /// `cgx eza --version` would have `--version` parsed as a cgx flag
    /// (since we have a `--version` flag) rather than being passed to eza.
    ///
    /// This behavior is tracked in [clap issue #1538]: "`TrailingVarArg` doesn't work
    /// without `--`". The recommended workaround is to require users to use `--`
    /// explicitly (e.g., `cgx eza -- --version`), but this is less ergonomic than
    /// the npx/uvx pattern we're trying to emulate.
    ///
    /// # Prior Art
    ///
    /// This pattern of manual argument splitting is common in wrapper tools:
    ///
    /// - `npx`: "When run via the npx binary, all flags and options must be set
    ///   prior to any positional arguments." Everything after the package name is
    ///   passed to the tool.
    ///   See: <https://docs.npmjs.com/cli/v8/commands/npx/>
    ///
    /// - `uvx`: Wrapper options like `--from`, `--with`, and `--python` must come
    ///   before the tool name. Everything after the tool name is passed as arguments
    ///   to that tool.
    ///   See: <https://docs.astral.sh/uv/guides/tools/>
    ///
    /// - `cargo run`: Uses the `--` delimiter approach (`cargo run -- args`), which is more
    ///   explicit but less convenient for repeated use.
    ///
    /// # Algorithm
    ///
    /// The function scans arguments left-to-right, tracking which arguments are flags
    /// and which are flag values. The first argument that is neither a flag nor a flag
    /// value is identified as the crate spec, and serves as the split point.
    ///
    /// For compatibility, if an explicit `--` delimiter is present, it is used as the
    /// split point instead.
    ///
    /// # Examples
    ///
    /// ```text
    /// cgx ripgrep --version
    ///  cgx args: ["cgx", "ripgrep"]
    ///  binary args: ["--version"]
    ///
    /// cgx --features foo ripgrep --color=always -i
    ///  cgx args: ["cgx", "--features", "foo", "ripgrep"]
    ///  binary args: ["--color=always", "-i"]
    ///
    /// cgx --path ./foo mycrate --help
    ///  cgx args: ["cgx", "--path", "./foo", "mycrate"]
    ///  binary args: ["--help"]
    ///
    /// cgx ripgrep -- --version
    ///  cgx args: ["cgx", "ripgrep"]
    ///  binary args: ["--version"]
    /// ```
    ///
    /// [clap issue #1538](https://github.com/clap-rs/clap/issues/1538)
    fn split_at_crate_spec<I, T>(args: I) -> (Vec<String>, Vec<String>)
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(|s| s.into()).collect();

        // Check for explicit -- separator (standard POSIX convention)
        // If present, everything before it goes to cgx, everything after goes to the binary
        if let Some(dash_dash_pos) = args.iter().position(|arg| arg == "--") {
            let cgx_args = args[..dash_dash_pos].to_vec();
            let binary_args = args[dash_dash_pos + 1..].to_vec();
            return (cgx_args, binary_args);
        }

        // Build flag lists dynamically from clap metadata, so that this function works reliably
        // as we add and modify CLI options.

        let cmd = CliArgs::command();
        let mut no_value_flags = HashSet::new();
        let mut value_taking_flags = HashSet::new();
        let mut short_value_taking_flags = HashSet::new();

        for arg in cmd.get_arguments() {
            // Skip positional arguments
            if arg.is_positional() {
                continue;
            }

            // Determine if this flag takes a value based on its action
            let takes_value = matches!(arg.get_action(), ArgAction::Set | ArgAction::Append);

            // Handle long flags
            if let Some(long) = arg.get_long() {
                let flag = format!("--{}", long);
                if takes_value {
                    value_taking_flags.insert(flag);
                } else {
                    no_value_flags.insert(flag);
                }
            }

            // Handle short flags
            if let Some(short) = arg.get_short() {
                if takes_value {
                    short_value_taking_flags.insert(short);
                }
                // Note: short no-value flags don't need tracking; we handle them via else clause
            }
        }

        let mut position = 1; // Start after binary name (args[0])

        while position < args.len() {
            let arg = &args[position];

            if let Some(flag) = arg.strip_prefix("--") {
                // Long flag
                if flag.contains('=') {
                    // --flag=value syntax, counts as one argument
                    position += 1;
                } else if no_value_flags.contains(arg.as_str()) {
                    position += 1;
                } else if value_taking_flags.contains(arg.as_str()) {
                    // Skip flag and its value (next argument)
                    position += 2;
                } else if arg == "--version" {
                    // Special case: --version can have 0 or 1 values
                    // If next arg exists and doesn't look like a flag, it's the version value
                    if position + 1 < args.len() && !args[position + 1].starts_with('-') {
                        position += 2;
                    } else {
                        position += 1;
                    }
                } else {
                    // Unknown long flag, conservatively assume it takes no value
                    position += 1;
                }
            } else if let Some(flag) = arg.strip_prefix('-') {
                // Short flag(s)
                if flag.is_empty() {
                    // Just "-", often used to indicate stdin - treat as positional argument
                    break;
                }

                let first_char = flag.chars().next().unwrap();

                if short_value_taking_flags.contains(&first_char) {
                    if flag.len() == 1 {
                        // -F foo (flag and value are separate arguments)
                        position += 2;
                    } else {
                        // -Ffoo (flag and value combined in one argument)
                        position += 1;
                    }
                } else if first_char == 'V' {
                    // -V is short for --version, same special handling
                    if flag.len() == 1 && position + 1 < args.len() && !args[position + 1].starts_with('-') {
                        // -V 14 (separate arguments)
                        position += 2;
                    } else {
                        // -V or -V14 (combined)
                        position += 1;
                    }
                } else {
                    // Assume no value (like -v, -q, or bundled flags like -vvv or -qv)
                    position += 1;
                }
            } else {
                // Not a flag - this is the crate spec (first positional argument)!
                break;
            }
        }

        // Split at the crate spec position
        if position < args.len() {
            // Found a crate spec: split after it
            let cgx_args = args[..=position].to_vec();
            let binary_args = args[position + 1..].to_vec();
            (cgx_args, binary_args)
        } else {
            // No crate spec found (e.g., `cgx --path ./foo` with no crate name specified)
            // All args go to cgx, none to binary
            (args, vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::{
        Result,
        builder::{BuildOptions, BuildTarget},
        config::Config,
        cratespec::{CrateSpec, Forge, RegistrySource},
        git::GitSelector,
    };

    #[test]
    fn verify_cli() {
        CliArgs::command().debug_assert();
    }

    mod cratespec {
        use super::*;
        fn parse_cratespec_from_args(args: &[&str]) -> Result<CrateSpec> {
            let cli = CliArgs::parse_from_test_args(args);
            let config = Config::default();
            CrateSpec::load(&config, &cli)
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
            let cr = parse_cratespec_from_args(&["--version", "14", "ripgrep"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: Some(ref v) }
                if name == "ripgrep" && v == &semver::VersionReq::parse("14").unwrap()
            );
        }

        #[test]
        fn test_crate_with_matching_versions() {
            let cr = parse_cratespec_from_args(&["--version", "14", "ripgrep@14"]).unwrap();
            assert_matches!(
                cr,
                CrateSpec::CratesIo { ref name, version: Some(ref v) }
                if name == "ripgrep" && v == &semver::VersionReq::parse("14").unwrap()
            );
        }

        #[test]
        fn test_crate_with_conflicting_versions() {
            let result = parse_cratespec_from_args(&["--version", "15", "ripgrep@14"]);
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
            let cli = CliArgs::parse_from_test_args(args);
            let config = Config::default();
            BuildOptions::load(&config, &cli.build_options, cli.verbose)
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
            let cli = CliArgs::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: true,
                offline: true,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.build_options, cli.verbose).unwrap();
            assert!(opts.locked);
            assert!(opts.offline);
        }

        #[test]
        fn test_config_locked_without_offline() {
            let cli = CliArgs::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: true,
                offline: false,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.build_options, cli.verbose).unwrap();
            assert!(opts.locked);
            assert!(!opts.offline);
        }

        #[test]
        fn test_config_offline_without_locked() {
            let cli = CliArgs::parse_from_test_args(["ripgrep"]);
            let config = Config {
                locked: false,
                offline: true,
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.build_options, cli.verbose).unwrap();
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
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain, Some("nightly".to_string()));
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_extract_toolchain_specific_version() {
            let args = vec!["cgx", "+1.70.0", "ripgrep"];
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain.as_deref(), Some("1.70.0"));
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_extract_toolchain_stable() {
            let args = vec!["cgx", "+stable", "ripgrep"];
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

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
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain, Some("nightly".to_string()));
            assert_eq!(
                filtered,
                vec!["cgx", "--git", "https://github.com/foo/bar", "mycrate"]
            );
        }

        #[test]
        fn test_no_toolchain() {
            let args = vec!["cgx", "ripgrep"];
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "ripgrep"]);
        }

        #[test]
        fn test_bare_plus() {
            let args = vec!["cgx", "+", "ripgrep"];
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "+", "ripgrep"]);
        }

        #[test]
        fn test_plus_in_middle_not_toolchain() {
            let args = vec!["cgx", "ripgrep", "+something"];
            let (toolchain, filtered) = CliArgs::extract_toolchain(args);

            assert_eq!(toolchain, None);
            assert_eq!(filtered, vec!["cgx", "ripgrep", "+something"]);
        }

        #[test]
        fn test_toolchain_with_version_flag() {
            let args = vec!["+nightly", "ripgrep", "--version", "14"];
            let cli = CliArgs::parse_from_test_args(args);

            assert_eq!(cli.toolchain, Some("nightly".to_string()));
            assert_eq!(cli.crate_spec, Some("ripgrep".to_string()));
        }

        #[test]
        fn test_toolchain_propagates_from_config_to_build_options() {
            // BuildOptions reads toolchain from Config, not from CLI args directly.
            // CLI toolchain override (+nightly) is applied by Config::load_from_dir().
            let args = vec!["ripgrep"];
            let cli = CliArgs::parse_from_test_args(args);

            let config = Config {
                toolchain: Some("nightly".to_string()),
                ..Default::default()
            };
            let opts = BuildOptions::load(&config, &cli.build_options, cli.verbose).unwrap();
            assert_eq!(opts.toolchain, Some("nightly".to_string()));
        }

        #[test]
        fn test_no_toolchain_in_build_options() {
            let args = vec!["ripgrep"];
            let cli = CliArgs::parse_from_test_args(args);

            let config = Config::default();
            let opts = BuildOptions::load(&config, &cli.build_options, cli.verbose).unwrap();
            assert_eq!(opts.toolchain, None);
        }
    }

    mod config_overrides {
        use super::*;

        #[test]
        fn test_config_overrides_can_combine() {
            // Verify that system-config-dir, app-dir, and user-config-dir work together.
            // This tests our design decision that these three options are compatible,
            // not just clap functionality.
            let result = CliArgs::try_parse_from([
                "cgx",
                "--system-config-dir",
                "/tmp/system",
                "--app-dir",
                "/tmp/app",
                "--user-config-dir",
                "/tmp/user",
                "ripgrep",
            ]);
            assert!(result.is_ok());
            let cli = result.unwrap();
            assert_eq!(cli.system_config_dir, Some(PathBuf::from("/tmp/system")));
            assert_eq!(cli.app_dir, Some(PathBuf::from("/tmp/app")));
            assert_eq!(cli.user_config_dir, Some(PathBuf::from("/tmp/user")));
        }
    }

    mod argument_splitting {
        use super::*;

        #[test]
        fn test_simple_split_at_crate_spec() {
            let args = vec!["cgx", "ripgrep", "--version"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "ripgrep"]);
            assert_eq!(binary_args, vec!["--version"]);
        }

        #[test]
        fn test_split_with_cgx_flags_before_crate() {
            let args = vec!["cgx", "--features", "foo", "ripgrep", "--color=always", "-i"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--features", "foo", "ripgrep"]);
            assert_eq!(binary_args, vec!["--color=always", "-i"]);
        }

        #[test]
        fn test_split_with_path_flag() {
            let args = vec!["cgx", "--path", "./foo", "mycrate", "--help"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--path", "./foo", "mycrate"]);
            assert_eq!(binary_args, vec!["--help"]);
        }

        #[test]
        fn test_no_crate_spec_no_binary_args() {
            let args = vec!["cgx", "--path", "./foo", "--version"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--path", "./foo", "--version"]);
            assert_eq!(binary_args, Vec::<String>::new());
        }

        #[test]
        fn test_explicit_dash_dash_separator() {
            let args = vec!["cgx", "ripgrep", "--", "--version"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "ripgrep"]);
            assert_eq!(binary_args, vec!["--version"]);
        }

        #[test]
        fn test_dash_dash_before_crate_spec() {
            let args = vec!["cgx", "--path", "./foo", "--", "--help"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--path", "./foo"]);
            assert_eq!(binary_args, vec!["--help"]);
        }

        #[test]
        fn test_short_flags() {
            let args = vec!["cgx", "-j", "4", "ripgrep", "-i"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "-j", "4", "ripgrep"]);
            assert_eq!(binary_args, vec!["-i"]);
        }

        #[test]
        fn test_combined_short_flag_with_value() {
            let args = vec!["cgx", "-F", "foo,bar", "ripgrep", "-A", "3"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "-F", "foo,bar", "ripgrep"]);
            assert_eq!(binary_args, vec!["-A", "3"]);
        }

        #[test]
        fn test_bundled_short_flags() {
            let args = vec!["cgx", "-vvv", "ripgrep", "-i"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "-vvv", "ripgrep"]);
            assert_eq!(binary_args, vec!["-i"]);
        }

        #[test]
        fn test_equals_syntax() {
            let args = vec!["cgx", "--features=foo,bar", "ripgrep", "--color=always"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--features=foo,bar", "ripgrep"]);
            assert_eq!(binary_args, vec!["--color=always"]);
        }

        #[test]
        fn test_version_with_value() {
            let args = vec!["cgx", "ripgrep", "--version", "14"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "ripgrep"]);
            assert_eq!(binary_args, vec!["--version", "14"]);
        }

        #[test]
        fn test_version_flag_before_crate_with_value() {
            let args = vec!["cgx", "--version", "14", "ripgrep"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--version", "14", "ripgrep"]);
            assert_eq!(binary_args, Vec::<String>::new());
        }

        #[test]
        fn test_version_flag_before_crate_without_value() {
            let args = vec!["cgx", "--version", "ripgrep"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "--version", "ripgrep"]);
            assert_eq!(binary_args, Vec::<String>::new());
        }

        #[test]
        fn test_crate_with_version_suffix() {
            let args = vec!["cgx", "eza@=0.23.1", "--version"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "eza@=0.23.1"]);
            assert_eq!(binary_args, vec!["--version"]);
        }

        #[test]
        fn test_cargo_subcommand() {
            let args = vec!["cgx", "cargo", "deny", "--help"];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "cargo"]);
            assert_eq!(binary_args, vec!["deny", "--help"]);
        }

        #[test]
        fn test_multiple_binary_flags() {
            let args = vec![
                "cgx",
                "ripgrep",
                "--color=always",
                "-i",
                "--no-heading",
                "pattern",
                "file.txt",
            ];
            let (cgx_args, binary_args) = CliArgs::split_at_crate_spec(args);

            assert_eq!(cgx_args, vec!["cgx", "ripgrep"]);
            assert_eq!(
                binary_args,
                vec!["--color=always", "-i", "--no-heading", "pattern", "file.txt"]
            );
        }
    }

    mod http_args {
        use super::*;

        #[test]
        fn test_http_timeout_cli_arg() {
            let cli = CliArgs::parse_from_test_args(["--http-timeout", "2m", "test-crate"]);
            assert_eq!(cli.http_timeout, Some("2m".to_string()));
        }

        #[test]
        fn test_http_retries_cli_arg() {
            let cli = CliArgs::parse_from_test_args(["--http-retries", "5", "test-crate"]);
            assert_eq!(cli.http_retries, Some(5));
        }

        #[test]
        fn test_http_proxy_cli_arg() {
            let cli =
                CliArgs::parse_from_test_args(["--http-proxy", "socks5://localhost:1080", "test-crate"]);
            assert_eq!(cli.http_proxy, Some("socks5://localhost:1080".to_string()));
        }

        #[test]
        fn test_http_args_default_none() {
            let cli = CliArgs::parse_from_test_args(["test-crate"]);
            assert_eq!(cli.http_timeout, None);
            assert_eq!(cli.http_retries, None);
            assert_eq!(cli.http_proxy, None);
        }

        #[test]
        fn test_http_args_after_crate_spec_are_binary_args() {
            let cli = CliArgs::parse_from_test_args(["test-crate", "--http-timeout", "5s"]);
            assert_eq!(cli.http_timeout, None);
            assert_eq!(cli.args, vec!["--http-timeout", "5s"]);
        }
    }

    mod strip_cargo_subcommand_arg {
        use super::*;

        #[test]
        fn test_leaves_normal_invocation_unchanged() {
            let args = vec!["cgx", "ripgrep", "--help"];
            let result = CliArgs::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_leaves_cargo_without_cgx_unchanged() {
            let args = vec!["cargo-cgx", "ripgrep", "--help"];
            let result = CliArgs::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_empty_args() {
            let args: Vec<String> = vec![];
            let result = CliArgs::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }

        #[test]
        fn test_single_arg() {
            let args = vec!["cargo-cgx"];
            let result = CliArgs::strip_cargo_subcommand_arg(args.clone());
            assert_eq!(result, args);
        }
    }
}
