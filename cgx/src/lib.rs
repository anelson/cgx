pub mod logging;
mod reporter;

use std::ffi::OsString;

/// **INTERNAL - DO NOT USE IN PRODUCTION CODE**
///
/// Internal messaging types exposed solely for integration testing. This is NOT a stable interface
/// and WILL break without warning, outside of semver guarantees. If you need a stable messages
/// interface, please open an issue with your use case for discussion.
#[doc(hidden)]
pub use cgx_core::messages;
use cgx_core::{
    Cgx, Target,
    cli::{Cli, CrateArgs, MessageFormat, PrefetchAll},
    config::Config,
    error,
};
// Re-export key types from cgx-core for convenience
pub use cgx_core::{
    cli,
    error::{Error, Result},
};
/// Re-export of the snafu [`snafu::Report`] type so that callers can refer to this type without
/// taking an explicit snafu dep
pub use snafu::Report as SnafuReport;

/// Main entry point for the `cgx` engine.
///
/// Meant to be called from `main.rs` or other frontends.
#[snafu::report]
pub fn cgx_main() -> Result<()> {
    let cli = Cli::parse_from_cli_args(cgx_version_string());

    // Initialize tracing early, before any other operations
    logging::init(cli.verbosity());

    let config = Config::load(&cli.to_config_overrides())?;

    // Apply log level from config file if appropriate
    logging::apply_config(&config);

    // Spawn a separate thread that will handle messages from the cgx core and report them to the
    // user in the appropriate way.
    let json_mode = matches!(cli.message_format(), Some(MessageFormat::Json));
    let reporter_thread = reporter::ReporterThread::spawn(json_mode);

    // Decode and prepare the command the user wants to run. This is where the heavy lifting
    // happens; the result is a `Command` describing what to print or execute.
    let result = Command::try_from_cli(&cli, &reporter_thread, &config);

    // Success or failure, there will be no more messages produced after this point, so join the
    // reporter thread to make sure we've processed any that were emitted before we proceed.
    reporter_thread.join();

    let command = result?;
    command.execute()
}

/// Build the version string shown by `-V`/`--version`.
///
/// clap prepends the command name (`cgx`), so this returns just the version, including the git sha
/// and commit date when they were available at build time (via `vergen`).
fn cgx_version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match (
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_DATE"),
    ) {
        (Some(sha), Some(date))
            if sha != "VERGEN_IDEMPOTENT_OUTPUT" && date != "VERGEN_IDEMPOTENT_OUTPUT" =>
        {
            format!("{version} ({sha} {date})")
        }
        _ => version.to_string(),
    }
}

/// Possible successful results of preparing a command for execution.
enum Command {
    /// Binary is ready to go but user requested `--no-exec` so just print the path we'd execute
    NoExec { bin_path: std::path::PathBuf },
    /// Binary has been prepared and we're ready to execute it with a given args
    Execute {
        bin_path: std::path::PathBuf,
        binary_args: Vec<OsString>,
    },
    /// User just asked to list available targets of the crate
    ListTargets {
        crate_name: String,
        default: Option<Target>,
        bins: Vec<Target>,
        examples: Vec<Target>,
    },
    /// User asked to list configured tools and aliases
    ListTools { toml: String },
    /// Binary preparation completed without execution
    Prefetched,
}

impl Command {
    /// Execute the prepared command: run the binary, or print the resolved path, the crate's
    /// targets, or the configured-tools TOML.
    fn execute(self) -> Result<()> {
        match self {
            Command::NoExec { bin_path } => {
                // Print path to stdout for scripting (e.g., binary=$(cgx --no-exec tool))
                println!("{}", bin_path.display());
                Ok(())
            }
            Command::Execute {
                bin_path,
                binary_args,
            } => {
                // Run the binary - never returns on success. It either replaces the process
                // (Unix) or exits with the child's code (Windows).
                cgx_core::runner::run(&bin_path, &binary_args)
            }
            Command::ListTargets {
                crate_name,
                default,
                bins,
                examples,
            } => {
                // Ensure there are executable targets
                if bins.is_empty() && examples.is_empty() {
                    return error::NoPackageBinariesSnafu { krate: crate_name }.fail();
                }

                println!(
                    "default_run: {}",
                    default
                        .map(|target| target.name)
                        .as_deref()
                        .unwrap_or("<not set>")
                );
                for bin in bins {
                    println!("bin: {}", bin.name);
                }
                for example in examples {
                    println!("example: {}", example.name);
                }

                Ok(())
            }
            Command::ListTools { toml } => {
                print!("{}", toml);
                Ok(())
            }
            Command::Prefetched => Ok(()),
        }
    }

