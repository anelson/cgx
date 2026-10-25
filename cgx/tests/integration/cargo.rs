//! Tests for interactions with the `cargo` CLI and interoperability with Cargo-managed state.
//!
//! We have a dummy `argv-probe` crate that we use to inspect the actual arguments passed to
//! whatever cgx executes.  For these tests we have to rename it to have a `cargo-` prefix so that
//! Cargo recognizes it as a cargo plugin.

use std::{env, fs, path::PathBuf};

use assert_cmd::{Command, cargo::cargo_bin_cmd};
use assert_fs::{TempDir, prelude::*};
use cgx::messages::{Message, RunnerMessage};
use serde_json::{Value, json};

use crate::{
    testdata::ArgvProbe,
    utils::{Cgx, CommandExt, JsonOutput},
};

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
    let probe = ArgvProbe::copy_to_temp_as_crate("cargo-argv-probe");
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
    let cargo_output = Command::new(&real_cargo)
        .arg("argv-probe")
        .args(args)
        .current_dir(probe.path())
        .env("PATH", path)
        .env_remove("CARGO")
        .env("CARGO_PKG_NAME", "user-package")
        .assert_json_stdout();
    let expected_args: Vec<_> = ["argv-probe"].into_iter().chain(args).collect();
    assert_eq!(cargo_output.query("$.argv"), [&json!(expected_args)]);
    assert_eq!(
        cargo_output.query("$.env[?@.name == 'CARGO'].value"),
        [&json!(real_cargo)]
    );
    assert_eq!(
        cargo_output.query("$.env[?@.name == 'CARGO_PKG_NAME'].value"),
        [&json!("user-package")]
    );

    let path = env::join_paths(
        std::iter::once(real_cargo.parent().unwrap().to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let cgx_output = cgx
        .cmd
        .env("PATH", path)
        .env_remove("CARGO")
        .env("CARGO_PKG_NAME", "user-package")
        .args(["--path"])
        .arg(probe.path())
        .args(["cargo", "argv-probe"])
        .args(args)
        .assert_json_stdout();
    for path in [
        "$.argv",
        "$.env[?@.name == 'CARGO'].value",
        "$.env[?@.name == 'CARGO_PKG_NAME'].value",
    ] {
        assert_eq!(
            cgx_output.query(path),
            cargo_output.query(path),
            "JSONPath {path}"
        );
    }
}

#[test]
fn direct_cargo_plugin_crate_keeps_literal_argv() {
    let probe = ArgvProbe::copy_to_temp_as_crate("cargo-argv-probe");
    let mut cgx = Cgx::with_test_fs();
    for cargo in [None, Some(env!("CARGO"))] {
        cgx = cgx.reset();
        cgx.cmd.env_remove("CARGO");
        if let Some(cargo) = cargo {
            cgx.cmd.env("CARGO", cargo);
        }
        let (output, messages) = cgx
            .cmd
            .with_json_messages()
            .env("CARGO_PKG_NAME", "user-package")
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
            .assert_with_messages();
        let output = JsonOutput::from_output(output.get_output().clone());
        let expected_args =
            Vec::from(["--features", "literal", "two words", "", "--", "tail"].map(str::to_owned));
        let reported_args = messages.iter().find_map(|message| match message {
            Message::Runner(RunnerMessage::BinaryExecutionPlan {
                args, no_exec: false, ..
            }) => Some(args),
            _ => None,
        });
        assert_eq!(reported_args, Some(&expected_args));
        assert_eq!(output.query("$.argv"), [&json!(expected_args)]);
        match cargo {
            Some(cargo) => assert_eq!(output.query("$.env[?@.name == 'CARGO'].value"), [&json!(cargo)]),
            None => assert_eq!(
                output.query("$.env[?@.name == 'CARGO'].value"),
                Vec::<&Value>::new()
            ),
        }
        assert_eq!(
            output.query("$.env[?@.name == 'CARGO_PKG_NAME'].value"),
            [&json!("user-package")]
        );
    }
}

#[test]
fn selected_toolchain_applies_to_both_invocation_modes() {
    let probe = ArgvProbe::copy_to_temp_as_crate("cargo-argv-probe");
    let cgx = Cgx::with_test_fs();
    let rustup = env::var_os("RUSTUP").unwrap_or_else(|| "rustup".into());
    let output = Command::new(&rustup)
        .args(["show", "active-toolchain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let toolchain = output.split_whitespace().next().unwrap();
    let output = Command::new(&rustup)
        .args(["which", "--toolchain", toolchain, "cargo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cargo_path = PathBuf::from(String::from_utf8(output).unwrap().trim());
    let inherited_cargo = cgx
        .test_fs()
        .cwd
        .child(format!("cargo{}", env::consts::EXE_SUFFIX));
    fs::copy(&cargo_path, inherited_cargo.path()).unwrap();
    for cli_toolchain in [false, true] {
        let configured_toolchain = if cli_toolchain {
            "cgx-test-missing-toolchain"
        } else {
            toolchain
        };
        cgx.user_config_dir()
            .child("cgx.toml")
            .write_str(&format!("toolchain = {configured_toolchain:?}\n"))
            .unwrap();

        for (invocation, expected_args, expected_cargo) in [
            (
                vec!["cargo", "argv-probe"],
                vec!["argv-probe"],
                cargo_path.as_path(),
            ),
            (vec!["cargo-argv-probe"], vec![], inherited_cargo.path()),
        ] {
            let mut cmd = cargo_bin_cmd!("cgx");
            if cli_toolchain {
                cmd.arg(format!("+{toolchain}"));
            }
            let output = cmd
                .arg("--system-config-dir")
                .arg(cgx.test_fs().system_root.path())
                .arg("--app-dir")
                .arg(cgx.test_fs_app_root().path())
                .current_dir(cgx.test_fs().cwd.path())
                .env("CARGO", inherited_cargo.path())
                .env_remove("CARGO_PKG_NAME")
                .args(["--offline", "--path"])
                .arg(probe.path())
                .args(invocation)
                .assert_json_stdout();
            assert_eq!(output.query("$.argv"), [&json!(expected_args)]);
            assert_eq!(
                output.query("$.env[?@.name == 'CARGO'].value"),
                [&json!(expected_cargo)]
            );
            assert_eq!(
                output.query("$.env[?@.name == 'CARGO_PKG_NAME'].value"),
                Vec::<&Value>::new()
            );
        }
    }
}

#[test]
fn cargo_plugin_no_exec_prints_only_binary_path() {
    let probe = ArgvProbe::copy_to_temp_as_crate("cargo-argv-probe");
    let mut cgx = Cgx::with_test_fs();
    let output = cgx
        .cmd
        .env_remove("CARGO")
        .args(["--offline", "--no-exec", "--path"])
        .arg(probe.path())
        .args(["cargo", "argv-probe"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.lines().count(), 1, "output: {output:?}");
    assert!(PathBuf::from(output.trim()).is_file(), "output: {output:?}");
}

#[test]
fn versioned_cargo_plugin_forwards_bare_subcommand() {
    let probe = ArgvProbe::copy_to_temp_as_crate("cargo-argv-probe");
    let mut cgx = Cgx::with_test_fs();
    let (output, messages) = cgx
        .cmd
        .with_json_messages()
        .env("CARGO", env!("CARGO"))
        .args(["--path"])
        .arg(probe.path())
        .env_remove("CARGO_PKG_NAME")
        .args(["cargo", "argv-probe@=1.2.3", "--help", "two words", ""])
        .assert_with_messages();
    let output = JsonOutput::from_output(output.get_output().clone());
    let expected_args = Vec::from(["--help", "two words", ""].map(str::to_owned));
    let reported_command = messages.iter().find_map(|message| match message {
        Message::Runner(RunnerMessage::CargoSubcommandExecutionPlan { subcommand, args, .. }) => {
            Some((subcommand.as_str(), args.as_slice()))
        }
        _ => None,
    });
    assert_eq!(reported_command, Some(("argv-probe", expected_args.as_slice())));
    assert_eq!(
        output.query("$.argv"),
        [&json!(["argv-probe", "--help", "two words", ""])]
    );
    assert_eq!(
        output.query("$.env[?@.name == 'CARGO'].value"),
        [&json!(env!("CARGO"))]
    );
    assert_eq!(
        output.query("$.env[?@.name == 'CARGO_PKG_NAME'].value"),
        Vec::<&Value>::new()
    );
}
