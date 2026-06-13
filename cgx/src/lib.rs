pub mod logging;
mod reporter;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
};

/// **INTERNAL - DO NOT USE IN PRODUCTION CODE**
///
/// Internal messaging types exposed solely for integration testing. This is NOT a stable interface
/// and WILL break without warning, outside of semver guarantees. If you need a stable messages
/// interface, please open an issue with your use case for discussion.
#[doc(hidden)]
pub use cgx_core::messages;
use cgx_core::{
    Target,
    builder::BuildOptions,
    cli::{Cli, CrateArgs, MessageFormat, PrefetchAll},
    config::Config,
    cratespec::{CrateRequest, CrateSpec},
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

    // Decode and prepare the command that the user wants to execute. This is where the heavy
    // lifting happens.
    let result = match &cli {
        Cli::ListTargets(args) => prepare_list_targets(&reporter_thread, &config, args),
        Cli::ListTools(_) => prepare_list_tools(&reporter_thread, &config),
        Cli::Prefetch(args) => prepare_prefetch(&reporter_thread, &config, args),
        Cli::PrefetchAll(prefetch_all) => prepare_prefetch_all(&reporter_thread, &config, prefetch_all),
        Cli::NoExec(args) => prepare_no_exec(&reporter_thread, &config, args),
        Cli::Run { args, tool_args } => prepare_run(&reporter_thread, &config, args, tool_args.clone()),
    };

    // Success or failure, there will be no more messages produced after this point, so join the
    // reporter thread to make sure we've processed any that were emitted before we proceed
    reporter_thread.join();

    match result? {
        Command::NoExec { bin_path } => {
            // Print path to stdout for scripting (e.g., binary=$(cgx --no-exec tool))
            println!("{}", bin_path.display());
            Ok(())
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

            Ok(())
        }
        Command::ListTools { toml } => {
            print!("{}", toml);
            Ok(())
        }
        Command::Prefetched => Ok(()),
    }
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

fn prepare_list_tools(reporter_thread: &reporter::ReporterThread, config: &Config) -> Result<Command> {
    let reporter = reporter_thread.message_reporter();

    let mut tools: Vec<_> = config.tools.iter().collect();
    tools.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, tool_config) in tools {
        reporter.report(|| messages::RunnerMessage::list_tool(name, tool_config));
    }

    let mut aliases: Vec<_> = config.aliases.iter().collect();
    aliases.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, target) in aliases {
        reporter.report(|| messages::RunnerMessage::list_alias(name, target));
    }

    Ok(Command::ListTools {
        toml: config.tools_toml()?,
    })
}

fn prepare_prefetch_all(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    prefetch_all: &PrefetchAll,
) -> Result<Command> {
    let cgx = cgx_core::Cgx::new(config.clone(), reporter_thread.message_reporter().clone())?;
    let tools = configured_tools(config);
    let reporter = reporter_thread.message_reporter();
    let mut failures = Vec::new();

    for tool in tools {
        reporter.report(|| messages::RunnerMessage::prefetch_all_started(&tool.name, &tool.aliases));

        let result = prefetch_tool(&cgx, config, prefetch_all, &tool);
        match result {
            Ok(bin_path) => {
                reporter.report(|| {
                    messages::RunnerMessage::prefetch_all_completed(&tool.name, &tool.aliases, &bin_path)
                });
            }
            Err(err) => {
                reporter
                    .report(|| messages::RunnerMessage::prefetch_all_failed(&tool.name, &tool.aliases, &err));
                failures.push(format!("{}: {}", tool.name, err));
            }
        }
    }

    if failures.is_empty() {
        Ok(Command::Prefetched)
    } else {
        error::PrefetchAllFailedSnafu { failures }.fail()
    }
}

/// One distinct tool referenced by the configuration: the alias-resolved tool name plus the
/// configured names that map to it. `--prefetch-all` prefetches each of these exactly once.
#[derive(Debug, PartialEq, Eq)]
struct ConfiguredTool {
    /// The alias-resolved tool name, reported as `tool` in the prefetch-all messages.
    name: String,
    /// The other configured names that resolve to this tool, sorted, excluding the name itself.
    aliases: Vec<String>,
    /// The configured name to load the crate spec with. Any grouped name loads the same crate,
    /// and using a configured name (rather than the resolved one) keeps [`CrateSpec::load`]'s
    /// single-step alias resolution identical to a direct `cgx <name>` invocation.
    request: String,
}

/// Group every configured tool and alias by the tool it resolves to, applying the same
/// single-step alias resolution as [`CrateSpec::load`].
fn configured_tools(config: &Config) -> Vec<ConfiguredTool> {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in config.tools.keys().chain(config.aliases.keys()) {
        let resolved = config.aliases.get(name).unwrap_or(name);
        groups.entry(resolved.clone()).or_default().insert(name.clone());
    }

    groups
        .into_iter()
        .map(|(name, members)| {
            let request = members.first().cloned().unwrap();
            let aliases = members.into_iter().filter(|member| *member != name).collect();
            ConfiguredTool {
                name,
                aliases,
                request,
            }
        })
        .collect()
}

fn prefetch_tool(
    cgx: &cgx_core::Cgx,
    config: &Config,
    prefetch_all: &PrefetchAll,
    configured_tool: &ConfiguredTool,
) -> Result<std::path::PathBuf> {
    let crate_spec = CrateSpec::load(
        config,
        &CrateRequest::for_configured_tool(&configured_tool.request),
    )?;
    let build_options =
        BuildOptions::load_for_crate(config, &prefetch_all.to_build_overrides(), &crate_spec)?;

    cgx.crate_to_bin(&crate_spec, &build_options)
}

