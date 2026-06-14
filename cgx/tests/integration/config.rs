//! Integration tests for config file handling
//!
//! These tests verify that cgx correctly loads and honors configuration from cgx.toml files,
//! including the config hierarchy (user < cwd < CLI).

use assert_fs::prelude::*;
use cgx::messages::{CgxMessage, Message, Provenance, RunnerMessage};
use predicates::prelude::*;

use crate::utils::{Cgx, CommandExt};

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
fn config_features_are_honored() {
    let mut cgx = Cgx::with_test_fs();

    // A feature configured for a tool in `[tools]` must reach the resolved build plan. We assert on
    // the `CratePlan` message via `--list-targets` (resolve + metadata, no compile) rather than
    // running the tool. `vendored-openssl` is a real `eza` feature, so resolution succeeds.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = { version = "=0.23.1", features = ["vendored-openssl"] }
"#,
        )
        .unwrap();

    cgx.cmd.arg("--list-targets").with_json_messages().arg("eza");
    let (assert, messages) = cgx.cmd.assert_with_messages();
    assert.success();

    let features = messages
        .iter()
        .find_map(|message| match message {
            Message::Cgx(CgxMessage::CratePlan { options, .. }) => Some(options.features.clone()),
            _ => None,
        })
        .expect("expected a CratePlan message");

    assert_eq!(features, vec!["vendored-openssl"]);
}

#[test]
fn cli_features_replace_config_features() {
    let mut cgx = Cgx::with_test_fs();

    // `--features` on the CLI replaces (does not merge with) the configured tool features, so only
    // the CLI feature reaches the build plan. The configured feature is never applied, so it need
    // not be a real `eza` feature.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = { version = "=0.23.1", features = ["a-config-only-feature"] }
"#,
        )
        .unwrap();

    cgx.cmd
        .arg("--list-targets")
        .arg("--features")
        .arg("vendored-openssl")
        .with_json_messages()
        .arg("eza");
    let (assert, messages) = cgx.cmd.assert_with_messages();
    assert.success();

    let features = messages
        .iter()
        .find_map(|message| match message {
            Message::Cgx(CgxMessage::CratePlan { options, .. }) => Some(options.features.clone()),
            _ => None,
        })
        .expect("expected a CratePlan message");
    assert_eq!(features, vec!["vendored-openssl"]);
    assert!(!features.contains(&"a-config-only-feature".to_string()));
}

#[test]
fn config_default_features_are_honored() {
    let mut cgx = Cgx::with_test_fs();

    // `default-features = false` configured for a tool in `[tools]` must reach the resolved build
    // plan as `no_default_features`. We assert on the `CratePlan` message via `--list-targets`
    // (resolve + metadata, no compile) rather than running the tool.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = { version = "=0.23.1", default-features = false }
"#,
        )
        .unwrap();

    cgx.cmd.arg("--list-targets").with_json_messages().arg("eza");
    let (assert, messages) = cgx.cmd.assert_with_messages();
    assert.success();

    let no_default_features = messages
        .iter()
        .find_map(|message| match message {
            Message::Cgx(CgxMessage::CratePlan { options, .. }) => Some(options.no_default_features),
            _ => None,
        })
        .expect("expected a CratePlan message");

    assert!(no_default_features);
}

#[test]
fn config_default_features_and_cli_features_coexist() {
    let mut cgx = Cgx::with_test_fs();

    // Config `default-features` is independent of the feature list: passing `--features` on the CLI
    // replaces the (here unset) configured features but leaves the configured `default-features =
    // false` in effect, so the build plan carries both.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = { version = "=0.23.1", default-features = false }
"#,
        )
        .unwrap();

    cgx.cmd
        .arg("--list-targets")
        .arg("--features")
        .arg("vendored-openssl")
        .with_json_messages()
        .arg("eza");
    let (assert, messages) = cgx.cmd.assert_with_messages();
    assert.success();

    let options = messages
        .iter()
        .find_map(|message| match message {
            Message::Cgx(CgxMessage::CratePlan { options, .. }) => Some(options.clone()),
            _ => None,
        })
        .expect("expected a CratePlan message");

    assert!(options.no_default_features);
    assert_eq!(options.features, vec!["vendored-openssl"]);
}

