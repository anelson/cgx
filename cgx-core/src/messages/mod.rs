pub mod build;
pub mod build_cache;
pub mod crate_resolution;
pub mod git;
pub mod prebuilt_binary;
pub mod runner;
pub mod source;

use std::sync::mpsc;

pub use build::BuildMessage;
pub use build_cache::BuildCacheMessage;
pub use crate_resolution::CrateResolutionMessage;
pub use git::GitMessage;
pub use prebuilt_binary::PrebuiltBinaryMessage;
pub use runner::RunnerMessage;
use serde::{Deserialize, Serialize};
pub use source::SourceMessage;

// Re-export GitSelector since it's used in GitMessage's public API
pub use crate::git::GitSelector;

/// Top-level message enum representing all possible diagnostic messages from cgx.
///
/// Each variant corresponds to a specific subsystem and wraps that subsystem's message type.
/// Messages are serialized as tagged JSON with a "type" field indicating the subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    CrateResolution(CrateResolutionMessage),
    PrebuiltBinary(PrebuiltBinaryMessage),
    Source(SourceMessage),
    BuildCache(BuildCacheMessage),
    Git(GitMessage),
    Build(BuildMessage),
    Runner(RunnerMessage),
}

/// A reporter for diagnostic messages.
///
/// This type is cheaply cloneable and can be shared across threads. It supports two modes:
/// - `Null`: Messages are silently discarded (no-op)
/// - `Channel`: Messages are sent to an mpsc channel for processing
///
/// The `report` method takes a closure to avoid allocating or cloning data unless messages
/// are actually enabled.
#[derive(Clone, Debug)]
pub enum MessageReporter {
    Null,
    Channel(mpsc::SyncSender<Message>),
}

impl MessageReporter {
    /// Create a null reporter that discards all messages.
    pub fn null() -> Self {
        Self::Null
    }

    /// Create a channel reporter that sends messages to the given sender.
    pub fn channel(sender: mpsc::SyncSender<Message>) -> Self {
        Self::Channel(sender)
    }

    /// Report a message by invoking the closure only if messages are enabled.
    ///
    /// The closure is called only when a channel is configured, avoiding any allocation
    /// or cloning overhead when messages are disabled. The closure returns a type that
    /// implements `Into<Message>`, allowing module-specific message types to be used
    /// directly.
    ///
    /// # Example
    ///
    /// ```ignore
    /// reporter.report(|| ResolutionMessage::cache_miss(&spec));
    /// ```
    pub fn report<F, T>(&self, f: F)
    where
        F: FnOnce() -> T,
        T: Into<Message>,
    {
        if let Self::Channel(sender) = self {
            let msg = f().into();
            let _ = sender.send(msg);
        }
    }

    /// Returns true if message reporting is enabled (not null).
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Channel(_))
    }
}
