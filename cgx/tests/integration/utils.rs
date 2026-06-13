//! Utility functions to help run our CLI as part of a test
use assert_cmd::{Command, assert::OutputAssertExt, cargo::cargo_bin_cmd};
use assert_fs::{TempDir, prelude::*};
use cgx::messages::{BuildCacheMessage, BuildMessage, CgxMessage, Message, Provenance};
use serde_json::Deserializer;

pub(crate) struct TestFs {
    pub(crate) app_root: TempDir,
    pub(crate) system_root: TempDir,
    pub(crate) cwd: TempDir,
}

impl TestFs {
    fn new() -> Self {
        let system_root = TempDir::with_prefix("cgx-sys-").unwrap();
        let app_root = TempDir::with_prefix("cgx-app-").unwrap();
        let cwd = TempDir::with_prefix("cgx-cwd-").unwrap();

        Self {
            app_root,
            system_root,
            cwd,
        }
    }
}

/// Represents the `cgx` binary for use in tests.
///
/// The `cmd` field provides helpers for running the binary and asserting on its output.
pub(crate) struct Cgx {
    pub(crate) cmd: Command,
    pub(crate) test_fs: Option<TestFs>,
}

impl Cgx {
    /// Creates a new `Cgx` that locates the bin
    pub(crate) fn find() -> Self {
        Self {
            cmd: cargo_bin_cmd!("cgx"),
            test_fs: None,
        }
    }

    /// Clear all arguments that may have been set on the command and start again.
    ///
    /// Any arguments that were set by [`Self::with_test_env`] will be preserved, as will the temp
    /// dirs associated with the test env.
    pub(crate) fn reset(self) -> Self {
        // the `Command` struct doesn't have a way to clear args, so we just recreate it.
        let mut me = Self::find();

        if let Some(test_fs) = self.test_fs {
            me.set_test_env(test_fs);
        }

        me
    }

    /// Construct an isolated filesystem structure for running the command.
    ///
    /// In almost all cases, this is needed to ensure that test invocations of `cgx` do not use the
    /// config files from the host system.  Certainly any tests that rely on behavior that can be
    /// overridden in the config file, or that set config options as part of the test, must use
    /// this to get consistent results.
    ///
    /// This will populate the [`Self::test_fs`] field with temporary directories
    pub(crate) fn with_test_fs() -> Self {
        let mut me = Self::find();
        let test_fs = TestFs::new();
        me.set_test_env(test_fs);
        me
    }

    pub(crate) fn test_fs(&self) -> &TestFs {
        self.test_fs.as_ref().expect("test_fs not set")
    }

    pub(crate) fn test_fs_app_root(&self) -> &TempDir {
        &self.test_fs().app_root
    }

    /// Get the user config directory (`app_root/config`)
    ///
    /// This matches the logic in config.rs where `--app-dir` causes user config
    /// to be loaded from `{app_dir}/config/cgx.toml`
    pub(crate) fn user_config_dir(&self) -> assert_fs::fixture::ChildPath {
        self.test_fs().app_root.child("config")
    }

    fn set_test_env(&mut self, test_fs: TestFs) {
        self.cmd
            .arg("--system-config-dir")
            .arg(test_fs.system_root.path());
        self.cmd.arg("--app-dir").arg(test_fs.app_root.path());
        self.cmd.current_dir(test_fs.cwd.path());

        self.test_fs = Some(test_fs);
    }
}

/// Extension trait to add helper methods to `Command` for testing `cgx`
pub(crate) trait CommandExt {
    /// Add the argument to enable JSON message output in `cgx`
    ///
    /// NOTE: If this is used, make sure to call [`CommandExt::assert_with_messages`] to capture
    /// and parse the cgx messages separately from the rest of the stdout output.  If you forget
    /// tod o this, then assert helpers like `stdout` will see the JSON messages as well as the
    /// usual output which will likely cause test failures.
    fn with_json_messages(&mut self) -> &mut Self;

    /// Special case of [`OutputAssertExt::assert`] that filters out any value JSON cgx messages
    /// from stdout and returns them separately, as well as an [`Assert`] object which DOES NOT see
    /// the filtered JSON message output.
    fn assert_with_messages(&mut self) -> (assert_cmd::assert::Assert, Vec<Message>);
}

