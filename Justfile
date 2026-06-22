# Run all of the tests in all of the crates
#
# If the `gh` CLI is configured, uses that auth token to set GITHUB_TOKEN for tests that need it
# which reduces the chances of tests hitting GitHub API rate limits
[unix]
test:
    #!/usr/bin/env bash
    set -e
    if [ -z "$GITHUB_TOKEN" ] && command -v gh &>/dev/null; then
        export GITHUB_TOKEN="$(gh auth token 2>/dev/null || true)"
    fi
    cargo test --all-features --workspace

[windows]
test:
    #!powershell
    $ErrorActionPreference = "Continue"
    if (-not $env:GITHUB_TOKEN) {
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        if ($gh) {
            $token = & gh auth token 2>$null
            if ($LASTEXITCODE -eq 0 -and $token) { $env:GITHUB_TOKEN = $token }
        }
    }
    cargo test --all-features --workspace
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[unix]
xwin-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    if ! cargo xwin --version >/dev/null 2>&1; then
        cargo install cargo-xwin --version 0.19.2 --locked
    fi
    RUSTFLAGS='-Dwarnings' cargo xwin check --workspace --all-targets --target x86_64-pc-windows-msvc

[unix]
xmac-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    if ! command -v docker >/dev/null 2>&1; then
        echo "docker is required for xmac-check" >&2
        exit 1
    fi
    if ! docker info >/dev/null 2>&1; then
        echo "docker daemon is not reachable for xmac-check" >&2
        exit 1
    fi
    docker run --rm \
      -v "{{justfile_directory()}}:/io" \
      -v cgx-xmac-cargo-registry:/usr/local/cargo/registry \
      -v cgx-xmac-cargo-git:/usr/local/cargo/git \
      -v cgx-xmac-rustup:/usr/local/rustup \
      -v cgx-xmac-cache:/root/.cache \
      -v cgx-xmac-target:/io-target \
      -w /io \
      -e CARGO_TARGET_DIR=/io-target \
      -e XDG_CACHE_HOME=/root/.cache \
      -e CARGO_ZIGBUILD_CACHE_DIR=/root/.cache/cargo-zigbuild \
      ghcr.io/rust-cross/cargo-zigbuild@sha256:dce4ea213244423439d97a2070031c6ea287fc32f01b0aaa38f8b4d46f52e68c \
      bash -lc '
        export PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
        export XDG_CACHE_HOME=/root/.cache
        export CARGO_ZIGBUILD_CACHE_DIR=/root/.cache/cargo-zigbuild
        export XMAC_OPENSSL_INCLUDE=/root/.cache/xmac-openssl/include

        cleanup() {
          rm -f /io/.intentionally-empty-file.o \
                /io/.intentionally-empty-file.c \
                /io/.intentionally-empty-file.cpp
        }

        cleanup
        trap cleanup EXIT

        rm -rf "$XMAC_OPENSSL_INCLUDE"
        mkdir -p "$XMAC_OPENSSL_INCLUDE"
        cp -a /usr/include/openssl "$XMAC_OPENSSL_INCLUDE"/
        cp -a /usr/include/x86_64-linux-gnu/openssl/. "$XMAC_OPENSSL_INCLUDE"/openssl/
        export X86_64_APPLE_DARWIN_OPENSSL_INCLUDE_DIR="$XMAC_OPENSSL_INCLUDE"
        export X86_64_APPLE_DARWIN_OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu

        if ! rustup target list --installed | grep -qx x86_64-apple-darwin; then
          rustup target add x86_64-apple-darwin
        fi

        RUSTFLAGS="-Dwarnings" cargo-zigbuild check --workspace --all-targets --target x86_64-apple-darwin
      '

# Regenerate cargo-dist release workflows, including the dry-run workflow.
[unix]
regen-dist-release:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    .github/scripts/regen-dist-release.sh

# Check that cargo-dist generated release workflows are up to date.
[unix]
check-dist-release-generated:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    .github/scripts/check-dist-release-generated.sh

# Format the entire project with beautifiers
fmt:
    # (Ab)use nightly rustfmt features to correct some annoying rustfmt issues,
    # and then run the stable rustfmt after that which will apply the standard rust formatting.
    #
    # This isn't as ridiculous or wasteful as it sounds.  The nightly fmt fails on overflow lines which helps
    # catch cases when lines are too long to format, and does some other formatting that the stable rustfmt doesn't to.
    # Once these are done, running stable rustfmt doesn't undo them
    cargo +nightly fmt -- --config-path rustfmt-nightly.toml
    cargo fmt
    taplo fmt

# Verify that the code is properly formatted, but unlike `fmt` instead of applying formatting changes,
# fails with an error if files are not properly formatted.
#
# This is mainly useful for CI and precommit checks
fmtcheck:
    # NOTE: We can't use the dual fmt config hack here.  We expect the code to pass a stable rustfmt check.
    cargo fmt --check
    taplo fmt --check

# Rules live in `.ast-grep/rules`; their self-tests live in `.ast-grep/rule-tests`. Assumes the `ast-grep` binary is
# installed, like `just` and `taplo`.
# Run the ast-grep structural lints that clippy cannot express (e.g. `use` must be at module scope) plus rule self-tests
ast-grep:
    ast-grep test --skip-snapshot-tests
    ast-grep scan

# Spell-check source, comments, docs, and text files.
# Config and allow-list overrides go in `typos.toml`.
typos:
    typos

# Do a Rust "vibe check" (*cringe*) on the codebase
# This is helpful for humans but it's mainly intended to provide a deterministic way for coding agents
# to get feedback on their almost certainly shitty changes before wasting a human's time with their garbage code.
# Run the generated workflow check plus Rust compile, clippy, and docs checks.
vibecheck: check-dist-release-generated ast-grep typos
    cargo check --all-targets --workspace
    cargo check --all-targets --all-features --workspace
    cargo clippy --all-targets --all-features --workspace -- -D warnings
    cargo doc --workspace --no-deps --document-private-items

# Check dependencies, looking for security vulns, unused dependencies, and duplicates
depcheck:
    cargo deny check
    cargo machete --with-metadata -- ./Cargo.toml

# Wrapper around `cargo add` that adds a dependency to the workspace according to our standards
wadd +args:
    #!/usr/bin/env bash
    set -e
    # Check if we have a workspace by looking for [workspace] in Cargo.toml
    if grep -q "^\[workspace\]" Cargo.toml 2>/dev/null; then
        # We have a workspace, use the full workflow
        if ! command -v cargo-autoinherit &> /dev/null; then
            echo "Installing cargo-autoinherit..."
            cargo install cargo-autoinherit --locked
        fi
        cargo add {{args}}
        cargo autoinherit
    else
        # No workspace yet, just use cargo add directly
        cargo add {{args}}
    fi

precommit: fmt vibecheck depcheck test

build: vibecheck fmt
    cargo build
