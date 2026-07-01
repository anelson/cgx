mod archive;
mod binstall;
mod checksum;
mod github;
mod gitlab;
mod quickinstall;

use std::collections::HashSet;

use snafu::IntoError;

use crate::{
    Result, bin_resolver::ConclusiveResolution, config::BinaryProvider, downloader::DownloadedCrate, error,
};

pub(super) use archive::{
    ArchiveFormat, extract_binary_at_archive_relative_path, extract_binary_by_candidate_names,
};
pub(super) use binstall::BinstallProvider;
pub(super) use github::GithubProvider;
pub(super) use gitlab::GitlabProvider;
pub(super) use quickinstall::QuickinstallProvider;

/// Trait for providers that can resolve pre-built binaries.
pub(super) trait Provider {
    /// Attempt to find and download a pre-built binary for the given crate.
    ///
    /// All providers receive the full [`DownloadedCrate`], which includes both the resolved
    /// metadata and the crate source directory. Providers that only need the metadata (like
    /// heuristic URL probers) can access it via `krate.resolved`.
    ///
    /// Returns [`ConclusiveResolution::Found`] with the binary,
    /// [`ConclusiveResolution::Nonexistent`] if this provider conclusively has no binary for
    /// the crate, or `Err` if the provider could not determine a conclusive outcome.
    ///
    /// If the provider returns `Err`, the resolution of the crate for this provider will be
    /// considered inconclusive, that is to say it is not cached as either a positive or negative
    /// result.  Providers should strive to produce only conclusive results, but of course
    /// providers to fallible things like network I/O and parsing of data, in which case those
    /// providers should return `Err` to indicate that they could not determine a conclusive
    /// outcome.
    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<ConclusiveResolution>;

    /// The kind of this provider, used for progress reporting.
    fn kind(&self) -> BinaryProvider;
}

/// Wrap a downloaded asset preparation failure with the provider and asset URL that caused it.
pub(super) fn provider_asset_preparation_failed(
    provider: BinaryProvider,
    url: &str,
    source: error::Error,
) -> error::Error {
    error::ProviderAssetPreparationFailedSnafu {
        provider,
        url: url.to_string(),
    }
    .into_error(Box::new(source))
}

/// Build the prioritized archive-internal binary basenames to look for after an asset matched.
///
/// The crate's default binary name is always tried first because that is the name callers expect
/// to install, for example `rg` for the `ripgrep` crate. If the matched release asset was generated
/// from a different name component, that basename is tried next as a provider-derived fallback; for
/// example, a `taplo-cli` crate may publish `taplo-...tar.gz` assets whose archives contain a
/// `taplo` binary. The crate name is tried last for projects whose package and binary names differ
/// in metadata but whose release archives still use the crate name. Duplicate and empty names are
/// removed while preserving this priority order.
pub(super) fn expected_binary_names(
    primary: &str,
    matched_candidate_name: Option<&str>,
    crate_name: &str,
) -> Vec<String> {
    let mut names = Vec::new();
    push_expected_binary_name(&mut names, primary);
    if let Some(name) = matched_candidate_name {
        push_expected_binary_name(&mut names, name);
    }
    push_expected_binary_name(&mut names, crate_name);
    names
}

fn push_expected_binary_name(names: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !names.iter().any(|existing| existing == name) {
        names.push(name.to_string());
    }
}

/// A candidate release asset filename paired with instructions for extracting or copying it.
pub(super) struct CandidateFilename {
    /// Release asset filename to probe or download, without any provider-specific URL prefix.
    pub filename: String,

    /// Binary name implied by this asset filename's name component.
    ///
    /// This is not necessarily the crate's default binary name. Some projects publish assets under
    /// a different name than the crate, so providers use this as a fallback expected basename when
    /// searching inside an archive after this candidate filename matched.
    pub binary_basename: String,

    /// File format inferred from the filename suffix.
    pub format: ArchiveFormat,
}