impl CommandExt for Command {
    fn with_json_messages(&mut self) -> &mut Self {
        self.arg("--message-format").arg("json")
    }

    fn assert_with_messages(&mut self) -> (assert_cmd::assert::Assert, Vec<Message>) {
        let output = self.assert().get_output().clone();

        // Parse stdout line-by-line as JSON messages
        let mut messages = Vec::new();
        let mut filtered_stdout = Vec::new();
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        for line in stdout_str.lines() {
            match Deserializer::from_str(line).into_iter::<Message>().next() {
                Some(Ok(msg)) => messages.push(msg),
                Some(Err(_)) | None => {
                    // Not a valid JSON message or an empty line
                    // Pass this on to the Assert object
                    filtered_stdout.push(line);
                }
            }
        }

        let filtered_output = std::process::Output {
            status: output.status,
            stdout: filtered_stdout.join("\n").into_bytes(),
            stderr: output.stderr,
        };

        let assert = filtered_output.assert();

        (assert, messages)
    }
}

/// Assert that the emitted messages report the crate's binary was built from source.
///
/// This is cache-agnostic: it reads only the [`CgxMessage::CrateProvenance`] message and does
/// not distinguish a fresh compile from a build-cache hit. Use [`assert_compiled_from_source`] or
/// [`assert_cached_source_build`] to assert one or the other of those cases specifically.
pub(crate) fn assert_built_from_source(messages: &[Message]) {
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Cgx(CgxMessage::CrateProvenance {
                provenance: Provenance::BuiltFromSource { .. },
                ..
            })
        )),
        "expected a CrateProvenance message reporting the binary was built from source, got: {messages:#?}"
    );
}

/// Assert that the emitted messages report the crate's binary was compiled from source *during this
/// run*, meaning that it performed a fresh `cargo` build, rather than serving from cache.
///
/// Checks both that a [`BuildMessage::Started`] was emitted (cargo actually ran; it is emitted only
/// on a build-cache miss) and that the [`CgxMessage::CrateProvenance`] message reports a source
/// build.
pub(crate) fn assert_compiled_from_source(messages: &[Message]) {
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "expected a BuildMessage::Started message (a fresh cargo compile), got: {messages:#?}"
    );
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Cgx(CgxMessage::CrateProvenance {
                provenance: Provenance::BuiltFromSource { .. },
                ..
            })
        )),
        "expected a CrateProvenance message reporting the binary was built from source, got: {messages:#?}"
    );
}

/// Assert that the emitted messages report the crate's binary was a source build served from the
/// build cache *without* recompiling.
///
/// Checks that a [`BuildCacheMessage::CacheHit`] was emitted, that no [`BuildMessage::Started`] was
/// emitted (cargo did not run this time), and that the authoritative
/// [`CgxMessage::CrateProvenance`] reports a source build. Use this when a test's point is "this
/// run reused a cached source build".
pub(crate) fn assert_cached_source_build(messages: &[Message]) {
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::BuildCache(BuildCacheMessage::CacheHit { .. }))),
        "expected a BuildCacheMessage::CacheHit message, got: {messages:#?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::Build(BuildMessage::Started { .. }))),
        "expected NO BuildMessage::Started message (binary should be served from cache, not recompiled), \
         got: {messages:#?}"
    );
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Cgx(CgxMessage::CrateProvenance {
                provenance: Provenance::BuiltFromSource { .. },
                ..
            })
        )),
        "expected a CrateProvenance message reporting the binary was built from source, got: {messages:#?}"
    );
}

/// Assert that the emitted messages report the crate's binary was obtained as a pre-built binary.
///
/// Reads the authoritative [`CgxMessage::CrateProvenance`].
pub(crate) fn assert_prebuilt(messages: &[Message]) {
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Cgx(CgxMessage::CrateProvenance {
                provenance: Provenance::Prebuilt { .. },
                ..
            })
        )),
        "expected a CrateProvenance message reporting a pre-built binary, got: {messages:#?}"
    );
}
