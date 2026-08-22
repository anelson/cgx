# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
## [0.1.1] - 2026-08-22

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies
- Update Cargo.lock dependencies

## [0.1.0] - 2026-08-02

### 🚀 Features

- Implement `anelson/cgx` custom GitHub Action to download and install `cgx` (and optionally `cargo-cgx`) in workflows, add it to `PATH` for later workflow steps, cache tool state, and prefetch configured tools ([#207](https://github.com/anelson/cgx/pull/207))
- Fall back to ABI-compatible alternative targets when resolving prebuilt binaries ([#240](https://github.com/anelson/cgx/pull/240))
- Further expand prebuilt binary heuristics ([#242](https://github.com/anelson/cgx/pull/242))

### 🐛 Bug Fixes

- Fix various typos in comments and docs and add `typos` check ([#222](https://github.com/anelson/cgx/pull/222))
- Relative tool paths in config files are now evaluated relative to the config file's location ([#224](https://github.com/anelson/cgx/pull/224))
- Improve prebuilt binary discovery for alternate binary names and target layouts, and avoid reusing cached resolutions when the enabled providers change ([#232](https://github.com/anelson/cgx/pull/232))
- Improve binary provider support so `cgx ripgrep` can resolve a prebuilt binary ([#235](https://github.com/anelson/cgx/pull/235))

### 📚 Documentation

- Document and apply AI policy ([#243](https://github.com/anelson/cgx/pull/243))

### ⚙️ Miscellaneous Tasks

- Update Cargo.lock dependencies
- Fix a typo that slipped in past the recent `typos` commit

### 🛡️ Security

- Enable Github release attestation on release builds and verify attestation by default in the `cgx` custom action ([#252](https://github.com/anelson/cgx/pull/252))
- Pin the GitHub Actions used by the release workflows to commit SHA digests ([#252](https://github.com/anelson/cgx/pull/252))

## [0.0.13] - 2026-06-19

### 🚀 Features

- Add the `anelson/cgx` GitHub Action to download and install `cgx` (and optionally `cargo-cgx`), add it to `PATH` for later workflow steps, cache tool state, and prefetch configured tools ([#207](https://github.com/anelson/cgx/pull/207))

### ⚙️ Miscellaneous Tasks

- Enable several more clippy lints and fix the code accordingly ([#212](https://github.com/anelson/cgx/pull/212))

## [0.0.12] - 2026-06-17

### 🚀 Features

- Add `--prefetch`, `--prefetch-all`, and `--list-tools` for warming caches and preparing configured tools for later offline execution ([#201](https://github.com/anelson/cgx/pull/201))

### 🐛 Bug Fixes

- Handle fresh Cargo home ([#210](https://github.com/anelson/cgx/pull/210))

## [0.0.11] - 2026-06-03

### ⚙️ Miscellaneous Tasks

- Add more complexity to Linux musl builds in the hopes of fixing them ([#178](https://github.com/anelson/cgx/pull/178))
- Update Dependabot config ([#188](https://github.com/anelson/cgx/pull/188))

### 💼 Other

- _(refactor)_ Move from `anelson-labs/cgx` to `anelson/cgx` ([#191](https://github.com/anelson/cgx/pull/191))

### 🛡️ Security

- _(ci)_ Switch to using Trusted Publishing to crates.io ([#192](https://github.com/anelson/cgx/pull/192))

## [0.0.10] - 2026-05-28

### 💼 Other

- _(deps)_ Bump sha2 from 0.10.9 to 0.11.0 ([#150](https://github.com/anelson/cgx/pull/150))

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies

### 🛡️ Security

- _(deps)_ Bump gix from 0.80.0 to 0.83.0 ([#148](https://github.com/anelson/cgx/pull/148))
- _(release)_ Embed auditable dependency SBOMs in published binaries via cargo-auditable ([#166](https://github.com/anelson/cgx/pull/166))

## [0.0.9] - 2026-03-14

### 🚀 Features

- Add ability to resolve pre-built binaries for crates ([#93](https://github.com/anelson/cgx/pull/93))
- _(http)_ Centralize HTTP client with retry, proxy, and timeout support ([#114](https://github.com/anelson/cgx/pull/114))
- _(git)_ Align gix HTTP behavior with cgx HTTP config and document curl runtime deps ([#128](https://github.com/anelson/cgx/pull/128))

### 📚 Documentation

- Fix typo causing messed-up Markdown rendering in README

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies

## [0.0.8] - 2025-11-16

### 🚀 Features

- Add structured message format for detailed operation reporting ([#68](https://github.com/anelson/cgx/pull/68))

### ⚙️ Miscellaneous Tasks

- Configure cargo-dist to exclude cargo-cgx from release text ([#65](https://github.com/anelson/cgx/pull/65))

## [0.0.7] - 2025-11-07

### 🚀 Features

- Add `--refresh` flag to bypass cache ([#64](https://github.com/anelson/cgx/pull/64))

### ⚙️ Miscellaneous Tasks

- Do not try to use `cargo-auditable` when building `cgx` release bins ([#62](https://github.com/anelson/cgx/pull/62))

## [0.0.6] - 2025-11-06

### 🚀 Features

- Add an --unlocked flag, make --locked the default ([#59](https://github.com/anelson/cgx/pull/59))

### ⚙️ Miscellaneous Tasks

- Update Cargo.lock dependencies

## [0.0.5] - 2025-11-04

### 🚜 Refactor

- Make our `insta` snapshot tests of SBOMs more robust

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies

## [0.0.4] - 2025-11-04

### 🚀 Features

- Resolve crates from crates.io, other registries, Git repositories, or local paths, build their selected binaries, and execute them with forwarded arguments
- Add offline-aware caches for crate resolution, source downloads, persistent Git checkouts, and built binaries
- Generate CycloneDX SBOMs for binaries built from source
- Automatically detect GitHub and GitLab repository URLs passed to `--git`
- Add `--no-exec` to print the prepared binary path and `--list-targets` to inspect runnable targets
- Load and merge `cgx.toml` configuration from system, user, and directory scopes ([#18](https://github.com/anelson/cgx/pull/18))
- Add `cargo-cgx` binary crate for cargo subcommand integration ([#51](https://github.com/anelson/cgx/pull/51))
- Honor tool versions in config when resolving crates ([#46](https://github.com/anelson/cgx/pull/46))

### 🐛 Bug Fixes

- Add `cargo-binstall` metadata to Cargo.toml for faster installs
- Fix broken README link in cgx-core/Cargo.toml that blocks release

### 🚜 Refactor

- Factor most logic out into cgx-core library crate ([#41](https://github.com/anelson/cgx/pull/41))

### 📚 Documentation

- Add text in README about instability
- Update README with installation instructions ([#50](https://github.com/anelson/cgx/pull/50))

### 🧪 Testing

- Add integration tests that actually drive the CLI and verify behavior ([#34](https://github.com/anelson/cgx/pull/34))

## [0.0.3] - 2025-10-05

### ⚙️ Miscellaneous Tasks

- Migrate repository namespace
- (Hopefully) get dist working on aarch64
- Try to fix release-plz PR creation using correct token
- Fix release-plz workflow issues
- Trying to fix broken `release-plz release` GHA workflow job

## [0.0.2] - 2025-10-05

### 💼 Other

- Add precommit hook to enforce conventional commits
- Update Rust to 1.85.1
- Configure dependabot to also update GHA actions

### 📚 Documentation

- Add an initial CHANGELOG file
- Remove some unnecessary sections from CHANGELOG.md

### ⚙️ Miscellaneous Tasks

- Introduce highly automated release workflow
- Exclude the `.github/workflows/release.yml` workflow from dependabot
- Fix various formatting issues, mainly TOML

### 🛡️ Security

- _(deps)_ Bump actions/checkout from 4 to 5 ([#5](https://github.com/anelson/cgx/pull/5))
- _(deps)_ Bump extractions/setup-just from 2 to 3 ([#3](https://github.com/anelson/cgx/pull/3))

## [0.0.1] - 2025-10-05

### Added

- Initial release of empty crate as a starting point