/// Generate candidate filenames that might be used for a release asset for a given crate, version,
/// and platform.
///
/// Produces naming patterns common across GitHub, GitLab, and elsewhere, combining candidate
/// names, various forms of the platform string, the version, and archive suffixes with various
/// separators. Each candidate carries its [`ArchiveFormat`] representing what the expected format
/// would be for a given candidate file, if it is found to exist.
///
/// Names are tried in priority order: `crate_name` first, then any `extra_binary_names` — binaries
/// the crate declares whose name differs from the crate name. This is what lets, for example,
/// `cgx taplo-cli` find the `taplo`-named assets of the `taplo-cli` crate. For each name,
/// variations with multiple forms of platform strings are generated. Duplicate filenames are
/// removed.
pub(super) fn generate_candidate_filenames(
    crate_name: &str,
    extra_binary_names: &[&str],
    version: &str,
    platform: &str,
) -> Vec<CandidateFilename> {
    let formats = ArchiveFormat::all_formats();
    let platforms = platform_aliases(platform);

    // Crate name first, then binary-name fallbacks. On Windows, also try `{name}.exe` as the name
    // component for projects that bake the extension into the asset name (e.g.
    // `eza.exe_x86_64-pc-windows-gnu.tar.gz`); these come last as the least-likely form.
    let mut names: Vec<String> = Vec::new();
    names.push(crate_name.to_string());
    names.extend(extra_binary_names.iter().map(|&n| n.to_string()));
    if platform.contains("windows") {
        names.push(format!("{}.exe", crate_name));
        names.extend(extra_binary_names.iter().map(|n| format!("{}.exe", n)));
    }

    let mut candidates = Vec::new();
    for name in &names {
        for platform_token in &platforms {
            for &(format, suffix) in formats {
                push_candidate_patterns(&mut candidates, name, version, platform_token, format, suffix);
            }
        }
    }

    // Dedup by filename, keeping the first (highest-priority) occurrence. Needed because a binary
    // name may coincide with the crate name on exotic targets, or a short alias with the triple.
    let mut seen = HashSet::new();
    candidates.retain(|c| seen.insert(c.filename.clone()));

    candidates
}

/// Build the list of platform tokens to try for asset matching: the full target triple first (the
/// most specific, lowest-false-positive form), then the shorter `{os}-{arch}` and `{arch}-{os}`
/// forms many projects use in their asset names.
///
/// The os component is mapped from the triple: `*apple*`/`*darwin*` -> `darwin`, `*windows*` ->
/// `windows`, `*linux*` -> `linux`. The arch is the triple's first component, used verbatim (so
/// `x86_64`, `aarch64`, `armv7` all match as-is); no arch synonyms are invented, to avoid
/// over-generating candidates that could match the wrong asset.
fn platform_aliases(platform: &str) -> Vec<String> {
    let mut aliases = vec![platform.to_string()];

    let arch = platform.split('-').next().unwrap_or("");
    let os = if platform.contains("windows") {
        Some("windows")
    } else if platform.contains("apple") || platform.contains("darwin") {
        Some("darwin")
    } else if platform.contains("linux") {
        Some("linux")
    } else {
        None
    };

    if let Some(os) = os {
        if !arch.is_empty() {
            for alias in [format!("{}-{}", os, arch), format!("{}-{}", arch, os)] {
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
            }
        }
    }

    aliases
}

