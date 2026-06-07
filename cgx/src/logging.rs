use std::{io::IsTerminal, sync::OnceLock};

use tracing::Level;
use tracing_subscriber::{EnvFilter, fmt, prelude::*, reload};

use crate::Config;

/// Default tracing filter expression for INFO level logging.
///
/// This will be expanded in the future to tune per-crate log levels, as some crates are very
/// quiet at DEBUG while others spam excessively at INFO.
const DEFAULT_TRACING_FILTER: &str = "info";

/// Handle for reloading the tracing filter at runtime.
///
/// This allows us to update the log level after loading configuration from files,
/// while still having logging available during the config loading process itself.
type ReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Global storage for the reload handle, initialized once during [`init`].
static RELOAD_HANDLE: OnceLock<ReloadHandle> = OnceLock::new();

/// Initialize tracing/logging based on the contents of the parsed CLI args.
///
/// This function configures the global tracing subscriber with appropriate filtering,
/// formatting, and output options based on the verbosity level requested by the user.
///
/// # Verbosity levels
///
/// - `0`: WARN and ERROR only, simple format with color (silent on happy path)
/// - `1`: INFO level, structured format with timestamp/target
/// - `2`: DEBUG level, structured format
/// - `3+`: TRACE level, structured format
///
/// # Environment variable support
///
/// Log filtering can be controlled via environment variables in priority order:
/// 1. `CGX_LOG` - cgx-specific log filter (checked first)
/// 2. `RUST_LOG` - standard Rust log filter (fallback)
/// 3. Hard-coded defaults based on verbosity level (if neither env var is set)
///
/// This allows for fine-grained control of logging output without recompiling.
///
/// # Panics
///
/// This function will panic if called more than once in the same process, as the
/// global tracing subscriber can only be initialized once.
pub(crate) fn init(verbose: u8) {
    let (level, use_simple_format) = match verbose {
        0 => (Level::WARN, true),
        1 => (Level::INFO, false),
        2 => (Level::DEBUG, false),
        _ => (Level::TRACE, false),
    };

    // Build the filter by checking environment variables in priority order
    // Try environment variables in priority order: CGX_LOG > RUST_LOG > hard-coded default
    let filter = EnvFilter::try_from_env("CGX_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| {
            // Neither env var set, use hard-coded default based on verbosity
            if verbose == 0 {
                // For silent mode, only show WARN and ERROR
                EnvFilter::new("warn")
            } else {
                // For verbose modes, use the default filter expression at the determined level
                EnvFilter::new(format!("{},{}", DEFAULT_TRACING_FILTER, level))
            }
        });

    // Wrap the filter in a reload layer so we can update it later based on config
    let (filter, reload_handle) = reload::Layer::new(filter);

    // Store the reload handle for later use by apply_config()
    // Ignore the error if already set (e.g., in tests that call init multiple times)
    let _ = RELOAD_HANDLE.set(reload_handle);

    // Check if we're outputting to a TTY for color support
    let use_ansi = std::io::stderr().is_terminal();

    if use_simple_format {
        // Simple format for default (non-verbose) mode: just the message, with color if TTY
        // This is meant to not even look very "loggy", and just prints log messages, one per line.
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_level(true)
                    .with_ansi(use_ansi)
                    .without_time(),
            )
            .init();
    } else {
        // Structured format for verbose modes: timestamp, target, level, message
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .with_target(true)
                    .with_level(true)
                    .with_ansi(use_ansi),
            )
            .init();
    }
}

/// Apply logging configuration from the config file if appropriate.
///
/// This function should be called after [`Config::load`] to apply any log level settings from
/// the config file. It respects the following priority order:
/// 1. `CGX_LOG` environment variable (highest priority, checked at init time)
/// 2. `RUST_LOG` environment variable (checked at init time)
/// 3. CLI `-v` flags (if user specified verbosity, don't override)
/// 4. `Config.log_level` field (applied by this function)
/// 5. Hard-coded defaults (lowest priority, set at init time)
///
/// # Arguments
///
/// * `config` - The loaded configuration containing the optional `log_level` field
/// * `args` - The CLI arguments to check if user explicitly set verbosity
///
/// # Log Level Format
///
/// The `config.log_level` field should be a valid tracing filter string, such as:
/// - `"trace"` - Most verbose, includes all events
/// - `"debug"` - Debug and above
/// - `"info"` - Info and above (default)
/// - `"warn"` - Warnings and errors only
/// - `"error"` - Errors only
///
/// More complex filter syntax is also supported (e.g., `"cgx=debug,info"`).
pub(crate) fn apply_config(config: &Config, verbose: u8) {
    // Don't override if user explicitly set verbosity via CLI
    if verbose > 0 {
        tracing::debug!("Not applying config log_level: CLI verbosity flag takes precedence");
        return;
    }

    // Don't override if environment variables are set (they have higher priority)
    if std::env::var("CGX_LOG").is_ok() || std::env::var("RUST_LOG").is_ok() {
        tracing::debug!("Not applying config log_level: environment variable takes precedence");
        return;
    }

    // Check if config specifies a log level
    let Some(ref log_level) = config.log_level else {
        tracing::debug!("No log_level specified in config");
        return;
    };

    // Parse the log level from config
    let new_filter = match EnvFilter::try_new(log_level) {
        Ok(filter) => filter,
        Err(e) => {
            tracing::warn!("Invalid log_level in config file: {}: {}", log_level, e);
            return;
        }
    };

    // Get the reload handle and apply the new filter
    if let Some(handle) = RELOAD_HANDLE.get() {
        match handle.reload(new_filter) {
            Ok(()) => {
                tracing::info!("Applied log level from config: {}", log_level);
            }
            Err(e) => {
                tracing::warn!("Failed to reload log filter: {}", e);
            }
        }
    } else {
        tracing::warn!("Reload handle not initialized; cannot apply config log level");
    }
}
