//! Tests for interactions with the `cargo` CLI and interoperability with Cargo-managed state.

use std::{env, path::PathBuf};

use assert_cmd::Command;
use assert_fs::{TempDir, prelude::*};

use crate::{testdata::CargoArgvProbe, utils::Cgx};

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

#[test]
fn cargo_plugin_argv_matches_real_cargo() {
    let probe = CargoArgvProbe::copy_to_temp();
    let cargo_home = TempDir::with_prefix("cgx-cargo-home-").unwrap();
    let mut cgx = Cgx::with_test_fs();
    let real_cargo = PathBuf::from(env!("CARGO"));

    // Actually build the cargo argv probe binary using cargo so we can run it using cargo with
    // `cargo argv-probe` and make sure the way `cargo` invokes the binary and the way `cgx` invokes
    // it are the same.  If either `cargo` or `cgx` drift from this, the test will fail so we know
    // we need to look more closely.
    assert!(
        real_cargo.is_absolute(),
        "Cargo must supply its absolute executable path"
    );
    Command::new(&real_cargo)
        .args(["build", "--offline", "--locked"])
        .current_dir(probe.path())
        .env("CARGO_HOME", cargo_home.path())
        .assert()
        .success();

    // Put our `cargo-argv-probe` Rust binary in the PATH for this `cargo` invocation, to ensure
    // that `cargo` finds it and invokes it as a cargo plugin.
    let path = env::join_paths(
        std::iter::once(probe.debug_bin_dir()).chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let args = [
        "check",
        "--features",
        "plugin-feature",
        "--offline",
        "two words",
        "",
        "--",
        "tail",
    ];
    let expected = "[\"argv-probe\", \"check\", \"--features\", \"plugin-feature\", \"--offline\", \"two \
                    words\", \"\", \"--\", \"tail\"]\n";
    let cargo_output = Command::new(&real_cargo)
        .arg("argv-probe")
        .args(args)
        .current_dir(probe.path())
        .env("CARGO_HOME", cargo_home.path())
        .env("PATH", path)
        .assert()
        .success()
        .stdout(expected)
        .get_output()
        .stdout
        .clone();

    cgx.cmd
        .env("CARGO_HOME", cargo_home.path())
        .env("CARGO", &real_cargo)
        .args(["--path"])
        .arg(probe.path())
        .args(["cargo", "argv-probe"])
        .args(args)
        .assert()
        .success()
        .stdout(String::from_utf8(cargo_output).unwrap());
}

#[test]
fn direct_cargo_plugin_crate_keeps_literal_argv() {
    let probe = CargoArgvProbe::copy_to_temp();
    let cargo_home = TempDir::with_prefix("cgx-cargo-home-").unwrap();
    let mut cgx = Cgx::with_test_fs();
    cgx.cmd
        .env("CARGO_HOME", cargo_home.path())
        .env("CARGO", env!("CARGO"))
        .args(["--path"])
        .arg(probe.path())
        .args([
            "cargo-argv-probe",
            "--features",
            "literal",
            "two words",
            "",
            "--",
            "tail",
        ])
        .assert()
        .success()
        .stdout("[\"--features\", \"literal\", \"two words\", \"\", \"--\", \"tail\"]\n");
}

#[test]
fn versioned_cargo_plugin_forwards_bare_subcommand() {
    let probe = CargoArgvProbe::copy_to_temp();
    let cargo_home = TempDir::with_prefix("cgx-cargo-home-").unwrap();
    let mut cgx = Cgx::with_test_fs();
    cgx.cmd
        .env("CARGO_HOME", cargo_home.path())
        .env("CARGO", env!("CARGO"))
        .args(["--path"])
        .arg(probe.path())
        .args(["cargo", "argv-probe@=1.2.3"])
        .assert()
        .success()
        .stdout("[\"argv-probe\"]\n");
}
