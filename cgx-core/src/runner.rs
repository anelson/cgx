//! Binary execution with platform-specific signal handling.
//!
//! This module provides functionality to execute built binaries with proper TTY control
//! and signal handling. The implementation follows cargo run's approach:
//!
//! - **Unix (Linux, macOS, BSDs)**: Uses `exec()` to replace the current process
//! - **Windows**: Spawns child, ignores Ctrl-C while waiting, exits with child's code
//! - **Other platforms**: Basic spawn+wait+exit fallback
//!
//! The `run()` function never returns on success - it either replaces the process (Unix)
//! or exits with the child's exit code (Windows/other).

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{ffi::OsString, path::Path, process::Command};

#[cfg(windows)]
use snafu::ResultExt;

#[cfg(windows)]
use crate::error;
use crate::error::{Error, Result};

/// Run a binary, replacing or waiting for it depending on platform.
///
/// This function executes the binary at `bin_path` with the given `args`, providing
/// proper TTY control and signal handling appropriate for the platform.
///
/// # Platform Behavior
///
/// - **Unix**: Replaces the current process via `execvp()`. Never returns on success.
/// - **Windows**: Spawns child, ignores Ctrl-C, waits, then exits with child's code.
/// - **Other**: Basic spawn+wait+exit (may have suboptimal signal handling).
///
/// # Arguments
///
/// * `bin_path` - Path to the binary to execute
/// * `args` - Arguments to pass to the binary
///
/// # Returns
///
/// Only returns `Err` if the binary cannot be launched. On success, this function
/// either replaces the current process or exits, and thus never returns.
pub fn run(bin_path: &Path, args: &[OsString]) -> Result<()> {
    #[cfg(unix)]
    {
        exec_replace(bin_path, args)
    }

    #[cfg(windows)]
    {
        spawn_and_wait_windows(bin_path, args)
    }

    #[cfg(not(any(unix, windows)))]
    {
        spawn_and_wait_fallback(bin_path, args)
    }
}

/// Unix implementation: Replace current process with the target binary.
///
/// Uses the `exec()` system call to replace the current process image with the new binary.
/// This means cgx's process ID stays the same, but it becomes the target binary.
/// Signals are handled naturally because the target binary receives them directly.
#[cfg(unix)]
fn exec_replace(bin_path: &Path, args: &[OsString]) -> Result<()> {
    let mut cmd = Command::new(bin_path);
    cmd.args(args);
    // Environment and current directory are inherited by default

    // exec() replaces the current process and never returns on success.
    // Only returns Err if exec fails (binary not found, not executable, etc.)
    let err = cmd.exec();

    // Only reachable if exec() failed
    Err(err).map_err(|source| Error::ExecFailed {
        path: bin_path.to_owned(),
        source,
    })
}

/// Windows implementation: Spawn child and wait, ignoring Ctrl-C events.
///
/// On Windows, we can't replace the process, so we:
/// 1. Install a console control handler that ignores Ctrl-C
/// 2. Spawn the child process
/// 3. Wait for it to complete
/// 4. Exit with the child's exit code
///
/// Both the parent (cgx) and child receive Ctrl-C events. The parent ignores them,
/// allowing the child to handle signals as it sees fit.
#[cfg(windows)]
fn spawn_and_wait_windows(bin_path: &Path, args: &[OsString]) -> Result<()> {
    // Install handler that ignores Ctrl-C in parent process.
    // The child will receive and handle Ctrl-C directly from the Windows console.
    ctrlc::set_handler(|| {
        // Do nothing - just prevent parent from terminating.
        // Child receives console control events independently.
    })
    .context(error::ConsoleHandlerFailedSnafu)?;

    // Spawn the child process
    let mut child = Command::new(bin_path)
        .args(args)
        .spawn()
        .map_err(|source| Error::SpawnFailed {
            path: bin_path.to_owned(),
            source,
        })?;

    // Wait for the child to complete
    let status = child.wait().map_err(|source| Error::WaitFailed { source })?;

    // Exit with the child's exit code (never returns)
    // This is intentional - the function contract specifies it never returns on success
    let exit_code = status.code().unwrap_or(1);
    #[allow(clippy::exit)]
    std::process::exit(exit_code)
}

/// Fallback implementation for platforms that are neither Unix nor Windows.
///
/// This provides basic spawn+wait+exit functionality without special signal handling.
/// On most POSIX-like systems, the default signal handling will likely be reasonable,
/// but Ctrl-C behavior may not be optimal.
#[cfg(not(any(unix, windows)))]
fn spawn_and_wait_fallback(bin_path: &Path, args: &[OsString]) -> Result<()> {
    // Spawn the child process
    let mut child = Command::new(bin_path)
        .args(args)
        .spawn()
        .map_err(|source| Error::SpawnFailed {
            path: bin_path.to_owned(),
            source,
        })?;

    // Wait for the child to complete
    let status = child.wait().map_err(|source| Error::WaitFailed { source })?;

    // Exit with the child's exit code (never returns)
    // This is intentional - the function contract specifies it never returns on success
    let exit_code = status.code().unwrap_or(1);
    #[allow(clippy::exit)]
    std::process::exit(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_nonexistent_binary() {
        // Attempting to run a nonexistent binary should return an error
        let result = run(Path::new("/nonexistent/binary"), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_with_args() {
        // This test would need a test binary to actually execute,
        // which would be better handled in integration tests
        // For now, just verify the function signature is correct
        let _: fn(&Path, &[OsString]) -> Result<()> = run;
    }
}
