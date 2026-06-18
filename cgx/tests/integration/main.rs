#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration-test helper fns may unwrap/expect freely; this is entirely test code so test \
              conventions should apply"
)]

mod basic;
mod cargo;
mod config;
mod defaults;
mod git_sources;
mod prebuilt_binaries;
mod utils;
