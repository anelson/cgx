//! Tests for pre-built binary resolution behavior with explicit configuration.
//!
//! These tests explicitly set --prebuilt-binary flags to test non-default behavior,
//! disqualification scenarios, cache interactions, and config overrides.

use assert_fs::prelude::*;
use cgx::messages::{
    BuildCacheMessage, BuildMessage, CrateResolutionMessage, Message, PrebuiltBinaryMessage, SourceMessage,
};
use cgx_core::config::BinaryProvider;

use crate::utils::{
    Cgx, CommandExt, assert_built_from_source, assert_cached_source_build, assert_compiled_from_source,
    assert_prebuilt,
};

/// Test that `--prebuilt-binary never` forces building from source even when binaries exist.
#[test]
fn never_mode_forces_source_build() {
    let mut cgx = Cgx::with_test_fs();

    // eza has pre-built binaries, but we force source build
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("never")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    // Verify prebuilt binaries were disabled and the binary was built from source.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PrebuiltBinariesDisabled)
        )),
        "Expected PrebuiltBinaryMessage::PrebuiltBinariesDisabled"
    );
    assert_built_from_source(&messages);
}

/// Test that `--prebuilt-binary always` succeeds when a binary is available.
#[test]
fn always_mode_succeeds_with_available_binary() {
    let mut cgx = Cgx::with_test_fs();

    // eza has pre-built binaries
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    // Verify a prebuilt binary was resolved and that the binary came from it (no source build).
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved"
    );
    assert_prebuilt(&messages);
}

/// Test that `--prebuilt-binary always` fails when no binary is available.
#[test]
fn always_mode_fails_without_binary() {
    let mut cgx = Cgx::with_test_fs();

    // cargo-expand doesn't publish pre-built binaries, so this should error
    cgx.cmd
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("cargo-expand@=1.0.88")
        .arg("--version")
        .assert()
        .failure();
}

/// Test that `--prebuilt-binary always` fails fast when build options disqualify prebuilt
/// binaries, and does NOT fall back to building from source.
#[test]
fn always_mode_fails_when_build_options_disqualify() {
    let mut cgx = Cgx::with_test_fs();

    // A custom profile disqualifies prebuilt binaries, which is not compatible with `always` mode,
    // so this should error without attempting to build from source.
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--profile")
        .arg("dev")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert
        .failure()
        .stderr(predicates::str::contains("custom profile specified"));

    // The invocation must fail before any source build starts.
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "expected the invocation to fail without starting a build"
    );
}

/// Test that custom features disqualify pre-built binary usage.
#[test]
fn custom_features_disqualifies() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--features")
        .arg("vendored-openssl")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    // Should build from source due to custom features
    assert.success();

    // Verify disqualification and that the binary was built from source.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );
    assert_built_from_source(&messages);
}

/// Test that `--all-features` disqualifies pre-built binary usage.
#[test]
fn all_features_disqualifies() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--all-features")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    // Should build from source due to --all-features
    assert.success();

    // Verify disqualification and that the binary was built from source.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );
    assert_built_from_source(&messages);
}

/// Test that `--no-default-features` disqualifies pre-built binary usage.
#[test]
fn no_default_features_disqualifies() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-default-features")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    // Should build from source due to --no-default-features
    assert.success();

    // Verify disqualification and that the binary was built from source.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );
    assert_built_from_source(&messages);
}

/// Test that `default-features = false` configured for a tool in `[tools]` disqualifies pre-built
/// binary usage, the same as the `--no-default-features` CLI flag.
#[test]
fn config_default_features_false_disqualifies() {
    let mut cgx = Cgx::with_test_fs();

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

    // No CLI feature flags; just use the configured `[tools]` options
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // The configured `default-features = false` disqualifies the pre-built binary...
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );

    // ...so the binary is built from source.
    assert_built_from_source(&messages);
}

/// Test cache flow: default (binary) → never (source) → default (binary from cache).
#[test]
fn cache_flow_switching_modes() {
    let mut cgx = Cgx::with_test_fs();

    // First run with defaults - should use pre-built binary
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // Verify a prebuilt binary was resolved and used (no source build) on the first run.
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved on first run"
    );
    assert_prebuilt(&messages);

    // Second run with --prebuilt-binary never - should build from source
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("never")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // Verify prebuilt binaries were disabled and the binary was built from source on the second run.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PrebuiltBinariesDisabled)
        )),
        "Expected PrebuiltBinaryMessage::PrebuiltBinariesDisabled on second run"
    );
    assert_compiled_from_source(&messages);

    // Third run with defaults again - should use pre-built binary from cache (no network)
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stderr(predicates::str::is_empty());

    // Verify we hit the binary resolution cache and reused the prebuilt binary on the third run.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PositiveCacheHit { .. })
        )),
        "Expected PrebuiltBinaryMessage::PositiveCacheHit on third run"
    );
    assert_prebuilt(&messages);
}

