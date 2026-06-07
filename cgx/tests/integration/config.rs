//! Integration tests for config file handling
//!
//! These tests verify that cgx correctly loads and honors configuration from cgx.toml files,
//! including the config hierarchy (user < cwd < CLI).

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_fs::prelude::*;
use cgx::messages::{Message, RunnerMessage};
use predicates::prelude::*;

use crate::utils::{Cgx, CommandExt};

const TIMESTAMP_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../cgx-core/testdata/crates/timestamp"
);

fn timestamp_fixture(cgx: &Cgx) -> PathBuf {
    copy_timestamp_fixture(cgx.test_fs().cwd.path())
}

fn copy_timestamp_fixture(root: &Path) -> PathBuf {
    let destination = root.join("timestamp-fixture");
    copy_dir(Path::new(TIMESTAMP_FIXTURE), &destination);
    destination
}

fn copy_dir(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();

    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());

        if source_path.is_dir() {
            copy_dir(&source_path, &destination_path);
        } else {
            fs::copy(&source_path, &destination_path).unwrap();
        }
    }
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

/// Test that a cgx.toml in the cwd pins a tool version.
///
/// This verifies that when a cgx.toml file in the current working directory specifies
/// a version constraint for a tool, that constraint is honored when resolving the tool.
#[test]
fn config_pins_version_in_cwd() {
    let mut cgx = Cgx::with_test_fs();

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = "=0.20.0"
"#,
        )
        .unwrap();

    cgx.cmd
        .arg("--no-exec")
        .arg("eza")
        .assert()
        .success()
        .stdout(predicates::str::contains("eza-0.20.0"));
}

/// Test that CLI @version syntax overrides config pinning.
///
/// When the user explicitly specifies a version on the command line (using @version syntax),
/// that should take precedence over any version pinning in config files.
#[test]
fn cli_version_overrides_config_pin() {
    let mut cgx = Cgx::with_test_fs();

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = "=0.20.0"
"#,
        )
        .unwrap();

    // CLI specifies different version - should override config
    cgx.cmd
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert()
        .success()
        .stdout(predicates::str::contains("eza-0.23.1"));
}

/// Test that CWD config overrides user config.
///
/// This is the critical test for config hierarchy. When both user-level and project-level
/// (cwd) config files exist, the project-level config should take precedence.
///
/// Config hierarchy (lowest to highest precedence):
/// - System config
/// - User config
/// - Directory hierarchy configs (closer to cwd = higher precedence)
/// - CLI arguments
#[test]
fn cwd_config_overrides_user_config() {
    let mut cgx = Cgx::with_test_fs();

    // User config pins eza to 0.20.0
    cgx.user_config_dir()
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = "=0.20.0"
"#,
        )
        .unwrap();

    // CWD config pins eza to 0.23.1 (should win)
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = "=0.23.1"
"#,
        )
        .unwrap();

    cgx.cmd
        .arg("--no-exec")
        .arg("eza")
        .assert()
        .success()
        .stdout(predicates::str::contains("eza-0.23.1")) // CWD version wins
        .stdout(predicates::str::contains("eza-0.20.0").not()); // NOT user version
}

/// Test nested directory hierarchy (cwd/project/subdir).
///
/// When running from a subdirectory, config files are discovered by walking up the directory
/// tree from the current directory to the root. Configs closer to the cwd override those
/// farther away.
#[test]
fn nested_config_hierarchy() {
    let mut cgx = Cgx::with_test_fs();

    // Create nested directory structure
    let project_dir = cgx.test_fs().cwd.child("project");
    project_dir.create_dir_all().unwrap();

    let subdir = project_dir.child("subdir");
    subdir.create_dir_all().unwrap();

    // Root config (at cwd level)
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
ripgrep = "14.0"
"#,
        )
        .unwrap();

    // Project config overrides root config
    project_dir
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
ripgrep = "13.0"
"#,
        )
        .unwrap();

    // Run from subdir - project config should win (it's closer than root)
    cgx.cmd
        .current_dir(subdir.path())
        .arg("--no-exec")
        .arg("ripgrep")
        .assert()
        .success()
        .stdout(predicates::str::contains("ripgrep-13.0"));
}

/// Test alias resolution from config.
///
/// Config files can define aliases that map convenient short names to actual crate names.
/// This test verifies that aliases are properly resolved and combined with tool version pinning.
#[test]
fn config_alias_resolution() {
    let mut cgx = Cgx::with_test_fs();

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[aliases]
rg = "ripgrep"

[tools]
ripgrep = "=15.1.0"
"#,
        )
        .unwrap();

    // Use alias 'rg' instead of full name 'ripgrep'
    cgx.cmd
        .arg("--no-exec")
        .arg("rg")
        .assert()
        .success()
        .stdout(predicates::str::contains("ripgrep-15.1.0"));
}

