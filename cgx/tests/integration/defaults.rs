//! Tests that do not override any config settings and verify the default behavior of cgx
use cgx::messages::{
    BuildCacheMessage, CrateResolutionMessage, Message, PrebuiltBinaryMessage, RunnerMessage, SourceMessage,
};
use predicates::prelude::*;

use crate::utils::{Cgx, CommandExt, assert_compiled_from_source, assert_prebuilt};

/// Test running a crate that publishes pre-built binaries (eza).
///
/// With default settings, cgx should download the pre-built binary instead of building from source.
///
/// ```sh
/// cgx eza@=0.23.1 --version
/// cgx --no-exec eza@=0.23.1
/// ```
#[test]
fn run_with_prebuilt_binary() {
    let mut cgx = Cgx::with_test_fs();

    // First invocation should download and run the eza pre-built binary
    cgx.cmd
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::ord::eq(
            r##"eza - A modern, maintained replacement for ls
v0.23.1 [+git]
https://github.com/eza-community/eza
"##,
        ))
        .stderr(predicates::str::contains("Compiling").not());

    // Second invocation should be served from cache
    let mut cgx = cgx.reset();
    cgx.cmd
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert()
        .success()
        .stdout(predicates::str::starts_with(
            cgx.test_fs_app_root().join("bins").to_string_lossy(),
        ))
        .stdout(predicates::str::contains("eza-0.23.1"))
        .stderr(predicates::str::is_empty());
}

/// Test running a crate that does NOT publish pre-built binaries.
///
/// With default settings, cgx should fall back to building from source.
///
/// ```sh
/// cgx cargo-expand@1.0.88 --version
/// cgx --no-exec cargo-expand@1.0.88
/// ```
#[test]
fn run_without_prebuilt_binary() {
    let mut cgx = Cgx::with_test_fs();

    // First invocation should build from source since cargo-expand doesn't publish binaries.
    cgx.cmd
        .arg("cargo-expand@=1.0.88")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::starts_with("cargo-expand"))
        .stderr(predicates::str::contains("Compiling"));

    // Second invocation should be served from cache
    let mut cgx = cgx.reset();
    cgx.cmd
        .arg("--no-exec")
        .arg("cargo-expand@=1.0.88")
        .assert()
        .success()
        .stdout(predicates::str::starts_with(
            cgx.test_fs_app_root().join("bins").to_string_lossy(),
        ))
        .stdout(predicates::str::contains("cargo-expand"))
        .stderr(predicates::str::is_empty());
}

/// Test message reporting for a crate WITH pre-built binaries (uses default settings).
///
/// Verifies that binary resolution messages are emitted correctly when a pre-built binary is found.
#[test]
fn messages_with_prebuilt_binary() {
    let mut cgx = Cgx::with_test_fs();

    // First invocation should find and download pre-built binary
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    // Verify cache misses on first run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheMiss { .. })
        )),
        "Expected CrateResolution::CacheMiss on first run"
    );
    assert_prebuilt(&messages);

    // Second invocation should hit all caches
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert_with_messages();

    assert.success().stderr(predicates::str::is_empty());

    // Verify cache hits on second run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheHit { .. })
        )),
        "Expected CrateResolution::CacheHit on second run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::CacheHit { .. }))),
        "Expected PrebuiltBinary::CacheHit on second run (pre-built binary cached)"
    );
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Runner(RunnerMessage::ExecutionPlan { no_exec: true, .. })
        )),
        "Expected RunnerMessage::ExecutionPlan"
    );
}

/// Test message reporting for a crate WITHOUT pre-built binaries (uses default settings).
///
/// Verifies that source build messages are emitted when no pre-built binary is available.
#[test]
fn messages_without_prebuilt_binary() {
    let mut cgx = Cgx::with_test_fs();

    // First invocation should build from source (cargo-expand doesn't publish binaries)
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("cargo-expand@=1.0.88")
        .arg("--version")
        .assert_with_messages();

    assert
        .success()
        .stdout(predicates::str::starts_with("cargo-expand"));

    // Verify cache misses on first run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheMiss { .. })
        )),
        "Expected CrateResolution::CacheMiss on first run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Source(SourceMessage::CacheMiss { .. }))),
        "Expected Source::CacheMiss on first run (no prebuilt binary, building from source)"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheMiss { .. }))),
        "Expected BuildCache::CacheMiss on first run (building from source)"
    );
    assert_compiled_from_source(&messages);

    // Second invocation should hit all caches
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("cargo-expand@=1.0.88")
        .assert_with_messages();

    assert.success().stderr(predicates::str::is_empty());

    // Verify cache hits on second run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheHit { .. })
        )),
        "Expected CrateResolution::CacheHit on second run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Source(SourceMessage::CacheHit { .. }))),
        "Expected Source::CacheHit on second run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheHit { .. }))),
        "Expected BuildCache::CacheHit on second run"
    );
}
