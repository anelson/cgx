use std::{ffi::OsString, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::Message;

/// Messages related to binary execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerMessage {
    ExecutionPlan {
        binary_path: PathBuf,
        args: Vec<String>,
        no_exec: bool,
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
}

impl From<RunnerMessage> for Message {
    fn from(msg: RunnerMessage) -> Self {
        Message::Runner(msg)
    }
}