    /// Decode the parsed CLI into the command to run, performing all resolution/build work and
    /// emitting progress messages, but without executing or printing anything yet.
    fn try_from_cli(
        cli: &Cli,
        reporter_thread: &reporter::ReporterThread,
        config: &Config,
    ) -> Result<Command> {
        let cgx = Cgx::new(config.clone(), reporter_thread.message_reporter().clone())?;

        match cli {
            Cli::ListTargets(args) => Self::list_targets(cgx, args),
            Cli::ListTools(_) => Self::list_tools(cgx),
            Cli::Prefetch(args) => Self::prefetch(cgx, args),
            Cli::PrefetchAll(prefetch_all) => Self::prefetch_all(cgx, prefetch_all),
            Cli::NoExec(args) => Self::no_exec(cgx, reporter_thread, args),
            Cli::Run { args, tool_args } => Self::run(cgx, reporter_thread, args, tool_args.clone()),
        }
    }

    fn list_tools(cgx: Cgx) -> Result<Command> {
        let toml = cgx.list_configured_tools()?;
        Ok(Command::ListTools { toml })
    }

    fn prefetch_all(cgx: Cgx, prefetch_all: &PrefetchAll) -> Result<Command> {
        cgx.prefetch_all(&prefetch_all.to_build_overrides())?;
        Ok(Command::Prefetched)
    }

    /// Prepare and execute a crate, forwarding `tool_args` to the executed binary.
    fn run(
        cgx: Cgx,
        reporter_thread: &reporter::ReporterThread,
        args: &CrateArgs,
        tool_args: Vec<OsString>,
    ) -> Result<Command> {
        let bin_path = cgx.prepare_bin_crate(&args.crate_request()?, &args.to_build_overrides())?;

        reporter_thread
            .message_reporter()
            .report(|| messages::RunnerMessage::execution_plan(&bin_path, &tool_args, false));

        Ok(Command::Execute {
            bin_path,
            binary_args: tool_args,
        })
    }

    /// Prepare a crate without executing it, printing the resolved binary path to stdout.
    fn no_exec(cgx: Cgx, reporter_thread: &reporter::ReporterThread, args: &CrateArgs) -> Result<Command> {
        let bin_path = cgx.prepare_bin_crate(&args.crate_request()?, &args.to_build_overrides())?;

        let no_args: Vec<OsString> = Vec::new();
        reporter_thread
            .message_reporter()
            .report(|| messages::RunnerMessage::execution_plan(&bin_path, &no_args, true));

        Ok(Command::NoExec { bin_path })
    }

    /// Prepare a crate without executing it or printing its path.
    fn prefetch(cgx: Cgx, args: &CrateArgs) -> Result<Command> {
        let request = args.crate_request()?;

        // Prefer the raw CLI spec (e.g. `ripgrep@1.0`) for the label; otherwise the resolved request
        // name; otherwise a source-only invocation with no name to show.
        let label = args
            .crate_spec
            .clone()
            .or_else(|| request.name.clone())
            .unwrap_or_else(|| "<source>".to_string());

        cgx.prefetch(&label, &request, &args.to_build_overrides())?;

        Ok(Command::Prefetched)
    }

    /// Resolve a crate and list its runnable targets without building or executing.
    fn list_targets(cgx: Cgx, args: &CrateArgs) -> Result<Command> {
        let (crate_name, default, bins, examples) =
            cgx.list_targets(&args.crate_request()?, &args.to_build_overrides())?;

        Ok(Command::ListTargets {
            crate_name,
            default,
            bins,
            examples,
        })
    }
}
