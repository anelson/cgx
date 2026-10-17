//! Test crates used by the integration tests.
//!
//! Fixtures own temporary copies of checked-in crates so tests can build or modify them without
//! changing the originals or sharing state with other tests. The copies are removed on drop.

use std::path::{Path, PathBuf};

use assert_fs::{TempDir, prelude::*};

/// A temporary crate whose executable reports its command-line arguments.
pub(crate) struct CargoArgvProbe {
    root: TempDir,
}

impl CargoArgvProbe {
    /// Copy the complete crate into a fresh temporary directory without building it.
    pub(crate) fn copy_to_temp() -> Self {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/integration/testdata/cargo-argv-probe");
        let root = TempDir::with_prefix("cargo-argv-probe-").unwrap();
        root.copy_from(source, &["**"]).unwrap();
        Self { root }
    }

    pub(crate) fn path(&self) -> &Path {
        self.root.path()
    }

    /// Cargo's default output directory for debug binaries.
    pub(crate) fn debug_bin_dir(&self) -> PathBuf {
        self.path().join("target/debug")
    }
}
