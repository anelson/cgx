use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Message;
use crate::{
    bin_resolver::ResolvedBinary,
    builder::{BuildOptions, BuildTarget},
    config::BinaryProvider,
    crate_resolver::ResolvedCrate,
};

/// Engine-level messages emitted by [`crate::Cgx`] around the pre-built-vs-source decision.
///
/// These capture the full set of facts about a crate invocation — its resolved identity and source,
/// where its code lives on disk, the chosen build options and target, and how its binary was
/// obtained — so that callers (and tests) can observe what cgx decided without inspecting the built
/// binary itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CgxMessage {
    /// The fully-resolved crate and chosen build options, emitted *before* checking for a pre-built
    /// binary.
    ///
    /// Whether a pre-built binary will actually be found is not yet known, so this is the crate
    /// that is planned to be used, although whether it's to be built from source or obtained
    /// pre-built is not yet determined.
    CratePlan {
        /// The resolved crate identity (name, version, source).
        resolved: ResolvedCrate,
        /// Path to the crate's source code on disk (a cache path, or a local dir for `--path`).
        crate_path: PathBuf,
        /// The build options cgx chose for this invocation.
        options: BuildOptions,
        /// The Rust target triple this build targets (the explicit `--target`, or the host triple).
        target_platform: String,
    },
    /// How the crate's binary was ultimately obtained, emitted *after* the pre-built-vs-source
    /// decision.
    CrateProvenance {
        /// The resolved crate identity (name, version, source).
        resolved: ResolvedCrate,
        /// Path to the crate's source code on disk.
        crate_path: PathBuf,
        /// The build options cgx chose for this invocation.
        options: BuildOptions,
        /// The Rust target triple this binary is for (the explicit `--target`, or the host triple).
        ///
        /// The reported target platform pertains to the resolved binary itself, not from the build
        /// options: the binary may be for an ABI-compatible fallback of the host (a musl binary on
        /// a glibc host, an msvc PE on a windows-gnu host, etc)
        target_platform: String,
        /// Whether the binary was built from source or downloaded pre-built, and where it is.
        provenance: Provenance,
    },
}

/// How a crate's runnable binary was obtained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Provenance {
    /// Compiled from source by cargo.
    BuiltFromSource {
        /// Path to the compiled binary.
        binary_path: PathBuf,
        /// The concrete bin/example that was built (a `DefaultBin` request is resolved to the
        /// actual target).
        target_binary: BuildTarget,
    },
    /// Downloaded as a pre-built binary from the given provider.
    Prebuilt {
        /// The provider the binary was obtained from.
        provider: BinaryProvider,
        /// Path to the downloaded binary.
        binary_path: PathBuf,
    },
}

impl CgxMessage {
    /// Construct a [`Self::CratePlan`] from the resolved crate, its source path, chosen options,
    /// and enabled pre-built sources.
    pub fn crate_plan(resolved: &ResolvedCrate, crate_path: &Path, options: &BuildOptions) -> Self {
        Self::CratePlan {
            resolved: resolved.clone(),
            crate_path: crate_path.to_path_buf(),
            target_platform: options.target_platform().to_string(),
            options: options.clone(),
        }
    }

    /// Construct a [`Self::CrateProvenance`] recording that the binary was built from source.
    pub fn crate_provenance_built_from_source(
        resolved: &ResolvedCrate,
        crate_path: &Path,
        options: &BuildOptions,
        binary_path: &Path,
        target_binary: BuildTarget,
    ) -> Self {
        Self::CrateProvenance {
            resolved: resolved.clone(),
            crate_path: crate_path.to_path_buf(),
            target_platform: options.target_platform().to_string(),
            options: options.clone(),
            provenance: Provenance::BuiltFromSource {
                binary_path: binary_path.to_path_buf(),
                target_binary,
            },
        }
    }

    /// Construct a [`Self::CrateProvenance`] recording that a pre-built binary was used.
    pub fn crate_provenance_prebuilt(
        resolved: &ResolvedCrate,
        crate_path: &Path,
        options: &BuildOptions,
        binary: &ResolvedBinary,
    ) -> Self {
        Self::CrateProvenance {
            resolved: resolved.clone(),
            crate_path: crate_path.to_path_buf(),
            target_platform: binary.target.clone(),
            options: options.clone(),
            provenance: Provenance::Prebuilt {
                provider: binary.provider,
                binary_path: binary.path.clone(),
            },
        }
    }
}

impl From<CgxMessage> for Message {
    fn from(msg: CgxMessage) -> Self {
        Message::Cgx(msg)
    }
}
