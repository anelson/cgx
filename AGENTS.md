# AGENTS.md - Guidelines for agents in cgx

## Orientation

This is a Rust CLI binary that combines functionality of `cargo install`, `cargo binstall`, `cargo update`, and
`cargo run-bin` in a single command. It is inspired by `uvx` and `npx`.

## General instructions

- Do not emit emoji unless explicitly instructed.
- Be direct and succinct. Do not flatter the user.
- When writing code, do not communicate with the user through comments. Do not write trivial comments. Do not modify
  existing comments outside the scope of the task.

## Rust instructions

- Respect the structure of the project. Always stop and ask permission before adding dependencies. If permission is
  granted, use `just wadd` with the same arguments you would pass to `cargo add`. Never add dependencies manually.
- Crates used only by tests are dev dependencies. Crates used only by `build.rs` are build dependencies.
- After changing a crate, run `cargo check -p $crate --tests` for the crate you changed. This is only a basic
  compilability check; fix any errors before continuing.
- Whenever you refer to a type, function, or method in a doc comment, use a doc comment link, for example [`Foo`], not
  plain `Foo`. If such a link causes `cargo doc` to complain about a private type linked in public docs, consider
  whether the type should be public instead of blindly removing the link.
- If you are instructed to perform a vibe check, run `just vibecheck` and make sure it is completely clean.
- We treat warnings as errors. Missing imports, unused arguments, and similar warnings are sloppy work. Unless explicitly
  instructed otherwise, fix warnings caused by your changes. When suppressing warnings, be as granular as possible and
  document why the suppression is there. Do not suppress warnings at the module or crate level unless explicitly
  instructed.
- Unless explicitly instructed to the contrary, we do not care about backward compatibility. Public methods can be
  removed or renamed, fields can be changed, and breaking changes are allowed during refactoring or feature work.
- Put `use` imports at module scope near the top of the module. Do not import types at function or local scope. In
  `mod tests`, put test imports inside the test module at the top. Imports from the same crate or module should be
  grouped with braces.
- Arrange imports in three blocks separated by a single blank line, in this order: `std`/`core`/`alloc`, then external
  crates, then first-party paths (`crate`/`self`/`super`). Within a block, merge all imports that share a crate root
  into a single braced `use`.
- Do not put blank lines between `use` statements. The only blank lines allowed in an import section are the single
  blank line separating each of the three groups above. Inserting a stray blank line after the existing `use` lines
  defeats rustfmt's ability to regroup and reorder imports, so never do it when adding a new import. This applies to
  re-exports (`pub use ...`) too: do not try to visually separate them from plain imports, since rustfmt sorts them in
  with everything else in their group.

### Panicking, `unwrap`, and `expect`

Test code is exempt from these rules (see `clippy.toml`); the following applies to non-test code.

`clippy::unwrap_used` forbids a bare `.unwrap()`. When a call genuinely cannot fail, pick one of:

- keep `.unwrap()` and justify it with `#[expect(clippy::unwrap_used, reason = "why this cannot fail")]`, or
- use `.expect("why this cannot fail")`, where the message passed to `expect` justifies why the failure is impossible.

Prefer the `.expect("…")` form: the justification lives at the call site and is printed if the "impossible" ever
happens, so it does not duplicate an `#[expect]` reason. There is intentionally no `expect_used` lint, so `.expect()`
needs no attribute — but an `expect` message that does not explain why the condition cannot occur is not acceptable.

`panic!` and `unreachable!` are allowed but must carry an explanatory message, enforced by the `panic-requires-message`
ast-grep rule (run as part of `just vibecheck`). `todo!` and `unimplemented!` are forbidden outright (`clippy::todo`,
`clippy::unimplemented`).

### Error Reporting

We use `snafu` for error reporting. The main error enum is `cgx-core/src/error.rs::Error`; the `cgx` crate re-exports
the core error type, and `cargo-cgx` is only a thin wrapper. Where a crate defines `crate::Result`, use that as the
return type for fallible functions.

To report an error in Snafu, do not instantiate error variants directly. Use the context selector syntax. For example,
if you have an error variant like this:

```rust
#[snafu(display("Failed to read file {}: {}", path.display(), source))]
ReadFile {
    path: PathBuf,
    source: std::io::Error,
},
```

Report it like this:

```rust
use crate::Result;
use crate::error;
use snafu::ResultExt;
use std::path::PathBuf;

fn read_file(path: &PathBuf) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|_| error::ReadFileSnafu { path: path.clone() })?;
    Ok(contents)
}
```

