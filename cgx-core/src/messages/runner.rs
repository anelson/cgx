use std::{ffi::OsString, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::Message;
use crate::config::ToolConfig;

/// Messages related to binary execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerMessage {
    ExecutionPlan {
        binary_path: PathBuf,
        args: Vec<String>,
        no_exec: bool,
    },
    ListTool {
        name: String,
        config: ToolConfig,
    },
    ListAlias {
        name: String,
        target: String,
    },
    PrefetchStarted {
        invocation: String,
    },
    PrefetchCompleted {
        invocation: String,
        binary_path: PathBuf,
    },
    PrefetchAllStarted {
        invocation: String,
    },
    PrefetchAllCompleted {
        invocation: String,
        binary_path: PathBuf,
    },
    PrefetchAllFailed {
        invocation: String,
        error: String,
    },
}

impl RunnerMessage {
    pub fn execution_plan(binary_path: &std::path::Path, args: &[OsString], no_exec: bool) -> Self {
        Self::ExecutionPlan {
            binary_path: binary_path.to_path_buf(),
            args: args.iter().map(|s| s.to_string_lossy().into_owned()).collect(),
            no_exec,
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

    pub fn prefetch_started(invocation: &str) -> Self {
        Self::PrefetchStarted {
            invocation: invocation.to_string(),
        }
    }

    pub fn prefetch_completed(invocation: &str, binary_path: &std::path::Path) -> Self {
        Self::PrefetchCompleted {
            invocation: invocation.to_string(),
            binary_path: binary_path.to_path_buf(),
        }
    }

    pub fn prefetch_all_started(invocation: &str) -> Self {
        Self::PrefetchAllStarted {
            invocation: invocation.to_string(),
        }
    }

    pub fn prefetch_all_completed(invocation: &str, binary_path: &std::path::Path) -> Self {
        Self::PrefetchAllCompleted {
            invocation: invocation.to_string(),
            binary_path: binary_path.to_path_buf(),
        }
    }

    pub fn prefetch_all_failed(invocation: &str, error: &dyn std::fmt::Display) -> Self {
        Self::PrefetchAllFailed {
            invocation: invocation.to_string(),
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
