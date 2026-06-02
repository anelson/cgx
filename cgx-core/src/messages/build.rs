use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::builder::BuildOptions;

/// Messages related to build operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BuildMessage {
    Started {
        options: BuildOptions,
    },
    CargoMessage {
        message: cargo_metadata::Message,
    },
    /// Some stderr output directly from `cargo build`.  We do not make assumptions about whether
    /// or not `cargo build` output is UTF-8 clean or is line-oriented (its progress bar mechanism
    /// uses `\r` without `\n` for example), so instead we just pass the raw byte chunks.
    /// In `cgx_main` this output will be rendered to the parent process's stderr
    CargoStderr {
        bytes: Vec<u8>,
    },
    Completed {
        binary_path: PathBuf,
    },
}

impl BuildMessage {
    pub fn started(options: &BuildOptions) -> Self {
        Self::Started {
            options: options.clone(),
        }
    }

    pub fn cargo_message(message: cargo_metadata::Message) -> Self {
        Self::CargoMessage { message }
    }

    pub fn cargo_stderr(chunk: Vec<u8>) -> Self {
        Self::CargoStderr { bytes: chunk }
    }

    pub fn completed(binary_path: &std::path::Path) -> Self {
        Self::Completed {
            binary_path: binary_path.to_path_buf(),
        }
    }
}

impl From<BuildMessage> for Message {
    fn from(msg: BuildMessage) -> Self {
        Message::Build(msg)
    }
}