/// Test that custom features and default settings use different cache entries.
#[test]
fn custom_features_uses_separate_cache() {
    let mut cgx = Cgx::with_test_fs();

    // First run with custom features - builds from source
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--features")
        .arg("vendored-openssl")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // Verify disqualification, a source build, and a build-cache miss on the first run.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization on first run"
    );
    assert_compiled_from_source(&messages);
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheMiss { .. }))),
        "Expected BuildCacheMessage::CacheMiss on first run"
    );

    // Second run with defaults - should use pre-built binary (different cache entry)
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // Verify a prebuilt binary was resolved and used on the second run (proves a different cache
    // entry from the first run's source build).
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved on second run (different cache entry from first run)"
    );
    assert_prebuilt(&messages);

    // Third run with custom features again - should use cached build from first run
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--features")
        .arg("vendored-openssl")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stderr(predicates::str::is_empty());

    // Verify we hit the compiled binary cache from the first run (still a source-built binary).
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheHit { .. }))),
        "Expected BuildCacheMessage::CacheHit on third run (reusing build from first run)"
    );
    assert_cached_source_build(&messages);
}

/// Test that negative binary resolution results are cached.
#[test]
fn negative_cache_persists() {
    let mut cgx = Cgx::with_test_fs();

    // First run - checks providers, finds no binary, caches negative result
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("cargo-expand@=1.0.88")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    // Should see binary resolution cache miss on first run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CacheMiss { .. })
        )),
        "Expected PrebuiltBinary::CacheMiss on first run"
    );

    // Second run - should use cached negative result (no provider checks)
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("cargo-expand@=1.0.88")
        .assert_with_messages();

    assert.success();

    // Should see binary resolution cache lookup on second run
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CacheLookup { .. })
        )),
        "Expected PrebuiltBinary::CacheLookup on second run"
    );

    // The second run should be a negative cache hit (we previously determined no binary exists)
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::NegativeCacheHit { .. })
        )),
        "Expected PrebuiltBinary::NegativeCacheHit on second run"
    );

    // Should NOT see provider checking messages (proves we used the cache)
    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { .. })
        )),
        "Should not check providers on second run (negative result cached)"
    );
}

/// Test that --refresh bypasses binary resolution cache.
#[test]
fn refresh_bypasses_binary_cache() {
    let mut cgx = Cgx::with_test_fs();

    // First run - caches result
    cgx.cmd.arg("eza@=0.23.1").arg("--version").assert().success();

    // Second run with --refresh - should re-check providers (bypassing cache entirely)
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--refresh")
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert_with_messages();

    assert.success();

    // Refresh mode bypasses the binary cache entirely (no lookup/miss messages),
    // so we verify that providers are re-checked
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { .. })
        )),
        "Expected CheckingProvider on refresh (proves cache was bypassed)"
    );
}

