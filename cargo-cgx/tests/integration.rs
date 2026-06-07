//! Integration tests for cargo-cgx binary
//!
//! These tests verify that cargo-cgx correctly handles the cargo subcommand invocation pattern,
//! where cargo invokes the binary with argv like: `["cargo-cgx", "cgx", ...user_args]`

use assert_cmd::{Command, cargo::cargo_bin_cmd};

/// Helper struct for running cargo-cgx in tests
struct CargoCgx {
    cmd: Command,
}

impl CargoCgx {
    /// Creates a new [`CargoCgx`] that locates the binary
    fn find() -> Self {
        Self {
            cmd: cargo_bin_cmd!("cargo-cgx"),
        }
    }
}

/// Basic test that cargo-cgx runs and --help output looks correct
#[test]
fn test_help_output() {
    let mut cargo_cgx = CargoCgx::find();

    cargo_cgx
        .cmd
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("cgx"))
        .stderr(predicates::str::is_empty());
}

/// Test that --version shows cgx version information
#[test]
fn test_version_output() {
    let mut cargo_cgx = CargoCgx::find();

    cargo_cgx
        .cmd
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::is_match(r"cgx \d+\.\d+\.\d+").unwrap())
        .stderr(predicates::str::is_empty());
}

/// Test the cargo subcommand invocation pattern: `cargo cgx` -> `cargo-cgx cgx`
///
/// When cargo invokes cargo-cgx as a subcommand, it passes "cgx" as the second argument.
/// This test verifies that cargo-cgx correctly strips that argument and processes the rest.
#[test]
fn test_cargo_subcommand_invocation() {
    let mut cargo_cgx = CargoCgx::find();

    cargo_cgx
        .cmd
        .arg("cgx")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("cgx"))
        .stderr(predicates::str::is_empty());
}

/// Test that the cargo subcommand pattern works with --version
#[test]
fn test_cargo_subcommand_with_version() {
    let mut cargo_cgx = CargoCgx::find();

    cargo_cgx
        .cmd
        .arg("cgx")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::is_match(r"cgx \d+\.\d+\.\d+").unwrap())
        .stderr(predicates::str::is_empty());
}

/// Test that the cargo subcommand pattern works with cgx flags after the subcommand arg.
///
/// This tests: `cargo cgx --no-exec --help` which should be processed as
/// `cgx --no-exec --help` after stripping the "cgx" subcommand arg.
#[test]
fn test_cargo_subcommand_with_flags_and_tool() {
    let mut cargo_cgx = CargoCgx::find();

    // This will fail if the arg stripping doesn't work correctly, because
    // clap would see "cgx" as the crate name and "--no-exec" as its argument
    cargo_cgx
        .cmd
        .arg("cgx")
        .arg("--no-exec")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("cgx"))
        .stderr(predicates::str::is_empty());
}
