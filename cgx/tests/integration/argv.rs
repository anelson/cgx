//! Executable-name dispatch through real symlinks, without registry or network access.
//!
//! These tests invoke `cgx` via a symlink where the symlink specifies a tool to run, making sure
//! that this alternative tool calling mechanism works properly.
//!
//! Rather than using a real crate, the tests leverage our argv-probe test crate so we can see
//! exactly what is invoked and how.

use std::{
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::symlink},
};

use assert_cmd::{Command, cargo::cargo_bin};
use assert_fs::prelude::*;

use crate::{testdata::ArgvProbe, utils::Cgx};

#[test]
fn argv0_selects_tool() {
    let probe = ArgvProbe::copy_to_temp();
    let cgx = Cgx::with_test_fs();
    let fs = cgx.test_fs();
    cgx.user_config_dir()
        .child("cgx.toml")
        .write_str(&format!(
            r#"
offline = true
[prebuilt_binaries]
use_prebuilt_binaries = "never"
[aliases]
rg = "argv-probe"
[tools]
argv-probe = {{ path = {:?}, version = "=1.2.3" }}
"#,
            probe.path().to_str().unwrap()
        ))
        .unwrap();

    let args: Vec<OsString> = [
        "--",
        "--",
        "--help",
        "--version",
        "--offline",
        "--git",
        "--path",
        "--prefetch",
        "+nightly",
        "cgx",
        "",
        "pattern",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([OsString::from_vec(vec![0xff])])
    .collect();
    let expected = format!("{args:?}\n");

    for name in ["rg", "rg@=1.2.3", "argv-probe"] {
        let link = fs.cwd.child(name);
        symlink(cargo_bin!("cgx"), link.path()).unwrap();
        Command::new(link.path())
            .current_dir(fs.cwd.path())
            .env("CGX_SYSTEM_CONFIG_DIR", fs.system_root.path())
            .env("CGX_APP_DIR", fs.app_root.path())
            .env("CGX_USER_CONFIG_DIR", cgx.user_config_dir().path())
            .env("CGX_HTTP_TIMEOUT", "5s")
            .env("CGX_HTTP_RETRIES", "0")
            .env_remove("CGX_HTTP_PROXY")
            .args(&args)
            .assert()
            .success()
            .stdout(expected.clone());
    }

    Command::new(fs.cwd.child("rg").path())
        .env("CGX_HTTP_RETRIES", "not-a-number")
        .arg("--help")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--http-retries"));
}

#[test]
fn versioned_argv0_keeps_cgx_parsing() {
    let cgx = Cgx::with_test_fs();
    for (name, args) in [
        ("cgx@1", vec!["--help"]),
        ("cgx@=0.1.2", vec!["--version"]),
        ("cargo-cgx@1", vec!["cgx", "--help"]),
        ("cargo-cgx@=0.1.2", vec!["cgx", "--version"]),
    ] {
        let link = cgx.test_fs().cwd.child(name);
        symlink(cargo_bin!("cgx"), link.path()).unwrap();
        Command::new(link.path())
            .args(args)
            .assert()
            .success()
            .stdout(predicates::str::contains("cgx"))
            .stderr(predicates::str::is_empty());
    }
}