/// Test that `--prebuilt-binary-sources binstall` resolves via the Binstall provider only.
///
/// Uses git-gamble because it has binstall metadata with an override for x86_64-unknown-linux-gnu.
/// The `--no-exec` flag is required because git-gamble's pre-built binary is linked against NixOS
/// glibc and won't execute on non-Nix systems.
///
/// Gated to `target_env = "gnu"` because git-gamble's binstall `pkg-url` override only covers
/// `x86_64-unknown-linux-gnu`, not musl.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
fn binstall_provider_resolves_binary() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("binstall")
        .arg("--no-exec")
        .arg("git-gamble@=2.11.0")
        .assert_with_messages();

    assert.success();

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers.iter().all(|p| *p == BinaryProvider::Binstall),
        "Expected only Binstall provider, got: {:?}",
        providers
    );

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::Binstall);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that `--prebuilt-binary-sources github-releases` resolves via the GitHub provider only.
///
/// eza version 0.23.1 published GitHub release assets for Linux GNU, x86_64 Linux musl, and
/// `x86_64-pc-windows-gnu` only (no macOS, no windows-msvc, no aarch64 Linux musl).
#[test]
#[cfg(any(
    all(target_os = "linux", not(all(target_arch = "aarch64", target_env = "musl"))),
    all(target_os = "windows", target_env = "gnu")
))]
fn github_provider_resolves_binary() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers.iter().all(|p| *p == BinaryProvider::GithubReleases),
        "Expected only GithubReleases provider, got: {:?}",
        providers
    );

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::GithubReleases);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Regression test for #206: `taplo-cli` resolves its prebuilt binary from GitHub releases.
///
/// This exercises all three of the asset-matching heuristics the fix added: the asset is named after
/// the `taplo` binary (not the `taplo-cli` crate), uses a short `{os}-{arch}` platform token
/// (`taplo-linux-x86_64`, not the full triple), and on Linux/macOS is a bare `.gz` (a gzipped
/// binary, not a tarball). `--no-exec` runs the full download+extract path — so the naked-`.gz`
/// extraction is covered end to end — without executing the binary, sidestepping any glibc/musl
/// run-host mismatch. Gated to the OS/arch combinations for which taplo publishes an asset that the
/// `{os}-{arch}` alias matches.
#[test]
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn github_provider_resolves_taplo_cli_via_binary_name() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases")
        .arg("--no-exec")
        .arg("taplo-cli@=0.10.0")
        .assert_with_messages();

    // `--no-exec` prints the resolved binary path; it is named after the `taplo` binary target.
    assert.success().stdout(predicates::str::contains("taplo"));

    let binary = messages
        .iter()
        .find_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
            _ => None,
        })
        .expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::GithubReleases);

    assert_prebuilt(&messages);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that GitHub Releases correctly reports no eza binary for aarch64 Linux musl.
#[test]
#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "musl"))]
fn github_provider_reports_no_aarch64_musl_binary() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.failure();

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers.iter().all(|p| *p == BinaryProvider::GithubReleases),
        "Expected only GithubReleases provider, got: {:?}",
        providers
    );

    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::ProviderHasNoBinary {
                provider: BinaryProvider::GithubReleases,
                ..
            })
        )),
        "Expected ProviderHasNoBinary for GithubReleases"
    );

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Should not have Resolved for aarch64 Linux musl"
    );

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when prebuilt binaries are required"
    );
}

/// Test that `.tgz` archive suffix is matched by the GitHub provider.
///
/// cargo-binstall publishes Linux releases as `.tgz` files, exercising the `.tgz` candidate
/// generation added alongside `.tar.gz`.
#[test]
#[cfg(target_os = "linux")]
fn github_provider_resolves_tgz_binary() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases")
        .arg("--no-exec")
        .arg("cargo-binstall@=1.14.0")
        .assert_with_messages();

    assert.success();

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::GithubReleases);
}

/// Test that `--prebuilt-binary-sources quickinstall` resolves via the Quickinstall provider only.
///
/// Excluded on `x86_64-pc-windows-gnu` because cargo-quickinstall does not publish binaries for
/// that target (only `x86_64-pc-windows-msvc`).
#[cfg(not(all(target_os = "windows", target_env = "gnu")))]
#[test]
fn quickinstall_provider_resolves_binary() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("quickinstall")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers.iter().all(|p| *p == BinaryProvider::Quickinstall),
        "Expected only Quickinstall provider, got: {:?}",
        providers
    );

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::Quickinstall);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that `--prebuilt-binary always --prebuilt-binary-sources gitlab-releases` fails for a
/// GitHub-hosted crate, proving the sources flag restricts which providers are tried.
#[test]
fn gitlab_only_fails_for_github_crate() {
    let mut cgx = Cgx::with_test_fs();

    cgx.cmd
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("gitlab-releases")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert()
        .failure();
}

/// Test that `--prebuilt-binary-sources gitlab-releases` in auto mode reports no binary from
/// GitLab for a GitHub-hosted crate, then falls back to source build.
#[test]
fn gitlab_provider_reports_no_binary_for_github_crate() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary-sources")
        .arg("gitlab-releases")
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success();

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers.iter().all(|p| *p == BinaryProvider::GitlabReleases),
        "Expected only GitlabReleases provider, got: {:?}",
        providers
    );

    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::ProviderHasNoBinary {
                provider: BinaryProvider::GitlabReleases,
                ..
            })
        )),
        "Expected ProviderHasNoBinary for GitlabReleases"
    );

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Should not have Resolved when GitLab provider can't find a GitHub crate"
    );

    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started as fallback to source build"
    );
}

