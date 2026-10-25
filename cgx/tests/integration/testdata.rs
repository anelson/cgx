//! Test crates used by the integration tests.
//!
//! Fixtures own temporary copies of checked-in crates so tests can build or modify them without
//! changing the originals or sharing state with other tests. The copies are removed on drop.

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_fs::{TempDir, prelude::*};

/// A temporary crate whose executable reports its arguments and environment as JSON.
pub(crate) struct ArgvProbe {
    root: TempDir,
}

impl ArgvProbe {
    /// Copy the complete crate into a fresh temporary directory without building it.
    pub(crate) fn copy_to_temp() -> Self {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/integration/testdata/argv-probe");
        let root = TempDir::with_prefix("argv-probe-").unwrap();
        root.copy_from(source, &["**"]).unwrap();
        Self { root }
    }

    /// Copy the probe with a different package name, which also names its default binary.
    ///
    /// Cargo discovers `cargo foo` through an executable named `cargo-foo`, and `cgx cargo foo`
    /// resolves a crate with that name. Renaming the temporary copy supports these tests without
    /// giving the shared fixture a Cargo-specific name. Both the manifest and lockfile are updated
    /// so `--locked` builds remain valid.
    pub(crate) fn copy_to_temp_as_crate(crate_name: &str) -> Self {
        let probe = Self::copy_to_temp();
        let crate_name = format!("\"{crate_name}\"");
        for filename in ["Cargo.toml", "Cargo.lock"] {
            let path = probe.path().join(filename);
            let contents = fs::read_to_string(&path).unwrap();
            fs::write(path, contents.replace("\"argv-probe\"", &crate_name)).unwrap();
        }
        probe
    }

    pub(crate) fn path(&self) -> &Path {
        self.root.path()
    }

    /// Cargo's default output directory for debug binaries.
    pub(crate) fn debug_bin_dir(&self) -> PathBuf {
        self.path().join("target/debug")
    }
}
