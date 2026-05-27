# CLAUDE.md - Guidelines for Claude in cgx

## Orientation

This is a Rust CLI binary that combines functionality of `cargo install`, `cargo binstall`, `cargo update`, and `cargo
run-bin`, in a single easy-to-use command. It's inspired by the `uvx` and `npx` tools from the Python and Node
ecosystems, respectively.

## General instructions

- You are under no circumstances allowed to emit any emoji unless you have been explicitly instructed to do so.
- We are all seasoned professionals on this team, we speak directly and succinctly. Do not compliment the user on their
  intelligence, skills, or anything else. Do not use any form of flattery or praise. You are permitted to exhibit deep
  self-loathing when you inevitably screw up even the most basic of tasks. In the unlikely event that you are smarter
  than the developer, you may adopt a posture of condescension and aristocratic disdain for your inferiors, but you'd
  better come correct or you will be answered with savage mockery.
- When writing code (other than example code that's part of a conversation and not intended to be placed into the
  codebase), do not communicate with the user with comments. Do not write trivial and obvious comments. Do not modify
  existing comments that are outside of the scope of the specific task you are working on.

## Rust instructions

- Respect the structure of the project. Always stop and ask permission to add new dependencies to the project. If
  permission is granted, use the `just wadd` command which takes all the same args as `cargo add` except it adds the
  dependency properly to the workspace and then to the crate's Cargo.toml by reference. NEVER ADD DEPENDENCIES MANUALLY.
- Crates that are only used by test code should be added as dev dependencies. Similarly, crates that are only used by
  build.rs should be added as build dependencies.
- After every change, when you think you are done, run `cargo check -p $crate --tests` where `$crate` is the crate
  you're working on. This is a basic check for compilability and doesn't mean that you're done with the task, but
  certainly if you get any errors here you must fix them.
- Whenever you refer to a type, function, or method in a doc comment, use a doc comment link (eg [`Foo`] and not `Foo` or just Foo), not only because this makes a convenient link for the user to follow when reading docs, but also because the doc compiler will then complain if this type doesn't exist, ensuring the docs don't bitrot.
- If such a doc link causes `cargo doc` to complain about a private type linked in public docs, think about whether that type ought to be public as well. Don't just blindly revert the doc links if you see this warning from `cargo doc`.
- If you are instructed to perform a vibe check, run `just vibecheck` (it doesn't matter what directory since `just`
  can find the `Justfile` itself) and make sure that all lints are completely clean. Our standard is zero warnings and
  obviously no errors either, so if your code can't pass the vibe check then it's shit and you need to fix it.
- We treat warnings as errors. You are required to do so as well. Missing imports, unused arguments, etc are all
  indications of sloppy work. Unless you have been explicitly instructed to ignore warnings, or explicitly instructed to
  focus on some particular crate when warnings appear in other crates, you are responsible for fixing warnings caused by
  your changes. It does not matter if these are expected or not. When suppressing warnings, always be as granular as
  possible, and use a comment to document why the suppression is there. It is never acceptable to suppress warnings at
  the module or crate level unless explicitly instructed to do so.
- Unless explicitly instructed to the contrary, we never care about backward compatibility. Public methods can be
  removed or renamed, fields can be changed, all manner of breaking changes during a refactoring or feature implementation
  are allowed. There may be limited exceptions to this rule, but those exceptions will be stated explicitly in the task
  description.
- When importing types into a scope with `use`, do so at the module level, at the top of that module, without newlines
  between `use` lines. DO NOT import types at the function or local scope level. When importing types as part of a `mod
tests` module, the `use` statements should be inside the `mod tests` module, at the top of that module. If not part of
  a test, the imports should go at the top of the file.

### Error Reporting

We use `snafu` for error reporting. The error enum is in `error.rs`, and is called `Error`. Each crate should import
`crate::Result` and use that as the return type for functions that can fail.

To report an error in Snafu, you do not instantiate the error variants directly. Instead, you use the context selector
syntax. For example, if you have an error variant like this:

```rust
#[snafu(display("Failed to read file {}: {}", path.display(), source))]
ReadFile {
  path: PathBuf,
  source: std::io::Error,
},
```

