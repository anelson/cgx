//! Integration tests for config file handling
//!
//! These tests verify that cgx correctly loads and honors configuration from cgx.toml files,
//! including the config hierarchy (user < cwd < CLI).

use assert_fs::prelude::*;
use predicates::prelude::*;

use crate::utils::Cgx;

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
