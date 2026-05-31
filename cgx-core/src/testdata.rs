//! Module exposing a strongly typed interface to the test Cargo projects located in the `testdata`
//! directory.
//!
//! This module is only built when tests are enabled.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

pub(crate) struct CrateTestCase {
    /// The name of the test case, which is also the name of the directory under `testdata`.
    pub name: &'static str,

    /// The full path to the test case directory.
    ///
    /// NEVER EVER EVER MODIFY FILES HERE!  This is the canonical source of truth for the test case.
    /// Instead, use `temp_copy` to get a temporary copy of the test case that tests can modify at
    /// will.
    #[allow(dead_code)]
    path: PathBuf,

    /// The temp directory containing a copy of the test case.
    ///
    /// Kept alive to prevent automatic cleanup of the temporary directory. All test crates are
    /// self-contained and copied to a subdirectory (`temp_dir/{crate_name}/`).
    _temp_dir: TempDir,

    /// Path to the crate within the temp directory (`temp_dir/{crate_name`}/).
    /// This is what tests should use as the source directory.
    crate_path: PathBuf,
}

impl CrateTestCase {
    /// Get the path to the crate in the temporary directory.
    ///
    /// This is the directory containing Cargo.toml that should be used for building.
    #[allow(clippy::misnamed_getters)] // as far as the caller knows `crate_path` is the path
    pub(crate) fn path(&self) -> &Path {
        &self.crate_path
    }

    pub(crate) fn all() -> Vec<Self> {
        vec![
            Self::os_specific_deps(),
            Self::proc_macro_dep(),
            Self::simple_bin_no_deps(),
            Self::simple_lib_no_deps(),
            Self::single_crate_multiple_bins(),
            Self::single_crate_multiple_bins_with_default(),
            Self::stale_serde(),
            Self::thicc(),
            Self::timestamp(),
            Self::workspace_all_libs(),
            Self::workspace_multiple_bin_crates(),
        ]
    }

    pub(crate) fn os_specific_deps() -> Self {
        Self::load("os-specific-deps")
    }

    pub(crate) fn proc_macro_dep() -> Self {
        Self::load("proc-macro-dep")
    }

    pub(crate) fn simple_bin_no_deps() -> Self {
        Self::load("simple-bin-no-deps")
    }

    pub(crate) fn simple_lib_no_deps() -> Self {
        Self::load("simple-lib-no-deps")
    }

    pub(crate) fn single_crate_multiple_bins() -> Self {
        Self::load("single-crate-multiple-bins")
    }

    pub(crate) fn single_crate_multiple_bins_with_default() -> Self {
        Self::load("single-crate-multiple-bins-with-default")
    }

    pub(crate) fn stale_serde() -> Self {
        Self::load("stale-serde")
    }

    pub(crate) fn thicc() -> Self {
        Self::load("thicc")
    }

    pub(crate) fn timestamp() -> Self {
        Self::load("timestamp")
    }

    pub(crate) fn workspace_all_libs() -> Self {
        Self::load("workspace-all-libs")
    }

    pub(crate) fn workspace_multiple_bin_crates() -> Self {
        Self::load("workspace-multiple-bin-crates")
    }

    /// Load a test case from the filesystem, by name
    fn load(name: &'static str) -> Self {
        const TESTDATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/crates");

        let path = Path::new(TESTDATA_DIR).join(name);
        assert!(
            path.exists() && path.is_dir(),
            "Test case '{name}' doesn't exist: {}",
            path.display()
        );

        let temp_dir = tempfile::tempdir().unwrap();

        // Copy the crate into a subdirectory of temp_dir
        let crate_path = temp_dir.path().join(name);
        crate::helpers::copy_source_tree(&path, &crate_path).unwrap();

        // Canonicalize the path to ensure consistent handling across platforms
        // (e.g., resolves /var -> /private/var symlink on macOS)
        let crate_path = std::fs::canonicalize(crate_path).unwrap();

        Self {
            name,
            path,
            _temp_dir: temp_dir,
            crate_path,
        }
    }
}

pub(crate) struct ConfigTestCase {
    /// The full path to the test case (either a directory or a specific file)
    path: PathBuf,
}

impl ConfigTestCase {
    /// Get the path to this test case (directory or file, depending on the test case)
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Load a test case from the filesystem
    fn load(name: &'static str, relative_path: &str, must_exist: bool) -> Self {
        const TESTDATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/configs");

        let path = if relative_path.is_empty() {
            Path::new(TESTDATA_DIR).to_path_buf()
        } else {
            Path::new(TESTDATA_DIR).join(relative_path)
        };

        if must_exist {
            assert!(
                path.exists(),
                "Config test case '{}' doesn't exist: {}",
                name,
                path.display()
            );
        }

        Self { path }
    }

    // Directory-based hierarchy test cases

    pub(crate) fn hierarchy_root() -> Self {
        Self::load("hierarchy_root", "", true)
    }

    pub(crate) fn hierarchy_work() -> Self {
        Self::load("hierarchy_work", "work", true)
    }

    pub(crate) fn hierarchy_project1() -> Self {
        Self::load("hierarchy_project1", "work/project1", true)
    }

    pub(crate) fn hierarchy_project2() -> Self {
        Self::load("hierarchy_project2", "work/project2", true)
    }

    // File-based test cases

    pub(crate) fn explicit_non_standard_name() -> Self {
        Self::load(
            "explicit_non_standard_name",
            "work/project1/not_called_cgx.toml",
            true,
        )
    }

    pub(crate) fn invalid_toml() -> Self {
        Self::load("invalid_toml", "invalid_toml.toml", true)
    }

    pub(crate) fn invalid_options() -> Self {
        Self::load("invalid_options", "invalid_config_options.toml", true)
    }

    // Special case: intentionally nonexistent file for testing error handling

    pub(crate) fn nonexistent() -> Self {
        Self::load("nonexistent", "does_not_exist.toml", false)
    }
}