/// Test that `--prebuilt-binary-sources` with multiple providers restricts to only those providers.
#[test]
fn sources_flag_restricts_to_specified_providers() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases,quickinstall")
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert_with_messages();

    assert.success();

    let providers: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { provider, .. }) => {
                Some(*provider)
            }
            _ => None,
        })
        .collect();
    assert!(
        !providers.is_empty(),
        "Expected at least one CheckingProvider message"
    );
    assert!(
        providers
            .iter()
            .all(|p| *p == BinaryProvider::GithubReleases || *p == BinaryProvider::Quickinstall),
        "Expected only GithubReleases or Quickinstall providers, got: {:?}",
        providers
    );

    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved"
    );
}

/// Test that the default provider order resolves git-gamble via Binstall (the first provider
/// tried), since git-gamble has binstall metadata with a `pkg-url` override for
/// `x86_64-unknown-linux-gnu`.
///
/// Gated to `target_env = "gnu"` because git-gamble's binstall `pkg-url` override only covers
/// `x86_64-unknown-linux-gnu`, not musl.
#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
fn default_resolves_via_binstall() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("git-gamble@=2.11.0")
        .assert_with_messages();

    assert.success();

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::Binstall);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that the default provider order resolves eza via GitHub Releases. Binstall is tried
/// first but fails (no binstall metadata), then GitHub Releases finds a matching release asset.
///
/// eza 0.23.1 published GitHub release assets for Linux GNU, x86_64 Linux musl, and
/// `x86_64-pc-windows-gnu` only (no macOS, no windows-msvc, no aarch64 Linux musl). On
/// aarch64 Linux musl, default resolution falls through to Quickinstall.
#[test]
#[cfg(any(
    all(target_os = "linux", not(all(target_arch = "aarch64", target_env = "musl"))),
    all(target_os = "windows", target_env = "gnu")
))]
fn default_resolves_via_github() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert_with_messages();

    assert.success();

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::GithubReleases);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that the default provider order falls through to Quickinstall for eza on aarch64 Linux
/// musl, where eza has no matching GitHub Releases asset.
#[test]
#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "musl"))]
fn default_resolves_via_quickinstall_on_aarch64_musl() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("eza@=0.23.1")
        .assert_with_messages();

    assert.success();

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::Quickinstall);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Test that the second invocation of a prebuilt binary is fully cached at every layer.
///
/// This is the prebuilt-binary equivalent of `run_from_git_source_with_tag()` in `git_sources.rs`:
/// invoke once to populate all caches, invoke again and assert that every cache layer reports a
/// hit AND that every network/download/build indicator is absent.
#[test]
fn prebuilt_binary_second_invocation_fully_cached() {
    let mut cgx = Cgx::with_test_fs();

    // First run: resolve and download a prebuilt binary
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("eza"));

    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved on first run"
    );

    // Second run: everything should come from cache
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert
        .success()
        .stdout(predicates::str::contains("eza"))
        .stderr(predicates::str::is_empty());

    // --- Positive cache hits: what SHOULD be present ---

    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheHit { .. })
        )),
        "Expected CrateResolutionMessage::CacheHit: proves crate resolution was served from cache"
    );

    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Source(SourceMessage::CacheHit { .. }))),
        "Expected SourceMessage::CacheHit: proves source download was served from cache"
    );

    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PositiveCacheHit { .. })
        )),
        "Expected PrebuiltBinaryMessage::PositiveCacheHit: proves binary resolution was served from cache"
    );

    // --- Absence of network activity: what MUST NOT be present ---

    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::Resolving { .. })
        )),
        "Should not see CrateResolutionMessage::Resolving: proves no registry/index lookup"
    );

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Source(SourceMessage::Downloading { .. }))),
        "Should not see SourceMessage::Downloading: proves no source code download"
    );

    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::CheckingProvider { .. })
        )),
        "Should not see PrebuiltBinaryMessage::CheckingProvider: proves no provider HTTP probing"
    );

    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DownloadingBinary { .. })
        )),
        "Should not see PrebuiltBinaryMessage::DownloadingBinary: proves no binary download"
    );

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not see BuildMessage::Started: proves no compilation"
    );
}

