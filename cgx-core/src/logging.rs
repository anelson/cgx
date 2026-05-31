#[cfg(test)]
use std::sync::OnceLock;
#[cfg(test)]
use tracing_subscriber::{EnvFilter, fmt};

/// Initialize tracing for tests with sensible defaults.
///
/// This function configures tracing to work correctly with cargo test's output capture,
/// ensuring that log output is only shown for failed tests. It uses [`std::sync::OnceLock`]
/// to ensure that logging is initialized only once per test process, regardless of how many
/// times this function is called.
///
/// # Log Level
///
/// Defaults to DEBUG level, but can be overridden by setting `CGX_LOG` or `RUST_LOG`
/// environment variables before running tests (`CGX_LOG` takes priority).
///
/// # Usage
///
/// Call this at the beginning of any test that would benefit from seeing log output.
#[cfg(test)]
pub(crate) fn init_test_logging() {
    static INIT: OnceLock<()> = OnceLock::new();

    INIT.get_or_init(|| {
        // Try environment variables in priority order: CGX_LOG > RUST_LOG > debug default
        let filter = EnvFilter::try_from_env("CGX_LOG")
            .or_else(|_| EnvFilter::try_from_default_env())
            .unwrap_or_else(|_| EnvFilter::new("debug"));

        // Use test_writer() to integrate with cargo test's output capture
        // This ensures log output only appears for failed tests unless `--nocapture` is used
        fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .with_target(true)
            .with_level(true)
            .init();
    });
}