#[test]
fn prefetch_single_tool_prepares_without_stdout() {
    let mut cgx = Cgx::with_test_fs();

    // Default options leave a pre-built binary eligible, so this prepares `eza` without compiling.
    cgx.cmd.arg("--prefetch").with_json_messages().arg("eza@=0.23.1");
    let (assert, messages) = cgx.cmd.assert_with_messages();
    assert.success().stdout(predicates::str::is_empty());

    // Preparing the crate emits a provenance message recording how (and where) the binary was obtained.
    let binary_path = messages
        .iter()
        .find_map(|message| match message {
            Message::Cgx(CgxMessage::CrateProvenance {
                provenance: Provenance::Prebuilt { binary_path, .. },
                ..
            }) => Some(binary_path.clone()),
            _ => None,
        })
        .expect("expected a CrateProvenance message reporting a pre-built binary");
    assert!(
        !binary_path.as_os_str().is_empty(),
        "prebuilt provenance should report the binary path"
    );
}

#[test]
fn prefetch_all_prefetches_each_configured_tool_once() {
    let mut cgx = Cgx::with_test_fs();

    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = "=0.23.1"

[aliases]
e = "eza"
"#,
        )
        .unwrap();

    cgx.cmd.arg("--prefetch-all").with_json_messages();
    let (assert, messages) = cgx.cmd.assert_with_messages();

    assert.success().stdout(predicates::str::is_empty());

    // The tool and its alias resolve to the same crate, so there is exactly one prefetch, reported
    // under the tool name with the alias listed alongside it.
    let completions: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Runner(RunnerMessage::PrefetchAllCompleted { tool, aliases, .. }) => {
                Some((tool.clone(), aliases.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        completions,
        [("eza".to_string(), vec!["e".to_string()])],
        "expected exactly one prefetch-all completion, reporting the tool with its alias"
    );

    assert!(
        !messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(
                    RunnerMessage::PrefetchAllStarted { tool, .. }
                        | RunnerMessage::PrefetchAllCompleted { tool, .. }
                        | RunnerMessage::PrefetchAllFailed { tool, .. }
                ) if tool == "e"
            )
        }),
        "expected no prefetch-all messages under the alias name"
    );
}

#[test]
fn prefetch_all_continues_after_failures_and_exits_nonzero() {
    let mut cgx = Cgx::with_test_fs();
    let missing_path = cgx.test_fs().cwd.child("missing");

    // `aaa_bad` points at a missing path so it fails deterministically (no network); `eza` still
    // resolves, and the overall command exits non-zero.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&format!(
            r#"
[tools]
aaa_bad = {{ path = "{}" }}
eza = "=0.23.1"
"#,
            missing_path.display().to_string().replace('\\', "\\\\"),
        ))
        .unwrap();

    cgx.cmd.arg("--prefetch-all").with_json_messages();
    let (assert, messages) = cgx.cmd.assert_with_messages();

    assert.failure();

    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllFailed { tool, .. })
                    if tool == "aaa_bad"
            )
        }),
        "expected prefetch-all failure for bad tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::PrefetchAllCompleted { tool, .. })
                    if tool == "eza"
            )
        }),
        "expected prefetch-all to continue after a failure"
    );
}

#[test]
fn list_tools_outputs_valid_toml_and_json_messages() {
    let mut cgx = Cgx::with_test_fs();

    // `--list-tools` only renders the merged config, so no crate is resolved or built.
    cgx.test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(
            r#"
[tools]
eza = { version = "=0.23.1", features = ["vendored-openssl"] }

[aliases]
e = "eza"
"#,
        )
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
                Message::Runner(RunnerMessage::ListTool { name, .. }) if name == "eza"
            )
        }),
        "expected list-tools JSON message for configured tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::ListAlias { name, target })
                    if name == "e" && target == "eza"
            )
        }),
        "expected list-tools JSON message for configured alias"
    );

    // Now the ultimate test of the TOML output by `--list-tools: The round-trip test.
    //
    // Feed the rendered TOML back into a fully isolated `cgx` (the same `with_test_fs`
    // isolation as the rest of the suite, so a host `/etc/cgx.toml` cannot contaminate the result)
    // and confirm the tool and alias survive re-rendering. Verify via the structured messages, not
    // substring matches.
    let rendered_toml = String::from_utf8(output).unwrap();

    let mut verify = Cgx::with_test_fs();
    verify
        .test_fs()
        .cwd
        .child("cgx.toml")
        .write_str(&rendered_toml)
        .unwrap();

    verify.cmd.arg("--list-tools").with_json_messages();
    let (assert, messages) = verify.cmd.assert_with_messages();
    assert
        .success()
        .stdout(predicates::str::contains("[tools]"))
        .stdout(predicates::str::contains("[aliases]"));

    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::ListTool { name, .. }) if name == "eza"
            )
        }),
        "expected the re-rendered config to still list the configured tool"
    );
    assert!(
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Runner(RunnerMessage::ListAlias { name, target })
                    if name == "e" && target == "eza"
            )
        }),
        "expected the re-rendered config to still list the configured alias"
    );
}