/// Test that the default provider order resolves tokei via Quickinstall. Binstall fails (no
/// metadata), GitHub fails (release exists but has no binary assets), GitLab fails (not on
/// GitLab), and Quickinstall is the fallback that succeeds.
///
/// Excluded on `x86_64-pc-windows-gnu` because cargo-quickinstall does not publish binaries for
/// that target, so the fallback chain cannot resolve via quickinstall.
#[cfg(not(all(target_os = "windows", target_env = "gnu")))]
#[test]
fn default_resolves_via_quickinstall() {
    let mut cgx = Cgx::with_test_fs();

    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--no-exec")
        .arg("tokei@=14.0.0")
        .assert_with_messages();

    assert.success();

    let resolved = messages.iter().find_map(|m| match m {
        Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
        _ => None,
    });
    let binary = resolved.expect("Expected PrebuiltBinaryMessage::Resolved");
    assert_eq!(binary.provider, BinaryProvider::Quickinstall);

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
}

/// Live regression test for the transient-failure caching bug (filed alongside the #206 taplo fix):
/// a GitHub rate-limit (throttle) during prebuilt-binary resolution must NOT be cached as a
/// negative, so a later authenticated run still resolves the prebuilt binary instead of being stuck
/// with a poisoned "no binary" answer.
///
/// This is the automated form of the manual check that initially discovered the bug. It is
/// `#[ignore]` by default because it deliberately exhausts GitHub's unauthenticated per-IP rate
/// limit and requires a real `GITHUB_TOKEN` in the environment. Run it explicitly (re-running
/// needs the hourly limit to have reset):
///
/// ```sh
/// GITHUB_TOKEN=$(gh auth token) \
///   cargo test -p cgx --test integration -- --ignored transient_throttle_is_not_cached_as_negative
/// ```
#[test]
#[ignore = "exhausts GitHub's unauthenticated rate limit and requires a real GITHUB_TOKEN; run manually"]
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn transient_throttle_is_not_cached_as_negative() {
    let token = std::env::var("GITHUB_TOKEN")
        .expect("this test requires a real GITHUB_TOKEN to prove the non-caching behavior");

    // Provoke a throttle: run unauthenticated against a fresh cache until GitHub rate-limits us.
    // taplo-cli@0.10.0 publishes a GitHub binary, so an un-throttled unauthenticated run *succeeds*
    // (and we discard its cache); the first run that *fails* is the throttled one, whose cache we
    // keep for the verification step. `--prebuilt-binary always` makes a throttle surface as a
    // non-zero exit (PrebuiltBinaryResolutionFailed) rather than a slow fall-back to a source build.
    const MAX_WARMUP_RUNS: usize = 80;
    let mut throttled = None;
    for _ in 0..MAX_WARMUP_RUNS {
        let mut cgx = Cgx::with_test_fs();
        let assert = cgx
            .cmd
            .env_remove("GITHUB_TOKEN")
            .arg("--prebuilt-binary")
            .arg("always")
            .arg("--prebuilt-binary-sources")
            .arg("github-releases")
            .arg("--no-exec")
            .arg("taplo-cli@=0.10.0")
            .assert();
        let output = assert.get_output();
        if output.status.success() {
            // Not throttled yet; discard this cache and keep warming up.
            continue;
        }

        // Confirm we actually hit the inconclusive (throttle) path, not some other failure: the
        // `always`-mode inconclusive error is PrebuiltBinaryResolutionFailed.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("resolution could not be completed"),
            "expected an inconclusive resolution failure (throttle), got stderr: {stderr}"
        );
        throttled = Some(cgx);
        break;
    }

    let throttled = throttled
        .expect("GitHub never rate-limited us within the warmup budget; cannot exercise the throttle path");

    // Verify the throttle was not cached as a negative: reuse the throttled run's cache, now WITH a
    // token, and confirm the prebuilt binary resolves. Under the old bug the throttle would have
    // cached a negative and this run would instead fail / fall back to a source build.
    let mut cgx = throttled.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .env("GITHUB_TOKEN", &token)
        .arg("--prebuilt-binary")
        .arg("always")
        .arg("--prebuilt-binary-sources")
        .arg("github-releases")
        .arg("--no-exec")
        .arg("taplo-cli@=0.10.0")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("taplo"));

    let binary = messages
        .iter()
        .find_map(|m| match m {
            Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { binary }) => Some(binary),
            _ => None,
        })
        .expect("expected PrebuiltBinaryMessage::Resolved after the throttle was not cached");
    assert_eq!(binary.provider, BinaryProvider::GithubReleases);
    assert_prebuilt(&messages);
}
