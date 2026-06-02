//! Integration tests for git source handling
//!
//! These tests verify that cgx correctly handles git sources, including fetching,
//! checking out, building, and caching.

use cgx::messages::{BuildCacheMessage, CrateResolutionMessage, GitMessage, GitSelector, Message};

use crate::utils::{Cgx, CommandExt};

/// Test running a crate from a git source with a specific tag.
///
/// Verifies:
/// - Git source resolution and checkout work correctly
/// - The crate builds and runs successfully
/// - Correct git messages are emitted during the flow
/// - Git checkout caching works on second run
#[test]
fn run_from_git_source_with_tag() {
    let mut cgx = Cgx::with_test_fs();

    // First run - cold cache, should fetch and checkout
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("never")
        .arg("--github")
        .arg("cargo-bins/cargo-binstall")
        .arg("--tag")
        .arg("v1.14.0")
        .arg("cargo-binstall")
        .arg("-V")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("1.14.0"));

    // --- First run assertions: verify cold cache behavior ---
    //
    // NOTE: There are TWO git checkout sequences in a single run:
    // 1. Crate resolution: Tag("v1.14.0") → resolves to commit hash
    // 2. Build phase: Commit("<hash>") → gets source for building
    //
    // The second operation will show local hits because the first operation
    // already populated the bare repo. We test the TAG-based operations to verify
    // cold cache behavior.

    // ResolvingRef with Tag selector proves: About to check if tag exists locally
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::ResolvingRef {
                selector: GitSelector::Tag(_),
                ..
            })
        )),
        "Expected ResolvingRef with Tag: proves we checked for the tag locally"
    );

    // FetchingRepo with Tag selector proves: The ref wasn't present locally
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::FetchingRepo {
                selector: GitSelector::Tag(_),
                ..
            })
        )),
        "Expected FetchingRepo with Tag: proves the tag was NOT present locally"
    );

    // ResolvedRef proves: The tag was successfully resolved to a commit after fetching
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::ResolvedRef { .. }))),
        "Expected ResolvedRef: proves the tag was resolved to a commit hash after fetching"
    );

    // NO RefFoundLocally with Tag proves: The tag wasn't present locally (had to fetch)
    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::RefFoundLocally {
                selector: GitSelector::Tag(_),
                ..
            })
        )),
        "Should not see RefFoundLocally with Tag: proves the tag was not present locally"
    );

    // CheckingOut proves: The checkout didn't exist, so cgx had to extract the working tree
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::CheckingOut { .. }))),
        "Expected CheckingOut: proves working tree was extracted from bare repo"
    );

    // CheckoutComplete proves: Extraction finished successfully
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::CheckoutComplete { .. }))),
        "Expected CheckoutComplete: proves extraction finished"
    );

    // --- Second run: warm cache at all levels ---
    let mut cgx = cgx.reset();
    let (assert, messages) = cgx
        .cmd
        .with_json_messages()
        .arg("--prebuilt-binary")
        .arg("never")
        .arg("--github")
        .arg("cargo-bins/cargo-binstall")
        .arg("--tag")
        .arg("v1.14.0")
        .arg("cargo-binstall")
        .arg("-V")
        .assert_with_messages();

    assert.success().stdout(predicates::str::contains("1.14.0"));

    // --- Second run assertions: verify multi-level caching ---
    //
    // On the second run, there are THREE levels of caching at play:
    // - Crate resolution cache: The Tag→commit mapping is cached, so NO git operations happen at all
    //   for the crate resolution phase (no Tag-based ResolvingRef).
    // - Git: The build phase does a Commit-based lookup, which finds it locally.
    // - Binary cache: The compiled binary is cached, so no actual build happens.

    // CrateResolution CacheHit proves: The Tag→commit resolution was served from cache,
    // completely bypassing git operations for the crate resolution phase.
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::CrateResolution(CrateResolutionMessage::CacheHit { .. })
        )),
        "Expected CrateResolution CacheHit: proves crate resolution was served from cache"
    );

    // NO Tag-based git operations proves: Crate resolution cache hit means we never
    // needed to consult the git repo for the Tag at all.
    assert!(
        !messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::ResolvingRef {
                selector: GitSelector::Tag(_),
                ..
            })
        )),
        "Should not see ResolvingRef with Tag: proves crate resolution was fully cached"
    );

    // Git operations ARE present, but only with Commit selector (for build phase)
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::ResolvingRef {
                selector: GitSelector::Commit(_),
                ..
            })
        )),
        "Expected ResolvingRef with Commit: build phase still checks for commit"
    );

    // RefFoundLocally proves: Build phase found the commit in the local bare repo
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Git(GitMessage::RefFoundLocally {
                selector: GitSelector::Commit(_),
                ..
            })
        )),
        "Expected RefFoundLocally with Commit: proves commit was found locally"
    );

    // NO FetchingRepo proves: Nothing was fetched (commit was already present)
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::FetchingRepo { .. }))),
        "Should not see FetchingRepo: proves no network access was needed"
    );

    // CheckoutExists proves: Build phase found the working tree already present
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::CheckoutExists { .. }))),
        "Expected CheckoutExists: proves working tree was already present"
    );

    // No CheckingOut proves: No extraction was needed (checkout already existed)
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Git(GitMessage::CheckingOut { .. }))),
        "Should not see CheckingOut: proves no extraction was needed"
    );

    // Binary CacheHit proves: The compiled binary was served from cache
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheHit { .. }))),
        "Expected BuildCache CacheHit: proves compiled binary was served from cache"
    );
}
