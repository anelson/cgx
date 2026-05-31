# Building cgx

This repository uses `just` as the project task runner. You can build with raw `cargo` commands, but the recipes in
`Justfile` are the documented interface for local checks because they capture project-specific details and overlap with
the CI workflow.

The local recipes are not a precise mirror of GitHub Actions, of course. CI also runs a platform matrix, runtime linkage
checks, and a few fast paths for Dependabot and release-plz PRs. The `just` recipes are a useful subset of the CI
checks that are fast and convenient enough to justify running locally on every change, or at the very least before
submitting each PR.

Run this to see the available recipes:

```sh
just --list
```

### Build dependencies

Required for ordinary local builds and tests:

- Rust 1.85.1. The pinned toolchain is in `rust-toolchain.toml`; `rustup` should install/select it automatically.
- A native compiler and linker for your platform.
- `git`.
- `just`.
- `pkg-config` and OpenSSL development libraries. `cgx` depends on the `gix` crate, whose current git-over-HTTP stack
  uses the curl/OpenSSL transport.

On Debian/Ubuntu-like systems, that usually means something like:

```sh
sudo apt-get install build-essential git just pkg-config libssl-dev
```

On macOS, install Xcode Command Line Tools and use your package manager for `just` and any missing OpenSSL/pkg-config
pieces. On Windows, use a Rust MSVC toolchain and the matching Visual Studio Build Tools.

Additional tools used by the fuller project recipes:

- `taplo` for TOML formatting checks.
- `cargo-deny` and `cargo-machete` for dependency checks.
- The nightly Rust toolchain for the first pass of `just fmt`.
- `gh` is optional for tests; `just test` uses `gh auth token` as `GITHUB_TOKEN` when available to avoid
  unauthenticated GitHub API limits.
- `curl` for scripts that install the configured cargo-dist version when it is missing or stale.
- Docker is needed for `just xmac-check`.

### Common commands

Build the default workspace member:

```sh
cargo build
```

Build all workspace binaries:

```sh
cargo build --workspace --bins
```

Run all tests:

```sh
just test
```

Run tests for one crate or one test directly with Cargo:

```sh
cargo test -p cgx-core --all-features
cargo test -p cgx-core --all-features test_name
```

Run the main compile/lint/doc check:

```sh
just vibecheck
```

`vibecheck` checks that cargo-dist generated workflows are up to date, then runs workspace `cargo check`, all-feature
`cargo check`, clippy with warnings as errors, and private-item docs.

Format the project:

```sh
just fmt
```

Check formatting without changing files:

```sh
just fmtcheck
```

Run dependency checks:

```sh
just depcheck
```

Run the full pre-commit sweep:

```sh
just precommit
```

`precommit` runs formatting first, then `vibecheck`, dependency checks, and the full test suite. It is intentionally
heavier than the checks you usually want while iterating on a feature, and is only intended to be run before submitting
a PR.

### Platform checks

For a regular change, `just vibecheck` and `just test` usually sufficient local validation. When a change touches
platform-sensitive code, process execution, paths, archive handling, linking, or build/release configuration, also run
the targeted platform checks that make sense for your machine (these use cross-compilation):

```sh
just xwin-check
just xmac-check
```

`xwin-check` installs `cargo-xwin` if needed and checks the Windows MSVC target. `xmac-check` runs a Dockerized
`cargo-zigbuild` environment for the macOS x86_64 target.

Both of these assume that you're running on Linux (or, in the case of `xmac-check`, any platform that can run Docker).

## Release Infrastructure

Releases are driven by release-plz and cargo-dist:

- release-plz opens the version/changelog PR and, after that PR lands, publishes crates and creates the version tag.
- cargo-dist owns the generated release workflow in `.github/workflows/release.yml`.
- `release.yml` and `.github/workflows/dist-dry-run-release.yml` are generated files. Do not edit them directly; run
  `just regen-dist-release` after changing cargo-dist config or release workflow setup.

### Linux musl build targets

The cargo-dist config in `dist-workspace.toml` includes two Linux musl targets:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

Each is built on its own native-architecture Ubuntu runner (`ubuntu-24.04` and `ubuntu-24.04-arm`) with no container.
cargo-dist installs `musl-tools` and runs a plain `cargo build` for the target. OpenSSL and libcurl are statically
vendored for the musl targets (configured in `cgx-core/Cargo.toml`), so the artifacts link everything statically.

Building each `musl` target on a same-architecture runner keeps cargo-dist on its plain `cargo build` logic rather than a
cross-compiler wrapper, which is what lets the build embed the cargo-auditable dependency manifest (because
`cargo-dist` doesn't support running `cargo-auditable` when cross-compiling, perhaps because `cargo-auditable` itself
doesn't, it's not clear).

### Release dry run

Before merging a release-plz PR that is expected to produce a release, run the manual `cargo-dist Dry Run` workflow from
GitHub Actions against the release-plz branch. Leave the `tag` input set to `dry-run`; the workflow refuses real tags.

The dry-run workflow is generated by `just regen-dist-release` using temporary cargo-dist settings, then the generated
GHA workflow is patched to disable the actual release. It uses cargo-dist's computed build matrix and runners, but it
does not create a GitHub Release or upload assets to a release.
