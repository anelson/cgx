use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::Message;
use crate::config::ToolConfig;

/// Messages related to binary execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerMessage {
    /// A conventional binary invocation, or path-only output under `--no-exec`.
    BinaryExecutionPlan {
        /// Path to the resolved binary.
        binary_path: PathBuf,
        /// Arguments forwarded to the binary, lossily converted to UTF-8 for serialization.
        args: Vec<String>,
        /// True when `--no-exec` was given, so the plan is reported without being executed.
        no_exec: bool,
    },
    /// A Cargo subcommand invocation, with its bare name separate from its arguments.
    CargoSubcommandExecutionPlan {
        /// Path to the resolved plugin binary.
        binary_path: PathBuf,
        /// Bare subcommand name, lossily converted to UTF-8 for serialization.
        subcommand: String,
        /// User-supplied arguments after the subcommand, lossily converted to UTF-8.
        args: Vec<String>,
    },
    /// One configured `[tools]` entry, reported by `--list-tools`.
    ListTool {
        /// The configured tool (crate) name.
        name: String,
        /// The tool's merged configuration.
        config: ToolConfig,
    },
    /// One configured `[aliases]` entry, reported by `--list-tools`.
    ListAlias {
        /// The alias name.
        name: String,
        /// The tool name the alias resolves to.
        target: String,
    },
    /// `--prefetch` started preparing a crate.
    PrefetchStarted {
        /// The crate spec as requested on the command line (a crate name, `name@version`, or an
        /// alias, unresolved), or the configured tool name or a `<source>` placeholder when the
        /// crate is discovered from a source such as `--git` or `--path`.
        crate_spec: String,
    },
    /// `--prefetch` finished preparing a crate.
    PrefetchCompleted {
        /// The crate spec as requested on the command line (see [`Self::PrefetchStarted`]).
        crate_spec: String,
        /// Path to the prepared binary.
        binary_path: PathBuf,
    },
    /// `--prefetch-all` started prefetching one configured tool.
    PrefetchAllStarted {
        /// The name of the crate in the `[tools]` config section.
        tool: String,
        /// List of aliases (if any) to the tool, in the `[aliases]` config section, that also
        /// resolve to this tool.
        aliases: Vec<String>,
    },
    /// `--prefetch-all` finished prefetching one configured tool.
    PrefetchAllCompleted {
        /// The name of the crate in the `[tools]` config section.
        tool: String,
        /// List of aliases (if any) to the tool, in the `[aliases]` config section, that also
        /// resolve to this tool.
        aliases: Vec<String>,
        /// Path to the prepared binary.
        binary_path: PathBuf,
    },
    /// `--prefetch-all` failed to prefetch one configured tool; the run continues with the
    /// remaining tools and reports an overall failure at the end.
    PrefetchAllFailed {
        /// The name of the crate in the `[tools]` config section.
        tool: String,
        /// List of aliases (if any) to the tool, in the `[aliases]` config section, that also
        /// resolve to this tool.
        aliases: Vec<String>,
        /// The rendered error that caused the failure.
        error: String,
    },
}

impl RunnerMessage {
    /// Describe conventional binary execution or path-only output under `--no-exec`.
    pub fn binary_execution_plan(binary_path: &Path, args: &[OsString], no_exec: bool) -> Self {
        Self::BinaryExecutionPlan {
            binary_path: binary_path.to_path_buf(),
            args: args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            no_exec,
        }
    }

    /// Describe Cargo subcommand execution of a specific subcommand
    pub fn cargo_subcommand_execution_plan(
        binary_path: &Path,
        subcommand: &OsStr,
        args: &[OsString],
    ) -> Self {
        Self::CargoSubcommandExecutionPlan {
            binary_path: binary_path.to_path_buf(),
            subcommand: subcommand.to_string_lossy().into_owned(),
            args: args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        }
    }

    pub fn list_tool(name: &str, config: &ToolConfig) -> Self {
        Self::ListTool {
            name: name.to_string(),
            config: config.clone(),
        }
    }

    pub fn list_alias(name: &str, target: &str) -> Self {
        Self::ListAlias {
            name: name.to_string(),
            target: target.to_string(),
        }
    }

    pub fn prefetch_started(crate_spec: &str) -> Self {
        Self::PrefetchStarted {
            crate_spec: crate_spec.to_string(),
        }
    }

    pub fn prefetch_completed(crate_spec: &str, binary_path: &Path) -> Self {
        Self::PrefetchCompleted {
            crate_spec: crate_spec.to_string(),
            binary_path: binary_path.to_path_buf(),
        }
    }

    pub fn prefetch_all_started(tool: &str, aliases: &[String]) -> Self {
        Self::PrefetchAllStarted {
            tool: tool.to_string(),
            aliases: aliases.to_vec(),
        }
    }

    pub fn prefetch_all_completed(tool: &str, aliases: &[String], binary_path: &Path) -> Self {
        Self::PrefetchAllCompleted {
            tool: tool.to_string(),
            aliases: aliases.to_vec(),
            binary_path: binary_path.to_path_buf(),
        }
    }

    pub fn prefetch_all_failed(tool: &str, aliases: &[String], error: &dyn std::fmt::Display) -> Self {
        Self::PrefetchAllFailed {
            tool: tool.to_string(),
            aliases: aliases.to_vec(),
            error: error.to_string(),
        }
    }
}

impl From<RunnerMessage> for Message {
    fn from(msg: RunnerMessage) -> Self {
        Message::Runner(msg)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;
    use crate::config::ToolConfig;

    #[test]
    fn list_tool_message_round_trips_as_json() {
        let message: Message =
            RunnerMessage::list_tool("timestamp", &ToolConfig::Version("0.1".to_string())).into();

        let json = serde_json::to_string(&message).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_matches!(
            parsed,
            Message::Runner(RunnerMessage::ListTool {
                ref name,
                config: ToolConfig::Version(ref version),
            }) if name == "timestamp" && version == "0.1"
        );
    }

    #[test]
    fn list_alias_message_round_trips_as_json() {
        let message: Message = RunnerMessage::list_alias("ts", "timestamp").into();

        let json = serde_json::to_string(&message).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_matches!(
            parsed,
            Message::Runner(RunnerMessage::ListAlias {
                ref name,
                ref target,
            }) if name == "ts" && target == "timestamp"
        );
    }
}
