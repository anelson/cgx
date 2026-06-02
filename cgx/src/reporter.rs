//! Reporter logic that takes messages from the cgx core impl and pushes them out to the user in
//! the appropriate way.
use std::io::Write;

use cgx_core::messages;
use tracing::*;

const MESSAGE_CHANNEL_SIZE: usize = 100;

/// Handle to the thread that is responsible for reporting messages to the user.
///
/// This thread pulls messages from the [`messages::MessageReporter`] passed to the cgx core,
/// and processes them for display to the user.
pub(crate) struct ReporterThread {
    reporter_thread: std::thread::JoinHandle<()>,
    message_reporter: messages::MessageReporter,
}

impl ReporterThread {
    /// Set up a channel reporter to run in a separate spawned thread.
    /// This thread handles:
    /// 1. `CargoStderrChunk` messages: echoed to stderr
    /// 2. All messages in JSON mode: serialized to stdout
    pub(crate) fn spawn(json_mode: bool) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(MESSAGE_CHANNEL_SIZE);
        let reporter_thread = std::thread::spawn(move || {
            debug!("Starting message reporter thread");
            for msg in rx {
                // Handle CargoStderrChunk by echoing to stderr
                if let messages::Message::Build(messages::BuildMessage::CargoStderr { ref bytes }) = msg {
                    let _ = std::io::stderr().write_all(bytes);
                    let _ = std::io::stderr().flush();
                }

                // In JSON mode, serialize all messages to stdout
                if json_mode {
                    match serde_json::to_string(&msg) {
                        Ok(json) => println!("{}", json),
                        Err(e) => eprintln!("Failed to serialize message: {}", e),
                    }
                }
            }
            debug!("Message reporter thread exiting");
        });
        let message_reporter = messages::MessageReporter::channel(tx);
        Self {
            reporter_thread,
            message_reporter,
        }
    }

    /// Get the instance of `MessageReporter` that can be used to pass messages to this
    /// reporter's thread.
    ///
    /// This is cheap to clone.
    pub(crate) fn message_reporter(&self) -> &messages::MessageReporter {
        &self.message_reporter
    }

    /// Block until the reporter thread exits.
    ///
    /// That wont' happen until every clone of the `MessageReporter` has been dropped.
    /// Make sure not to call this from code that still holds a clone of the `MessageReporter` or it
    /// will deadlock.
    pub(crate) fn join(self) {
        drop(self.message_reporter);
        let _ = self.reporter_thread.join();
    }
}