/// Resolve a single set of crate arguments into a [`cgx_core::Cgx`] engine plus its crate spec and
/// build options, for use by multiple commands that need a `Cgx` instance to operate on a specific
/// crate.
fn prepare_engine(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    args: &CrateArgs,
) -> Result<(cgx_core::Cgx, CrateSpec, BuildOptions)> {
    let crate_spec = CrateSpec::load(config, &args.crate_request()?)?;
    let build_options = BuildOptions::load_for_crate(config, &args.to_build_overrides(), &crate_spec)?;
    let cgx = cgx_core::Cgx::new(config.clone(), reporter_thread.message_reporter().clone())?;

    Ok((cgx, crate_spec, build_options))
}

/// Prepare and execute a crate, forwarding `tool_args` to the executed binary.
fn prepare_run(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    args: &CrateArgs,
    tool_args: Vec<OsString>,
) -> Result<Command> {
    let (cgx, crate_spec, build_options) = prepare_engine(reporter_thread, config, args)?;
    let bin_path = cgx.crate_to_bin(&crate_spec, &build_options)?;

    reporter_thread
        .message_reporter()
        .report(|| messages::RunnerMessage::execution_plan(&bin_path, &tool_args, false));

    Ok(Command::Execute {
        bin_path,
        binary_args: tool_args,
    })
}

/// Prepare a crate without executing it, printing the resolved binary path to stdout.
fn prepare_no_exec(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    args: &CrateArgs,
) -> Result<Command> {
    let (cgx, crate_spec, build_options) = prepare_engine(reporter_thread, config, args)?;
    let bin_path = cgx.crate_to_bin(&crate_spec, &build_options)?;

    let no_args: Vec<OsString> = Vec::new();
    reporter_thread
        .message_reporter()
        .report(|| messages::RunnerMessage::execution_plan(&bin_path, &no_args, true));

    Ok(Command::NoExec { bin_path })
}

/// Prepare a crate without executing it or printing its path.
fn prepare_prefetch(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    args: &CrateArgs,
) -> Result<Command> {
    let (cgx, crate_spec, build_options) = prepare_engine(reporter_thread, config, args)?;

    let label = args
        .crate_spec
        .as_deref()
        .or_else(|| crate_spec.configured_tool_name())
        .unwrap_or("<source>")
        .to_string();

    reporter_thread
        .message_reporter()
        .report(|| messages::RunnerMessage::prefetch_started(&label));

    let bin_path = cgx.crate_to_bin(&crate_spec, &build_options)?;

    reporter_thread
        .message_reporter()
        .report(|| messages::RunnerMessage::prefetch_completed(&label, &bin_path));

    Ok(Command::Prefetched)
}

/// Resolve a crate and list its runnable targets without building or executing.
fn prepare_list_targets(
    reporter_thread: &reporter::ReporterThread,
    config: &Config,
    args: &CrateArgs,
) -> Result<Command> {
    let (cgx, crate_spec, build_options) = prepare_engine(reporter_thread, config, args)?;
    let (crate_name, default, bins, examples) = cgx.list_targets(&crate_spec, &build_options)?;

    Ok(Command::ListTargets {
        crate_name,
        default,
        bins,
        examples,
    })
}

#[cfg(test)]
mod tests {
    use cgx_core::config::ToolConfig;

    use super::*;

    fn config_with(tools: &[&str], aliases: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        for &tool in tools {
            config
                .tools
                .insert(tool.to_string(), ToolConfig::Version("*".to_string()));
        }
        for &(name, target) in aliases {
            config.aliases.insert(name.to_string(), target.to_string());
        }
        config
    }

    #[test]
    fn tool_with_alias_yields_single_entry() {
        let config = config_with(&["eza"], &[("e", "eza")]);
        assert_eq!(
            configured_tools(&config),
            [ConfiguredTool {
                name: "eza".to_string(),
                aliases: vec!["e".to_string()],
                request: "e".to_string(),
            }]
        );
    }

    #[test]
    fn alias_to_unconfigured_crate_yields_its_target_tool() {
        let config = config_with(&[], &[("x", "ripgrep")]);
        assert_eq!(
            configured_tools(&config),
            [ConfiguredTool {
                name: "ripgrep".to_string(),
                aliases: vec!["x".to_string()],
                request: "x".to_string(),
            }]
        );
    }

    #[test]
    fn multiple_aliases_group_under_one_tool() {
        let config = config_with(&["eza"], &[("e", "eza"), ("ez", "eza")]);
        assert_eq!(
            configured_tools(&config),
            [ConfiguredTool {
                name: "eza".to_string(),
                aliases: vec!["e".to_string(), "ez".to_string()],
                request: "e".to_string(),
            }]
        );
    }

    #[test]
    fn tools_are_deterministically_ordered() {
        let config = config_with(&["zoxide", "eza"], &[("a", "zoxide")]);
        let names: Vec<_> = configured_tools(&config)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, ["eza", "zoxide"]);
    }

    #[test]
    fn independent_tools_remain_separate() {
        let config = config_with(&["eza", "ripgrep"], &[]);
        assert_eq!(
            configured_tools(&config),
            [
                ConfiguredTool {
                    name: "eza".to_string(),
                    aliases: Vec::new(),
                    request: "eza".to_string(),
                },
                ConfiguredTool {
                    name: "ripgrep".to_string(),
                    aliases: Vec::new(),
                    request: "ripgrep".to_string(),
                },
            ]
        );
    }
}
