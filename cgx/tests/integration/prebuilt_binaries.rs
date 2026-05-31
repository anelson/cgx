//! Tests for pre-built binary resolution behavior with explicit configuration.
//!
//! These tests explicitly set --prebuilt-binary flags to test non-default behavior,
//! disqualification scenarios, cache interactions, and config overrides.

use cgx::messages::{
    BuildCacheMessage, BuildMessage, CrateResolutionMessage, Message, PrebuiltBinaryMessage, SourceMessage,
};
use cgx_core::config::BinaryProvider;
use predicates::prelude::*;

use crate::utils::{Cgx, CommandExt};

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

    assert
        .success()
        .stdout(predicates::str::contains("eza"))
        .stderr(predicates::str::contains("Compiling"));

    // Verify prebuilt binaries were disabled
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PrebuiltBinariesDisabled)
        )),
        "Expected PrebuiltBinaryMessage::PrebuiltBinariesDisabled"
    );

    // Verify build was initiated
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started"
    );
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

    assert
        .success()
        .stdout(predicates::str::contains("eza"))
        .stderr(predicates::str::contains("Compiling").not());

    // Verify prebuilt binary was resolved
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved"
    );

    // Verify no build was initiated
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started when using prebuilt binary"
    );
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
    assert.success().stderr(predicates::str::contains("Compiling"));

    // Verify disqualification message
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );

    // Verify build was initiated
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started"
    );
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
    assert.success().stderr(predicates::str::contains("Compiling"));

    // Verify disqualification message
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );

    // Verify build was initiated
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started"
    );
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
    assert.success().stderr(predicates::str::contains("Compiling"));

    // Verify disqualification message
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization"
    );

    // Verify build was initiated
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started"
    );
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

    assert
        .success()
        .stderr(predicates::str::contains("Compiling").not());

    // Verify prebuilt binary was resolved
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved on first run"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started on first run (using prebuilt binary)"
    );

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

    assert.success().stderr(predicates::str::contains("Compiling"));

    // Verify prebuilt binaries were disabled and build was initiated
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::PrebuiltBinariesDisabled)
        )),
        "Expected PrebuiltBinaryMessage::PrebuiltBinariesDisabled on second run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started on second run"
    );

    // Third run with defaults again - should use pre-built binary from cache (no network)
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("eza@=0.23.1")
        .arg("--version")
        .assert_with_messages();

    assert.success().stderr(predicates::str::is_empty());

    // Verify we hit the binary resolution cache
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::CacheHit { .. }))),
        "Expected PrebuiltBinaryMessage::CacheHit on third run"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started on third run (using cached prebuilt binary)"
    );
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

    assert.success().stderr(predicates::str::contains("Compiling"));

    // Verify disqualification and source build
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::PrebuiltBinary(PrebuiltBinaryMessage::DisqualifiedDueToCustomization { .. })
        )),
        "Expected PrebuiltBinaryMessage::DisqualifiedDueToCustomization on first run"
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Expected BuildMessage::Started on first run"
    );
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

    assert
        .success()
        .stderr(predicates::str::contains("Compiling").not());

    // Verify prebuilt binary was resolved (proves different cache entry from source build)
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::Resolved { .. }))),
        "Expected PrebuiltBinaryMessage::Resolved on second run (different cache entry from first run)"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started on second run (using prebuilt binary)"
    );

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

    // Verify we hit the compiled binary cache (from first run with custom features)
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheHit { .. }))),
        "Expected BuildCacheMessage::CacheHit on third run (reusing build from first run)"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "Should not have BuildMessage::Started on third run (using cached build)"
    );
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

    assert
        .success()
        .stdout(predicates::str::contains("eza"))
        .stderr(predicates::str::contains("Compiling").not());

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

    assert
        .success()
        .stdout(predicates::str::contains("eza"))
        .stderr(predicates::str::contains("Compiling").not());

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

    assert.success().stderr(predicates::str::contains("Compiling"));

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
        messages
            .iter()
            .any(|m| matches!(m, Message::PrebuiltBinary(PrebuiltBinaryMessage::CacheHit { .. }))),
        "Expected PrebuiltBinaryMessage::CacheHit: proves binary resolution was served from cache"
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
