# cargo-cgx

Cargo subcommand for [cgx](https://crates.io/crates/cgx) - run Rust tools quickly without explicit installation.

## Usage

After installing this crate:

```sh
cargo install cargo-cgx
```

You can run cgx as a cargo subcommand:

```sh
cargo cgx ripgrep pattern
cargo cgx just --help
cargo cgx eza -la
```

This is functionally identical to using `cgx` directly:

```sh
cgx ripgrep pattern
cgx just --help
cgx eza -la
```

## Documentation

This is a thin wrapper around cgx that provides cargo subcommand integration.

For complete documentation on cgx's features, configuration, and usage, please see the [cgx crate
documentation](https://docs.rs/cgx/) and the [project README](https://github.com/anelson/cgx#readme).
