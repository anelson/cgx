//! Binary execution with platform-specific signal handling.
//!
//! This module provides functionality to execute built binaries with proper TTY control
//! and signal handling. The implementation follows cargo run's approach:
//!
//! - **Unix (Linux, macOS, BSDs)**: Uses `exec()` to replace the current process
//! - **Windows**: Spawns child, ignores Ctrl-C while waiting, exits with child's code
//! - **Other platforms**: Basic spawn+wait+exit fallback
//!
//! Both [`run_binary_crate`] and [`run_cargo_subcommand`] never return on success: they replace the
//! process (Unix) or exit with the child's exit code (Windows/other).

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::Command,
};

use snafu::ResultExt;

use crate::{Result, cargo, error};

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
/// Only returns an error if the binary cannot be launched. On success, this function either
/// replaces the current process or exits, and thus never returns.
pub fn run_binary_crate(bin_path: &Path, args: &[OsString]) -> Result<()> {
    let mut cmd = Command::new(bin_path);
    cmd.args(args);
    run(cmd)
}

/// Run a Cargo subcommand (ie, `cgx cargo $FOO`, where the binary to be run is `cargo-$FOO`),
/// following the execution behavior of [`run_binary_crate`].
///
/// The bare `subcommand` (ie `$FOO` in the example above) is passed before the user-supplied
/// `args`, consistent with Cargo's own convention for how subcommands are invoked.
///
/// An explicit `toolchain` (if present) is used to determine the runtime environment through
/// `rustup run` and will always use the corresponding Cargo version, regardless of the inherited
/// `CARGO` path and normal executable discovery.
pub fn run_cargo_subcommand(
    bin_path: &Path,
    subcommand: &OsStr,
    args: &[OsString],
    toolchain: Option<&str>,
) -> Result<()> {
    let mut cmd = if let Some(toolchain) = toolchain {
        let mut cmd = Command::new(cargo::rustup_path(toolchain)?);
        // Wrap the command un `rustup run $TOOLCHAIN` so the command is executed in the correct
        // toolchain environment.
        cmd.args(["run", toolchain]).arg(bin_path);
        cmd
    } else {
        Command::new(bin_path)
    };
    // This is an invocation of a cargo subcommand, ie `cgx cargo $FOO`, so we need to behave as
    // `cargo $FOO` would and set the CARGO env var to the path of the cargo binary, and pass the
    // subcommand name as the first argument
    cmd.env("CARGO", cargo::cargo_path(toolchain)?);
    cmd.arg(subcommand).args(args);

    run(cmd)
}

fn run(mut cmd: Command) -> Result<()> {
    let bin_path = Path::new(cmd.get_program()).to_path_buf();

    #[cfg(unix)]
    {
        exec_replace(&bin_path, &mut cmd)
    }

    #[cfg(windows)]
    {
        spawn_and_wait_windows(&bin_path, &mut cmd)
    }

    #[cfg(not(any(unix, windows)))]
    {
        spawn_and_wait_fallback(&bin_path, &mut cmd)
    }
}

/// Unix implementation: Replace current process with the target binary.
///
/// Uses the `exec()` system call to replace the current process image with the new binary.
/// This means cgx's process ID stays the same, but it becomes the target binary.
/// Signals are handled naturally because the target binary receives them directly.
#[cfg(unix)]
fn exec_replace(bin_path: &Path, cmd: &mut Command) -> Result<()> {
    // exec() replaces the current process and never returns on success.
    // Only returns Err if exec fails (binary not found, not executable, etc.)
    Err(cmd.exec()).with_context(|_| error::ExecFailedSnafu {
        path: bin_path.to_owned(),
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
fn spawn_and_wait_windows(bin_path: &Path, cmd: &mut Command) -> Result<()> {
    // Install handler that ignores Ctrl-C in parent process.
    // The child will receive and handle Ctrl-C directly from the Windows console.
    ctrlc::set_handler(|| {
        // Do nothing - just prevent parent from terminating.
        // Child receives console control events independently.
    })
    .context(error::ConsoleHandlerFailedSnafu)?;

    let mut child = cmd.spawn().with_context(|_| error::SpawnFailedSnafu {
        path: bin_path.to_owned(),
    })?;
    let status = child.wait().context(error::WaitFailedSnafu)?;

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
fn spawn_and_wait_fallback(bin_path: &Path, cmd: &mut Command) -> Result<()> {
    let mut child = cmd.spawn().with_context(|_| error::SpawnFailedSnafu {
        path: bin_path.to_owned(),
    })?;
    let status = child.wait().context(error::WaitFailedSnafu)?;

    // Exit with the child's exit code (never returns)
    // This is intentional - the function contract specifies it never returns on success
    let exit_code = status.code().unwrap_or(1);
    #[allow(clippy::exit)]
    std::process::exit(exit_code)
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;

    #[test]
    fn test_run_binary_crate_nonexistent_binary() {
        let result = run_binary_crate(Path::new("/nonexistent/binary"), &[]);
        assert_matches!(
            result,
            Err(error::Error::ExecFailed { .. } | error::Error::SpawnFailed { .. })
        );
    }
}