fn push_candidate_patterns(
    candidates: &mut Vec<CandidateFilename>,
    name: &str,
    version: &str,
    platform: &str,
    format: ArchiveFormat,
    suffix: &str,
) {
    candidates.push(CandidateFilename {
        filename: format!("{}-{}-v{}{}", name, platform, version, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}-{}-{}{}", name, platform, version, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}-v{}-{}{}", name, version, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}-{}-{}{}", name, version, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}_{}_v{}{}", name, platform, version, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}_{}_{}{}", name, platform, version, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}_v{}_{}{}", name, version, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}_{}_{}{}", name, version, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}-{}{}", name, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
    candidates.push(CandidateFilename {
        filename: format!("{}_{}{}", name, platform, suffix),
        binary_basename: name.to_string(),
        format,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filenames(candidates: &[CandidateFilename]) -> Vec<&str> {
        candidates.iter().map(|c| c.filename.as_str()).collect()
    }

    /// The taplo case: the crate is `taplo-cli`, but its GitHub assets are named after the `taplo`
    /// binary, use a short `{os}-{arch}` platform token, and a bare `.gz` suffix. All three must
    /// combine into a single candidate carrying the naked [`ArchiveFormat::Gz`] format.
    #[test]
    fn binary_name_short_platform_and_naked_gz_combine() {
        let candidates =
            generate_candidate_filenames("taplo-cli", &["taplo"], "0.10.0", "x86_64-unknown-linux-gnu");

        let target = candidates
            .iter()
            .find(|c| c.filename == "taplo-linux-x86_64.gz")
            .expect("expected a taplo-linux-x86_64.gz candidate");
        assert_eq!(target.format, ArchiveFormat::Gz);
    }

    /// Crate name + full triple + the first archive format is still generated first, so crates
    /// whose assets already match the historical scheme keep matching the same asset (no
    /// regression).
    #[test]
    fn crate_name_full_triple_is_first_candidate() {
        let candidates =
            generate_candidate_filenames("taplo-cli", &["taplo"], "0.10.0", "x86_64-unknown-linux-gnu");
        assert_eq!(
            candidates[0].filename,
            "taplo-cli-x86_64-unknown-linux-gnu-v0.10.0.tar"
        );
    }

    /// Every crate-name candidate is ordered before any binary-name candidate.
    #[test]
    fn crate_name_candidates_precede_binary_name_candidates() {
        let candidates =
            generate_candidate_filenames("taplo-cli", &["taplo"], "0.10.0", "x86_64-unknown-linux-gnu");
        let names = filenames(&candidates);
        let last_crate = names.iter().rposition(|n| n.starts_with("taplo-cli")).unwrap();
        let first_binary = names
            .iter()
            .position(|n| n.starts_with("taplo-") && !n.starts_with("taplo-cli"))
            .unwrap();
        assert!(
            last_crate < first_binary,
            "all taplo-cli candidates should precede the taplo (binary) candidates"
        );
    }

    /// Filenames are unique even when a binary name coincides with the crate name.
    #[test]
    fn candidates_are_deduplicated() {
        let candidates = generate_candidate_filenames("foo", &["foo"], "1.0.0", "x86_64-unknown-linux-gnu");
        let count = candidates.len();
        let mut names = filenames(&candidates);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "candidate filenames should be unique");
    }

    #[test]
    fn matched_candidate_carries_binary_basename() {
        let candidates =
            generate_candidate_filenames("ripgrep", &["rg"], "15.1.0", "x86_64-unknown-linux-musl");
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.filename == "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz")
            .unwrap();

        assert_eq!(candidate.binary_basename, "ripgrep");
    }

    #[test]
    fn ripgrep_15_1_0_release_asset_names_are_generated() {
        let cases = [
            (
                "x86_64-unknown-linux-musl",
                "ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz",
                ArchiveFormat::TarGz,
            ),
            (
                "aarch64-unknown-linux-gnu",
                "ripgrep-15.1.0-aarch64-unknown-linux-gnu.tar.gz",
                ArchiveFormat::TarGz,
            ),
            (
                "aarch64-apple-darwin",
                "ripgrep-15.1.0-aarch64-apple-darwin.tar.gz",
                ArchiveFormat::TarGz,
            ),
            (
                "x86_64-pc-windows-msvc",
                "ripgrep-15.1.0-x86_64-pc-windows-msvc.zip",
                ArchiveFormat::Zip,
            ),
        ];

        for (platform, asset_name, format) in cases {
            let candidates = generate_candidate_filenames("ripgrep", &["rg"], "15.1.0", platform);
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.filename == asset_name)
                .unwrap();

            assert_eq!(candidate.format, format);
            assert_eq!(candidate.binary_basename, "ripgrep");
        }
    }

    #[test]
    fn expected_binary_names_are_deduplicated_in_priority_order() {
        assert_eq!(
            expected_binary_names("rg", Some("ripgrep"), "ripgrep"),
            vec!["rg".to_string(), "ripgrep".to_string()]
        );
        assert_eq!(
            expected_binary_names("ripgrep", Some("ripgrep"), "ripgrep"),
            vec!["ripgrep".to_string()]
        );
    }

    /// On Windows, `{name}.exe` is also tried as the name component (for assets like
    /// `eza.exe_x86_64-pc-windows-gnu.tar.gz`).
    #[test]
    fn windows_adds_exe_name_variant() {
        let candidates = generate_candidate_filenames("mytool", &[], "1.0.0", "x86_64-pc-windows-msvc");
        let names = filenames(&candidates);
        assert!(
            names.iter().any(|n| n.starts_with("mytool.exe")),
            "expected a mytool.exe-prefixed candidate on Windows"
        );
    }

    #[test]
    fn platform_aliases_full_triple_first_then_short_forms() {
        let aliases = platform_aliases("x86_64-unknown-linux-gnu");
        assert_eq!(aliases[0], "x86_64-unknown-linux-gnu");
        assert!(aliases.contains(&"linux-x86_64".to_string()));
        assert!(aliases.contains(&"x86_64-linux".to_string()));
    }

    /// macOS triples use `apple`, but most asset names use `darwin`; the alias must bridge that.
    #[test]
    fn platform_aliases_maps_apple_to_darwin() {
        let aliases = platform_aliases("aarch64-apple-darwin");
        assert!(aliases.contains(&"darwin-aarch64".to_string()));
        assert!(aliases.contains(&"aarch64-darwin".to_string()));
    }

    #[test]
    fn platform_aliases_windows() {
        let aliases = platform_aliases("x86_64-pc-windows-msvc");
        assert!(aliases.contains(&"windows-x86_64".to_string()));
        assert!(aliases.contains(&"x86_64-windows".to_string()));
    }
}
