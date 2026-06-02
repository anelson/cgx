pub mod logging;
mod reporter;

use cgx_core::{
    Target,
    builder::BuildOptions,
    cli::{CliArgs, MessageFormat},
    config::Config,
    cratespec::CrateSpec,
    error,
};
use std::ffi::OsString;

// Re-export key types from cgx-core for convenience
pub use cgx_core::{
    cli,
    error::{Error, Result},
};

/// **INTERNAL - DO NOT USE IN PRODUCTION CODE**
///
/// Internal messaging types exposed solely for integration testing. This is NOT a stable interface
/// and WILL break without warning, outside of semver guarantees. If you need a stable messages
/// interface, please open an issue with your use case for discussion.
#[doc(hidden)]
pub use cgx_core::messages;

/// Re-export of the snafu [`snafu::Report`] type so that callers can refer to this type without
/// taking an explicit snafu dep
pub use snafu::Report as SnafuReport;

/// Main entry point for the `cgx` engine.
///
/// Meant to be called from `main.rs` or other frontends.
#[snafu::report]
pub fn cgx_main() -> Result<()> {
    let args = CliArgs::parse_from_cli_args();

    // Initialize tracing early, before any other operations
    logging::init(&args);

    if let Some(version_arg) = &args.version {
        if version_arg.is_empty() {
            let version = env!("CARGO_PKG_VERSION");

            match (
                option_env!("VERGEN_GIT_SHA"),
                option_env!("VERGEN_GIT_COMMIT_DATE"),
            ) {
                (Some(sha), Some(date))
                    if sha != "VERGEN_IDEMPOTENT_OUTPUT" && date != "VERGEN_IDEMPOTENT_OUTPUT" =>
                {
                    eprintln!("cgx {} ({} {})", version, sha, date);
                }
                _ => {
                    eprintln!("cgx {}", version);
                }
            }
            return Ok(());
        }
    }

    let config = Config::load(&args)?;

    // Apply log level from config file if appropriate
    logging::apply_config(&config, &args);

    let crate_spec = CrateSpec::load(&config, &args)?;
    let build_options = BuildOptions::load(&config, &args.build_options, args.verbose)?;

    // Spawn a separate thread that will handle messages from the cgx core and report them to the user
    // in the appropriate way.
    let json_mode = matches!(args.message_format, Some(MessageFormat::Json));
    let reporter_thread = reporter::ReporterThread::spawn(json_mode);

    // Decode and prepare the command that the user wants to execute based on the args and env.
    // This is where the heavy lifting happens.
    let result = prepare_command(&reporter_thread, config, crate_spec, build_options, args);

    // Success or failure, there will be no more messages produced after this point, so join the
    // reporter thread to make sure we've processed any that were emitted before we proceed
    reporter_thread.join();

    match result? {
        Command::NoExec { bin_path } => {
            // Print path to stdout for scripting (e.g., binary=$(cgx --no-exec tool))
            println!("{}", bin_path.display());
            return Ok(());
        }
        Command::Execute {
            bin_path,
            binary_args,
        } => {
            // Run the binary - this function never returns on success
            // It either replaces the process (Unix) or exits with the child's code (Windows)
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
            // Print bins with default indication
            for bin in bins {
                println!("bin: {}", bin.name);
            }

            // Print examples
            for example in examples {
                println!("example: {}", example.name);
            }

            return Ok(());
        }
    }
}

/// Possible successful results of [`prepare_command`]
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
}

/// Internal implementation that does the actual work of decoding the args, determinine that
/// command the user wants performed, and preparing the environment as needed.
fn prepare_command(
    reporter_thread: &reporter::ReporterThread,
    config: Config,
    crate_spec: CrateSpec,
    build_options: BuildOptions,
    args: CliArgs,
) -> Result<Command> {
    let cgx = cgx_core::Cgx::new(config, reporter_thread.message_reporter().clone())?;

    if args.list_targets {
        let (crate_name, default, bins, examples) = cgx.list_targets(&crate_spec, &build_options)?;

        return Ok(Command::ListTargets {
            crate_name,
            default,
            bins,
            examples,
        });
    }

    let bin_path = cgx.crate_to_bin(&crate_spec, &build_options)?;

    // Extract arguments to pass to the binary
    let binary_args = CrateSpec::get_binary_args(&args);

    // Report the execution plan
    reporter_thread
        .message_reporter()
        .report(|| messages::RunnerMessage::execution_plan(&bin_path, &binary_args, args.no_exec));

    if args.no_exec {
        Ok(Command::NoExec { bin_path })
    } else {
        Ok(Command::Execute {
            bin_path,
            binary_args,
        })
    }
}
