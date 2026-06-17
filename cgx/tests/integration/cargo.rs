//! Tests for interoperability with Cargo-managed state.

use assert_cmd::Command;
use assert_fs::{TempDir, prelude::*};

use crate::utils::Cgx;

/// `cgx` should be able to use a fresh Cargo home that has never initialized the crates.io sparse
/// index.
#[test]
fn fresh_cargo_home_can_resolve_and_download_crates_io_crate() {
    let cargo_home = TempDir::with_prefix("cgx-cargo-home-").unwrap();
    let mut cgx = Cgx::with_test_fs();

    cgx.cmd
        .env("CARGO_HOME", cargo_home.path())
        .arg("--list-targets")
        .arg("ripgrep@14")
        .assert()
        .success()
        .stdout(predicates::str::contains("rg"));
}

/// The sparse index config that `cgx` writes for a fresh Cargo home must remain compatible with
/// Cargo itself.
#[test]
fn bootstrapped_crates_io_sparse_index_still_works_for_cargo() {
    let cargo_home = TempDir::with_prefix("cgx-cargo-home-").unwrap();
    let mut cgx = Cgx::with_test_fs();

    cgx.cmd
        .env("CARGO_HOME", cargo_home.path())
        .arg("--list-targets")
        .arg("ripgrep@14")
        .assert()
        .success()
        .stdout(predicates::str::contains("rg"));

    let cargo_project = TempDir::with_prefix("cgx-cargo-project-").unwrap();
    cargo_project
        .child("Cargo.toml")
        .write_str(
            r#"
[package]
name = "cgx-cargo-bootstrap-check"
version = "0.0.0"
edition = "2024"

[dependencies]
itoa = "1"
"#,
        )
        .unwrap();
    cargo_project
        .child("src/lib.rs")
        .write_str(
            "pub fn dependency_smoke_test() -> String { itoa::Buffer::new().format(42).to_string() }\n",
        )
        .unwrap();

    Command::new("cargo")
        .arg("fetch")
        .current_dir(cargo_project.path())
        .env("CARGO_HOME", cargo_home.path())
        .assert()
        .success();
}
