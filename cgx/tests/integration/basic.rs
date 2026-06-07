//! Very basic smoke tests that just confirm that the `cgx` binary is able to run and do basic
//! operations
use crate::utils::Cgx;

/// Basic test, that `cgx` runs at all, and that `--help` at least looks vaguely right.
///
/// We're not interested in testing that `clap` works, this is a sanity check that will help when
/// running tests in various Docker envs to make sure the libc versions are compatible.
#[test]
fn test_help_output() {
    let mut cgx = Cgx::find();

    cgx.cmd
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("cgx"))
        .stderr(predicates::str::is_empty());
}

/// Sanity-check that the vergen logic to include git sha/date in version output is working, by
/// matching on a regex.
///
/// Expects an output something like:
///
/// ```text
/// cgx 0.0.3 (40d26c9 2025-10-26)
/// ````
#[test]
fn test_version_output() {
    let mut cgx = Cgx::find();

    cgx.cmd
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::is_match(r"cgx \d+\.\d+\.\d+ \([0-9a-f]{7} \d{4}-\d{2}-\d{2}\)\n").unwrap())
        .stderr(predicates::str::is_empty());
}