`with_context` defers creation of the context until the error actually occurs. This matters when the context involves
cloning or formatting. For simpler error scenarios that do not involve cloning or formatting, use `context` instead.

When propagating errors from other libraries, use the `source` field to wrap the original error. This preserves the
error chain and improves debugging.

If you have an error type rather than a `Result`, use `into_error` on the generated Snafu context selector:

```rust
use crate::Error;
use crate::error;
use snafu::ResultExt;
use std::path::Path;

fn handle_io_error(path: &Path, io_err: std::io::Error) -> Error {
    error::ReadFileSnafu {
        path: path.to_path_buf(),
    }
    .into_error(io_err)
}
```

Similarly, produce `Result::Err` from a Snafu context selector with its `fail` method.

When adding new error variants, provide a clear and informative error message in the `display` attribute.

Do not abuse existing error variants. When reporting an error, consider all existing `Error` variants and choose the one
that best fits the situation. If none fit, add a new variant. Do not shoehorn an error into a misleading variant.

`snafu` also offers helper methods on `Option` via `OptionExt` when you need to turn `None` into an error. Use those in
favor of `ok_or_else` or similar methods.

It also offers `ensure!` macros, which are like `assert!` but return an error instead of panicking.

### Unit testing

- Unit tests go in the same file as the code they test, in a `#[cfg(test)] mod tests` module at the bottom of the file.
- When asserting that something worked, do not write `assert!(result.is_ok())`. Call `unwrap()` on the result and let it
  panic if it was an error.
- When asserting that something failed, do not write `assert!(result.is_err())`. Use
  `assert_matches!(result, Err(Error::SpecificErrorVariant { .. }))`. Use `assert_matches::assert_matches` if needed.
- Construct asserts so they provide useful failure information. `assert_matches` is often better than `assert!` because
  it prints the actual value on failure.
- Do not use `expect` instead of `unwrap` unless the extra context materially improves the panic.
- In general, tests and test helpers should panic instead of returning `Result`. Exceptions are allowed only when there
  is a specific reason.

## Git instructions

You can assume that the `gh` CLI is available to interact with GitHub. You can use the `git` CLI as needed, but you are
strictly prohibited from staging, unstaging, committing, or reverting files under source control unless explicitly asked.

Never add a `Co-Authored-By: Claude ...` or any `Co-Authored-By: ... @anthropic.com` trailer to commit messages or PR
descriptions in this repository. This applies to commits you author directly, squash-merge commit bodies, and PR
descriptions. A `PreToolUse` hook in `.claude/settings.json` blocks matching `git commit` invocations; the hook is a
backstop, not permission to attempt the commit.

## Build and check commands

`BUILDING.md` is the human-facing build guide and is also applicable to agents. Key commands:

- Build: `cargo build`
- Run: `cargo run`
- Release build: `cargo build --release`
- Run all tests: `just test`
- Run tests in one crate: `cargo test -p crate --all-features`
- Run one test in one crate: `cargo test -p crate --all-features test_name`
- Main compile/lint/doc check: `just vibecheck`
- Apply all formatters/beautifiers to local code files: `just fmt`
- Formatting check: `just fmtcheck`
- Dependency checks: `just depcheck`
- Full local precommit sweep: `just precommit`

Use `just vibecheck` instead of invoking specific linters on specific files. When a change touches `#[cfg(...)]`,
platform-specific logic, process execution, paths, archive handling, linking, or build/release configuration, also run
the platform checks that make sense:

```sh
just xwin-check
just xmac-check
```

`xwin-check` installs `cargo-xwin` if needed and checks the Windows MSVC target. `xmac-check` requires Docker and checks
the macOS x86_64 target through a Dockerized `cargo-zigbuild` environment.

Use `just precommit` when you think a task is ready to commit. You still must not commit unless explicitly asked.
`precommit` runs formatting first, then `vibecheck`, dependency checks, and the full test suite.

## Code style

- **Formatting**: Follow the Rust style guide (`rustfmt`).
- **Naming**: Use snake_case for variables/functions and CamelCase for types/traits.
- **Error Handling**: Use `Result<T, E>` for recoverable errors and panic only for unrecoverable states.
- **Comments**: Use doc comments with `///` for public items. Use regular comments only for non-obvious logic,
  workarounds, hacks, or anything else that might confuse a future reader. Do not write zero-value comments.
- **Types**: Favor strong typing. Use type aliases for complex types when they make code clearer.
- **Functions**: Keep functions small and focused.