You would report this error like so (NB: There is no `ReadFile` error variant it's just an example):

```rust
use crate::Error;
use crate::error;
use crate::Result;
use snafu::ResultExt;
use std::path::PathBuf;
// ...
fn read_file(path: &PathBuf) -> Result<String> {
  let contents = std::fs::read_to_string(path).with_context(|_| error::ReadFileSnafu { path: path.clone() })?;
  Ok(contents)
}
```

Note that `with_context` defers the creation of the context until the error actually occurs. This is essential when the
context involves cloning or formatting, which you don't want to do unless the error actually happens. For simpler error
scenarios that don't involve cloning or formatting, you can use the `context` method instead.

When propagating errors from other libraries, use the `source` field to wrap the original error. This preserves the
error chain and allows for better debugging.

If you have not a `Result` but some error type, you can use the `into_error` method on the generated Snafu context
selectors. To continue the above example, imagine you have an `std::io::Error` from somewhere and you need to construct
a `ReadFile` error variant from it:

```rust
use crate::Error;
use crate::error;
use snafu::ResultExt;
use std::path::PathBuf;
// ...
fn handle_io_error(path: &Path, io_err: std::io::Error) -> Error {
  error::ReadFileSnafu { path: path.clone() }.into_error(io_err)
}
```

Similarly, you can produce a `Result::Err` from a snafu context selector with its `fail` method.

When adding new error variants, make sure to provide a clear and informative error message in the `display` attribute.

DO NOT ABUSE EXISTING ERROR VARIANTS. When reporting an error, consider all existing `Error` variants and choose the one that best fits the situation. If none of them fit, then add a new variant. But do not just pick the closest one and shoehorn your error into it. This leads to confusing and misleading error messages.

`snafu` also offers helper methods on `Option` via `OptionExt`, when you need to turn a `None` value into an error. Use
those in favor of `ok_or_else` or similar methods, because they are more concise.

It also offers `ensure!` macros, which are like `assert!` but return an error instead of panicking.

### Unit testing

- Unit tests go in the same file as the code they are testing, in a `#[cfg(test)] mod tests` module at the bottom of the file.
- When asserting that something worked, DO NOT DO `assert!(result.is_ok())`. This is stupid and unhelpful. Just call
  `unwrap()` on the result and let it panic if it was an error (NOTE THIS IS ONLY IN TESTS! Production code must report
  errors and not panic except in very specific situations where an unreachable panic is appropriate).
- Likewise when asserting that something failed, DO NOT DO `assert!(result.is_err())`. Instead, use
  `assert_matches!(result, Err(Error::SpecificErrorVariant { .. }))`. `use assert_matches::assert_matches` if needed to
  bring that macro into scope.
- Construct asserts so that they provide useful information on failure. Using the `assert_matches` crate is often
  better than using `assert!` to assert some truthy statement, because `assert_matches` will print the actual value on failure,
  which is often very helpful for debugging.
- Do not use `expect` with an error message in favor of `unwrap` unless that additional context will make the test
  failure panic more understandable. In most cases, `unwrap` is sufficient.
- In general, tests and test helpers should panic instead of return `Result`. There will be limited exceptions to this,
  but in general test helpers and tests themselves should panic on failure.

## Git instructions

You can assume that the `gh` CLI is available to you to interact with Github. You can use the `git` CLI as needed, however you are strictly prohibited from staging, unstaging, committing, or revering any files under source control unless you have been explicitly asked to do so.

**Never** add a `Co-Authored-By: Claude ...` or any `Co-Authored-By: ... @anthropic.com` trailer to commit messages or PR descriptions in this repository. This applies to commits you author directly, squash-merge commit bodies, and PR descriptions. Do not include the trailer even if a default workflow or template suggests it. The repo's contributors graph is polluted by these trailers, so they are forbidden here. A `PreToolUse` hook in `.claude/settings.json` will block any `git commit` invocation that includes the trailer; the hook is a backstop, not a license to attempt the commit.

## Instructions

### Build & Run Commands

- Build: `cargo build`
- Run: `cargo run`
- Release build: `cargo build --release`

### Test Commands

- Run all tests: `just test`
- Run all tests in a specific crate: `cargo test -p crate --all-features`
- Run a specific test in a specific crate: `cargo test -p crate --all-features test_name`

### Lint Commands

- Use `just vibecheck` in place of specific lints. NEVER attempt to invoke a specific linter on a specific file.
  Checking lints on the entire project is not expensive, and as an LLM you are too stupid to reliably know which
  specific files need to be linted.
- When a change touches `#[cfg(...)]`, platform-specific logic, or any other conditional-compilation path, also run
  `just xwin-check` and `just xmac-check` to make sure the code still compiles cleanly on non-Linux targets. These
  recipes are expected to be warning-free. `xwin-check` will install `cargo-xwin` automatically if needed; `xmac-check`
  requires Docker.
- Use `just precommit` when you think you are done with a task and it is ready to commit (although you will never commit
  it yourself because you are prohibited from doing that unless you have been explicitly instructed to do so). This will
  run all compiler checks, lints, and tests, and if all of that passes it will also run the formatter.

### Code Style Guidelines

- **Formatting**: Follow Rust style guide (`rustfmt`)
- **Naming**: Use snake_case for variables/functions, CamelCase for types/traits
- **Imports**: Imports from the same crate or module within a crate should be grouped together with `{` and `}`.
- **Error Handling**: Use Result<T, E> for recoverable errors, panic for unrecoverable
- **Comments**: Doc comments with `///` for public items, regular `//` for implementation. Thoroughly comment logic
  that is non-obvious, a hack around some limitation or bug, or anything else that might confuse a future reader. Do not
  write stupid zero-value comments like `// get the thing` on a line `let thing = get_thing();`.
- **Types**: Favor strong typing, use type aliases for complex types
- **Functions**: Keep functions small and focused on a single task
