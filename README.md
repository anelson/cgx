# cgx

[![CI](https://github.com/anelson/cgx/actions/workflows/ci.yml/badge.svg)](https://github.com/anelson/cgx/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/cgx?link=https%3A%2F%2Fcrates.io%2Fcrates%2Fcgx)](https://crates.io/crates/cgx)
![license](https://img.shields.io/crates/l/cgx.svg)

Execute Rust crates easily and quickly. Like `uvx` or `npx` for Rust.

`cgx` lets you run Cargo plugins and other Rust binaries without needing to install them first. It does what you would
otherwise do manually with `cargo install`, `cargo binstall`, `cargo update`, and `cargo run-bin`, but in a single
command.

:warning: **NOTE**: `cgx` is still under active development, and is not yet considered stable. :warning:

## Installation

### Quick Install (Recommended)

**macOS and Linux:**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anelson/cgx/releases/latest/download/cgx-installer.sh | sh
```

**Windows:**

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/anelson/cgx/releases/latest/download/cgx-installer.ps1 | iex"
```

The installer will download the appropriate binary for your platform and add it to your PATH.

> **Note:** To install a specific version for CI/reproducible builds, replace `latest` in the URL above with the desired
> version tag from the [Releases page](https://github.com/anelson/cgx/releases), such as `v0.0.10`.

### Alternative Installation Methods

You can also install using Rust tooling:

**Via cargo install:**

```sh
cargo install cgx
```

**Via cargo-binstall (faster, uses pre-built binaries):**

```sh
cargo binstall cgx
```

**Manual download:**

Download prebuilt binaries directly from the [Releases page](https://github.com/anelson/cgx/releases).

---

_Coming soon: Install via `curl https://cgx.sh/install.sh | sh` once the cgx.sh domain is set up._

## Runtime Dependencies

`cgx` uses `gix` for git operations, and for git-over-HTTP `gix` uses a curl/OpenSSL transport backend.

The pre-built Linux musl artifacts are fully static: libcurl, OpenSSL, and zlib are statically linked into the binary, so they add
no runtime library dependencies. Other Linux builds (glibc) dynamically link dependencies like `libcurl`, OpenSSL, and `libz`.

If you run a dynamically linked `cgx` in minimal containers or stripped-down environments, make sure the appropriate
shared libraries are present.

## Quick Start

Run a crate by name:

```sh
# Run ripgrep, installing or updating it if needed
cgx ripgrep --version
```

There's a special case if the first argument is `cargo`, which indicates that you want to run a Cargo subcommand that
may be a third-party Cargo plugin:

```sh
# Run `cargo deny`, installing cargo-deny if it is missing
cgx cargo deny --version
```

Like `npx` and `uvx`, `cgx` requires that its own flags come before the crate name, and any flags intended for the
executed crate come after the crate name:

```sh
# Correct: cgx flags before crate name, crate flags after
# This tells `cgx` to build ripgrep with the `serde` feature, and passes `--color=always` to the ripgrep binary
cgx --features serde ripgrep --color=always

# Wrong: --features is passed to ripgrep and `cgx` will use the default features for ripgrep instead of enabling `serde`
cgx ripgrep --features serde --color=always
```

You can also use `--` as an explicit separator:

```sh
cgx ripgrep -- --version
```

## Version Requirements

The default is to use the latest version of the crate. To choose a subset of releases, use the same version requirement
syntax Cargo supports for dependencies:

```sh
# Run the latest release compatible with major version 14
cgx ripgrep@14

# Run the latest release compatible with 14.1
cgx ripgrep@14.1

# Run exactly 14.1.1
cgx ripgrep@=14.1.1
```

You can also pass the version requirement as a `cgx` option before the crate name:

```sh
cgx --version 14 ripgrep
```

## Sources

By default, `cgx` resolves crates from crates.io. You can point it at other sources:

```sh
# Local crate or workspace
cgx --path ./tools/my-tool

# Git repository containing a Rust crate called `my-tool`, using the code in the `v1.0.0` tag
cgx --git https://github.com/owner/repo.git --tag v1.0.0 my-tool

# GitHub or GitLab shorthand specifying a repo containing a crate called `my-tool` in the default branch
cgx --github owner/repo my-tool
cgx --gitlab owner/repo my-tool

# Named Cargo registry or direct registry index URL, containing a crate `private-tool`
cgx --registry my-registry private-tool
cgx --index https://registry.example.com/index private-tool
```

For git sources, `--branch`, `--tag`, and `--rev` select the git ref. `--github-url` and `--gitlab-url` can
override the default API URLs to point to self-hosted instances of GitHub and GitLab, respectively.

## Build and Execution Controls

`cgx` builds in release mode by default and defaults to locked dependency resolution, unlike `cargo install`. Use these
flags to change how a crate is built or run:

```sh
# Cargo build options
cgx --features serde --no-default-features ripgrep
cgx --all-features ripgrep
cgx --debug ripgrep
cgx --profile release-with-debug ripgrep
cgx --target x86_64-unknown-linux-musl ripgrep
cgx -j 4 ripgrep

# Lockfile and network behavior
cgx --unlocked ripgrep
cgx --frozen ripgrep
cgx --offline ripgrep

# Build or resolve without executing
cgx --no-exec ripgrep
cgx --list-targets ripgrep

# Select a particular executable target from a crate
cgx --bin rg ripgrep
cgx --example demo some-crate

# Ignore cached data for this invocation
cgx --refresh ripgrep
```

`--no-exec` prints the resolved executable path to stdout. `--list-targets` lists the crate's binary and example targets
without building or executing them.

## Configuration Files

One of the handy features of tools like `uvx` and `npx` is that you can pin or customize tools in your workspace. `cgx`
supports this using `cgx.toml` configuration files.

Create a `cgx.toml` file in your project root:

```toml
[tools]
ripgrep = "14.1"
cargo-deny = "=0.17.0"
```

Now, anywhere inside this directory or its subdirectories, `cgx ripgrep` will use a 14.1-compatible ripgrep and
`cgx cargo deny` will use exactly `cargo-deny` 0.17.0.

You can also specify more complex configurations:

```toml
[tools]
# Simple version requirements
ripgrep = "14"
cargo-deny = "=0.17.0"

# Detailed configuration with features
taplo-cli = { version = "1.0", features = ["full"] }

# Git repository source
my-tool = { git = "https://github.com/owner/repo.git", tag = "v1.0.0" }

# Custom registry
private-tool = { version = "1.0", registry = "my-registry" }

[aliases]
# Convenient short names
rg = "ripgrep"
taplo = "taplo-cli"
```

Config files are loaded and merged in order of precedence, with later sources overriding earlier ones:

1. System-wide config (`/etc/cgx.toml` on Linux/macOS, platform equivalent on Windows)
2. User config (`$XDG_CONFIG_HOME/cgx/cgx.toml` or platform equivalent)
3. Directory hierarchy from filesystem root to current directory (each `cgx.toml` found)
4. Command-line arguments and `CGX_*` environment-backed CLI options

Use `--config-file <FILE>` to read only one config file and bypass the normal search. See
[`cgx-example.toml`](cgx-example.toml) for a more comprehensive example.

## Prebuilt Binaries

By default, `cgx` tries to use pre-built binaries and falls back to building from source if none are found. The default
provider order is:

1. `binstall`
2. `github-releases`
3. `gitlab-releases`
4. `quickinstall`

You can control this at the command line:

```sh
# Default behavior: try prebuilt binaries, fall back to source builds
cgx --prebuilt-binary auto ripgrep

# Require a prebuilt binary and fail if none are found
cgx --prebuilt-binary always ripgrep

# Never attempt to use a prebuilt binary; always build from source
cgx --prebuilt-binary never ripgrep

# Consider only GitHub releases and Quick Install as sources of prebuilt binaries;
# if neither are found then fall back to building from source
cgx --prebuilt-binary-sources github-releases,quickinstall ripgrep
```

Or in config:

```toml
[prebuilt_binaries]
use_prebuilt_binaries = "auto"
binary_providers = ["binstall", "github-releases", "gitlab-releases", "quickinstall"]
```

To disable prebuilt binaries in config, set `use_prebuilt_binaries = "never"`. An empty `binary_providers` list is only
valid when prebuilt binaries are disabled.

Prebuilt binaries are used only when default features and settings are selected. Custom features, `--all-features`,
`--no-default-features`, custom profiles, custom targets, custom toolchains, `--bin`, or `--example` always cause `cgx` to
build from source instead.

## HTTP Configuration and Proxies

`cgx` makes HTTP requests to download crate metadata, pre-built binaries, and release assets from registries, GitHub,
GitLab, and other providers. These requests can be configured via the `[http]` section in `cgx.toml`, CLI flags, or
environment variables.

```toml
[http]
timeout = "30s"
retries = 2
backoff_base = "500ms"
backoff_max = "5s"
proxy = "socks5://localhost:1080"
```

```sh
cgx --http-timeout 60s --http-retries 3 ripgrep
cgx --http-proxy socks5://localhost:1080 ripgrep
cgx --http-proxy http://user:password@proxyhost:8080 ripgrep
```

HTTP values use this precedence: CLI flags and `CGX_HTTP_*` environment variables, then config files, then Cargo
environment variable fallbacks, then defaults. The Cargo fallbacks are:

| Cargo Variable       | cgx Equivalent   | Description                          |
| -------------------- | ---------------- | ------------------------------------ |
| `CARGO_HTTP_PROXY`   | `--http-proxy`   | HTTP/SOCKS proxy URL                 |
| `CARGO_HTTP_TIMEOUT` | `--http-timeout` | Request timeout in seconds (integer) |
| `CARGO_NET_RETRY`    | `--http-retries` | Number of retry attempts             |

The standard proxy variables `HTTPS_PROXY`, `https_proxy`, and `http_proxy` are also honored automatically by the
underlying HTTP library.

For git operations (`--git`, `--github`, `--gitlab`) over HTTP/S, `cgx` applies the same HTTP settings where possible:
proxy, retries and backoff, user agent, and timeout. For git-over-HTTP specifically, `timeout` is used both as a
connection timeout and as a stalled-transfer timeout threshold.

## Cargo Subcommand Package

The `cargo-cgx` crate packages the same `cgx` tool but as a Cargo subcommand:

```sh
cargo install cargo-cgx
cargo cgx ripgrep --version
```

This is functionally the same as running `cgx ripgrep --version`. It exists for users who prefer the `cargo <command>`
style or want `cgx` available as a Cargo plugin.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