/// Test git source specification from config.
///
/// Tools can specify git repositories as their source instead of crates.io.
/// This verifies that git URLs with tags are properly handled.
#[test]
fn config_specifies_git_source() {
    let mut cgx = Cgx::with_test_fs();

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
cargo-binstall = { git = "https://github.com/cargo-bins/cargo-binstall.git", tag = "v1.14.0" }
"#,
        )
        .unwrap();

    cgx.cmd
        .arg("--no-exec")
        .arg("cargo-binstall")
        .assert()
        .success()
        .stdout(predicates::str::contains("cargo-binstall-1.14.0"));
}

#[test]
fn config_features_are_honored_for_execution() {
    let mut cgx = Cgx::with_test_fs();
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
timestamp = {{ path = "{}", features = ["frobnulator"] }}
"#,
            toml_path(&timestamp_fixture)
        ))
        .unwrap();

    cgx.cmd
        .arg("timestamp")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Features enabled: frobnulator, gonkolator",
        ));
}

#[test]
fn cli_features_replace_config_features_for_execution() {
    let mut cgx = Cgx::with_test_fs();
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
timestamp = {{ path = "{}", features = ["frobnulator"] }}
"#,
            toml_path(&timestamp_fixture)
        ))
        .unwrap();

    cgx.cmd
        .arg("--features")
        .arg("gonkolator")
        .arg("timestamp")
        .assert()
        .success()
        .stdout(predicates::str::contains("Features enabled: gonkolator"))
        .stdout(predicates::str::contains("frobnulator").not());
}

#[test]
fn prefetch_single_tool_prepares_without_stdout() {
    let mut cgx = Cgx::with_test_fs();
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.cmd
        .arg("--prefetch")
        .arg("--path")
        .arg(&timestamp_fixture)
        .arg("timestamp")
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

#[test]
fn prefetch_all_covers_tools_and_alias_invocations() {
    let mut cgx = Cgx::with_test_fs();
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
timestamp = {{ path = "{}" }}

[aliases]
ts = "timestamp"
"#,
            toml_path(&timestamp_fixture)
        ))
        .unwrap();

    cgx.cmd.arg("--prefetch-all").with_json_messages();
    let (assert, messages) = cgx.cmd.assert_with_messages();

    assert.success().stdout(predicates::str::is_empty());

    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllCompleted { invocation, .. })
                    if invocation == "timestamp"
            )
        }),
        "expected prefetch-all completion for configured tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllCompleted { invocation, .. })
                    if invocation == "ts"
            )
        }),
        "expected prefetch-all completion for alias invocation"
    );
}

#[test]
fn prefetch_all_continues_after_failures_and_exits_nonzero() {
    let mut cgx = Cgx::with_test_fs();
    let missing_path = cgx.test_fs().cwd.child("missing");
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
aaa_bad = {{ path = "{}" }}
timestamp = {{ path = "{}" }}
"#,
            toml_path(missing_path.path()),
            toml_path(&timestamp_fixture)
        ))
        .unwrap();

    cgx.cmd.arg("--prefetch-all").with_json_messages();
    let (assert, messages) = cgx.cmd.assert_with_messages();

    assert.failure();

    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllFailed { invocation, .. })
                    if invocation == "aaa_bad"
            )
        }),
        "expected prefetch-all failure for bad tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllCompleted { invocation, .. })
                    if invocation == "timestamp"
            )
        }),
        "expected prefetch-all to continue after a failure"
    );
}

#[test]
fn list_tools_outputs_valid_toml_and_json_messages() {
    let mut cgx = Cgx::with_test_fs();
    let timestamp_fixture = timestamp_fixture(&cgx);

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
timestamp = {{ path = "{}", features = ["frobnulator"] }}

[aliases]
ts = "timestamp"
"#,
            toml_path(&timestamp_fixture)
        ))
        .unwrap();

    cgx.cmd.arg("--list-tools").with_json_messages();
    let (assert, messages) = cgx.cmd.assert_with_messages();
    let output = assert
        .success()
        .stdout(predicates::str::contains("[tools]"))
        .stdout(predicates::str::contains("[aliases]"))
        .get_output()
        .stdout
        .clone();

    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::ListTool { name, .. }) if name == "timestamp"
            )
        }),
        "expected list-tools JSON message for configured tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::ListAlias { name, target })
                    if name == "ts" && target == "timestamp"
            )
        }),
        "expected list-tools JSON message for configured alias"
    );

    let rendered_toml = String::from_utf8(output).unwrap();
    let verify_cwd = assert_fs::TempDir::with_prefix("cgx-list-tools-verify-").unwrap();
    let verify_home = assert_fs::TempDir::with_prefix("cgx-list-tools-home-").unwrap();
    verify_cwd.child("cgx.toml").write_str(&rendered_toml).unwrap();

    let mut verify = Cgx::find();
    verify
        .cmd
        .current_dir(verify_cwd.path())
        .env("HOME", verify_home.path())
        .env("XDG_CONFIG_HOME", verify_home.path().join("config"))
        .arg("--list-tools")
        .assert()
        .success()
        .stdout(predicates::str::contains("timestamp"))
        .stdout(predicates::str::contains("ts"));
}
