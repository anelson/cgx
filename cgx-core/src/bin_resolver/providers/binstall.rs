#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use snafu::ResultExt;

use super::{ArchiveFormat, Provider};
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    downloader::DownloadedCrate,
    error,
    http::{Bytes, HttpClient},
    messages::PrebuiltBinaryMessage,
};

pub(in crate::bin_resolver) struct BinstallProvider {
    reporter: crate::messages::MessageReporter,
    cache_dir: PathBuf,
    verify_checksums: bool,
    http_client: HttpClient,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BinstallMeta {
    pkg_url: Option<String>,
    pkg_fmt: Option<String>,
    bin_dir: Option<String>,
    #[serde(default)]
    overrides: HashMap<String, BinstallOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BinstallOverride {
    pkg_url: Option<String>,
    pkg_fmt: Option<String>,
    bin_dir: Option<String>,
}

impl BinstallMeta {
    /// Merge target-specific overrides into the base metadata for the given platform.
    fn merge_overrides(&mut self, target: &str) {
        if let Some(overrides) = self.overrides.remove(target) {
            if overrides.pkg_url.is_some() {
                self.pkg_url = overrides.pkg_url;
            }
            if overrides.pkg_fmt.is_some() {
                self.pkg_fmt = overrides.pkg_fmt;
            }
            if overrides.bin_dir.is_some() {
                self.bin_dir = overrides.bin_dir;
            }
        }
    }
}

/// Map a binstall `pkg-fmt` value to an [`ArchiveFormat`].
///
/// Returns [`ArchiveFormat::TarGz`] when no format is specified (the binstall spec default).
fn archive_format_from_pkg_fmt(pkg_fmt: Option<&str>) -> Result<ArchiveFormat> {
    match pkg_fmt {
        Some("tar") => Ok(ArchiveFormat::Tar),
        Some("tgz") | None => Ok(ArchiveFormat::TarGz),
        Some("txz") => Ok(ArchiveFormat::TarXz),
        Some("tzstd") => Ok(ArchiveFormat::TarZst),
        Some("tbz2") => Ok(ArchiveFormat::TarBz2),
        Some("zip") => Ok(ArchiveFormat::Zip),
        Some("bin") => Ok(ArchiveFormat::NakedBinary),
        Some(other) => error::UnsupportedArchiveFormatSnafu {
            format: other.to_string(),
        }
        .fail(),
    }
}

/// Return the list of `{ archive-suffix }` values to try for a given `pkg-fmt`.
///
/// Matches the behavior of cargo-binstall's `PkgFmt::extensions()`: each pkg-fmt has a
/// primary (short) suffix and may have an expanded alias. The caller should try each suffix
/// in order until a download succeeds.
fn binstall_archive_suffixes(pkg_fmt: Option<&str>) -> &'static [&'static str] {
    match pkg_fmt {
        Some("tar") => &[".tar"],
        Some("tgz") | None => &[".tgz", ".tar.gz"],
        Some("txz") => &[".txz", ".tar.xz"],
        Some("tzstd") => &[".tzstd", ".tzst", ".tar.zst"],
        Some("tbz2") => &[".tbz2", ".tar.bz2"],
        Some("zip") => &[".zip"],
        Some("bin") => {
            if cfg!(windows) {
                &[".bin", "", ".exe"]
            } else {
                &[".bin", ""]
            }
        }
        Some(_) => &[""],
    }
}

/// Render a binstall template string by replacing `{ variable }` placeholders.
fn render_template(template: &str, ctx: &TemplateContext<'_>) -> String {
    let mut result = template.to_string();
    result = result.replace("{ name }", ctx.name);
    result = result.replace("{ version }", ctx.version);
    result = result.replace("{ target }", ctx.target);
    result = result.replace("{ archive-suffix }", ctx.archive_suffix);
    result = result.replace("{ binary-ext }", ctx.binary_ext);
    result = result.replace("{ bin }", ctx.bin);
    if let Some(repo) = ctx.repo {
        result = result.replace("{ repo }", repo);
    }
    result
}

struct TemplateContext<'a> {
    name: &'a str,
    version: &'a str,
    target: &'a str,
    archive_suffix: &'a str,
    binary_ext: &'a str,
    bin: &'a str,
    repo: Option<&'a str>,
}

impl BinstallProvider {
    pub(in crate::bin_resolver) fn new(
        reporter: crate::messages::MessageReporter,
        cache_dir: PathBuf,
        verify_checksums: bool,
        http_client: HttpClient,
    ) -> Self {
        Self {
            reporter,
            cache_dir,
            verify_checksums,
            http_client,
        }
    }

    /// Read and parse `[package.metadata.binstall]` from the crate's Cargo.toml.
    fn read_binstall_metadata(krate: &DownloadedCrate, target: &str) -> Result<Option<BinstallMeta>> {
        let doc = krate.parsed_cargo_toml()?;
        let binstall_value = doc
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("binstall"));
        let Some(binstall_value) = binstall_value else {
            return Ok(None);
        };
        let mut meta: BinstallMeta =
            binstall_value
                .clone()
                .try_into()
                .with_context(|_| error::BinstallMetadataInvalidSnafu {
                    path: krate.cargo_toml_path(),
                })?;
        meta.merge_overrides(target);
        Ok(Some(meta))
    }

    /// Get the repository URL for a crate, for use in `{ repo }` template variable.
    ///
    /// If the crate came from a forge, the forge URL is used directly. This handles the fork
    /// scenario where the Cargo.toml may still point to the upstream repository. For all other
    /// sources, falls back to the `[package].repository` field in Cargo.toml unfiltered (binstall
    /// templates can point at any host, so no host filtering is applied).
    fn get_repo_url(krate: &DownloadedCrate) -> Result<Option<String>> {
        match &krate.resolved.source {
            ResolvedSource::Forge { forge, .. } => Ok(Some(forge.repo_url())),
            ResolvedSource::CratesIo
            | ResolvedSource::Registry { .. }
            | ResolvedSource::Git { .. }
            | ResolvedSource::LocalDir { .. } => krate.repository_url(),
        }
    }

    /// Download a file from the given URL.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404 (resource
    /// does not exist), or `Err` for any other failure (network errors, non-404 HTTP errors).
    fn try_download(&self, url: &str) -> Result<Option<Bytes>> {
        self.http_client.try_download(url)
    }

    fn verify_checksum(&self, data: &[u8], url: &str) -> Result<()> {
        let checksum_url = format!("{}.sha256", url);

        let checksum_data = match self.try_download(&checksum_url)? {
            Some(data) => data,
            None => return Ok(()),
        };

        let checksum_str = String::from_utf8_lossy(&checksum_data);
        let expected_hash = checksum_str.split_whitespace().next().ok_or_else(|| {
            error::ChecksumMismatchSnafu {
                expected: checksum_str.to_string(),
                actual: "invalid checksum format".to_string(),
            }
            .build()
        })?;

        self.reporter
            .report(|| PrebuiltBinaryMessage::verifying_checksum(expected_hash));

        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_hash = crate::helpers::format_hex_lower(hasher.finalize());

        if expected_hash != actual_hash {
            return error::ChecksumMismatchSnafu {
                expected: expected_hash.to_string(),
                actual: actual_hash,
            }
            .fail();
        }

        self.reporter.report(PrebuiltBinaryMessage::checksum_verified);

        Ok(())
    }
}

impl Provider for BinstallProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::Binstall
    }

    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<ConclusiveResolution> {
        let resolved = &krate.resolved;

        let Some(meta) = Self::read_binstall_metadata(krate, platform)? else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::Binstall,
                    "no [package.metadata.binstall] in Cargo.toml",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let Some(ref pkg_url_template) = meta.pkg_url else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::Binstall,
                    "binstall metadata has no pkg-url",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        let pkg_fmt = meta.pkg_fmt.as_deref();
        let format = archive_format_from_pkg_fmt(pkg_fmt)?;
        let binary_ext = if cfg!(windows) { ".exe" } else { "" };
        let repo_url = Self::get_repo_url(krate)?;
        let version_string = resolved.version.to_string();
        let suffixes = binstall_archive_suffixes(pkg_fmt);

        let mut last_url = String::new();
        let mut data = None;

        for suffix in suffixes {
            let ctx = TemplateContext {
                name: &resolved.name,
                version: &version_string,
                target: platform,
                archive_suffix: suffix,
                binary_ext,
                bin: &resolved.name,
                repo: repo_url.as_deref(),
            };

            let url = render_template(pkg_url_template, &ctx);

            self.reporter
                .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::Binstall));

            match self.try_download(&url) {
                Ok(Some(bytes)) => {
                    data = Some(bytes);
                    last_url = url;
                    break;
                }
                Ok(None) => {
                    last_url = url;
                }
                Err(e) => return Err(e),
            }
        }

        let Some(data) = data else {
            // If not binary found and no transient error then it just means this crate legit
            // doesn't have a prebuilt binary for this platform in any of the places we know to
            // look.
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::Binstall,
                    format!("download failed: {}", last_url),
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };
        let url = last_url;

        if self.verify_checksums {
            self.verify_checksum(&data, &url)?;
        }

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir.path().join(format.canonical_filename());
        std::fs::write(&archive_path, &data).with_context(|_| error::IoSnafu {
            path: archive_path.clone(),
        })?;

        let binary_name = krate.default_binary_name()?;
        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = super::extract_binary(&archive_path, format, &binary_name, &extract_dir)?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("binstall")
            .join(&resolved.name)
            .join(resolved.version.to_string())
            .join(platform);

        std::fs::create_dir_all(&final_dir).with_context(|_| error::IoSnafu {
            path: final_dir.clone(),
        })?;

        let final_path = final_dir.join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));
        std::fs::copy(&binary_path, &final_path).with_context(|_| error::IoSnafu {
            path: final_path.clone(),
        })?;

        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&final_path)
                .with_context(|_| error::IoSnafu {
                    path: final_path.clone(),
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&final_path, perms).with_context(|_| error::IoSnafu {
                path: final_path.clone(),
            })?;
        }

        Ok(ConclusiveResolution::Found(ResolvedBinary {
            krate: resolved.clone(),
            provider: BinaryProvider::Binstall,
            path: final_path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use semver::Version;

    use super::*;
    use crate::{crate_resolver::ResolvedSource, cratespec::Forge, error::Error};

    #[test]
    fn render_template_basic() {
        let ctx = TemplateContext {
            name: "eza",
            version: "0.23.1",
            target: "x86_64-unknown-linux-gnu",
            archive_suffix: ".tar.gz",
            binary_ext: "",
            bin: "eza",
            repo: Some("https://github.com/eza-community/eza"),
        };

        let template = "{ repo }/releases/download/v{ version }/{ name }_{ target }{ archive-suffix }";
        let rendered = render_template(template, &ctx);
        let expected = concat!(
            "https://github.com/eza-community/eza/releases/download/",
            "v0.23.1/eza_x86_64-unknown-linux-gnu.tar.gz",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_template_binary_ext() {
        let ctx = TemplateContext {
            name: "mytool",
            version: "1.0.0",
            target: "x86_64-pc-windows-msvc",
            archive_suffix: ".zip",
            binary_ext: ".exe",
            bin: "mytool",
            repo: None,
        };

        let template = "https://example.com/{ name }-v{ version }-{ target }{ archive-suffix }";
        let rendered = render_template(template, &ctx);
        assert_eq!(
            rendered,
            "https://example.com/mytool-v1.0.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn render_template_bin_variable() {
        let ctx = TemplateContext {
            name: "cargo-watch",
            version: "8.0.0",
            target: "aarch64-apple-darwin",
            archive_suffix: ".tar.xz",
            binary_ext: "",
            bin: "cargo-watch",
            repo: Some("https://github.com/watchexec/cargo-watch"),
        };

        let template =
            "{ repo }/releases/download/v{ version }/{ bin }-v{ version }-{ target }{ archive-suffix }";
        let rendered = render_template(template, &ctx);
        let expected = concat!(
            "https://github.com/watchexec/cargo-watch/releases/download/",
            "v8.0.0/cargo-watch-v8.0.0-aarch64-apple-darwin.tar.xz",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_template_missing_repo() {
        let ctx = TemplateContext {
            name: "tool",
            version: "1.0.0",
            target: "x86_64-unknown-linux-gnu",
            archive_suffix: ".tar.gz",
            binary_ext: "",
            bin: "tool",
            repo: None,
        };

        let template = "{ repo }/download/{ name }";
        let rendered = render_template(template, &ctx);
        // { repo } is not replaced when repo is None
        assert_eq!(rendered, "{ repo }/download/tool");
    }

    #[test]
    fn archive_format_from_pkg_fmt_defaults_to_tar_gz() {
        assert_eq!(archive_format_from_pkg_fmt(None).unwrap(), ArchiveFormat::TarGz);
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tgz")).unwrap(),
            ArchiveFormat::TarGz
        );
    }

    #[test]
    fn archive_format_from_pkg_fmt_unknown_returns_error() {
        assert_matches::assert_matches!(
            archive_format_from_pkg_fmt(Some("unknown")),
            Err(Error::UnsupportedArchiveFormat { .. })
        );
    }

    #[test]
    fn archive_format_from_pkg_fmt_known_formats() {
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tar")).unwrap(),
            ArchiveFormat::Tar
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("txz")).unwrap(),
            ArchiveFormat::TarXz
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tzstd")).unwrap(),
            ArchiveFormat::TarZst
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tbz2")).unwrap(),
            ArchiveFormat::TarBz2
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("zip")).unwrap(),
            ArchiveFormat::Zip
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("bin")).unwrap(),
            ArchiveFormat::NakedBinary
        );
    }

    #[test]
    fn archive_format_canonical_filename_consistency() {
        assert_eq!(
            archive_format_from_pkg_fmt(None).unwrap().canonical_filename(),
            "archive.tar.gz"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tgz"))
                .unwrap()
                .canonical_filename(),
            "archive.tar.gz"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("txz"))
                .unwrap()
                .canonical_filename(),
            "archive.tar.xz"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tzstd"))
                .unwrap()
                .canonical_filename(),
            "archive.tar.zst"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("tbz2"))
                .unwrap()
                .canonical_filename(),
            "archive.tar.bz2"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("zip"))
                .unwrap()
                .canonical_filename(),
            "archive.zip"
        );
        assert_eq!(
            archive_format_from_pkg_fmt(Some("bin"))
                .unwrap()
                .canonical_filename(),
            if cfg!(windows) { "archive.exe" } else { "archive" }
        );
    }

    #[test]
    fn parse_binstall_metadata_from_toml() {
        let toml_content = r#"
            [package]
            name = "eza"
            version = "0.23.1"

            [package.metadata.binstall]
            pkg-url = "{ repo }/releases/download/v{ version }/{ name }_{ target }{ archive-suffix }"
            pkg-fmt = "tgz"
            bin-dir = "{ bin }{ binary-ext }"

            [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
            pkg-fmt = "zip"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let binstall_value = doc
            .get("package")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("binstall")
            .unwrap();

        let meta: BinstallMeta = binstall_value.clone().try_into().unwrap();
        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("{ repo }/releases/download/v{ version }/{ name }_{ target }{ archive-suffix }")
        );
        assert_eq!(meta.pkg_fmt.as_deref(), Some("tgz"));
        assert_eq!(meta.bin_dir.as_deref(), Some("{ bin }{ binary-ext }"));
        assert!(meta.overrides.contains_key("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn merge_overrides_applies_target() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/{ name }-{ target }.tar.gz"
            pkg-fmt = "tgz"

            [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
            pkg-fmt = "zip"
            pkg-url = "https://example.com/{ name }-{ target }.zip"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let binstall_value = doc
            .get("package")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("binstall")
            .unwrap();

        let mut meta: BinstallMeta = binstall_value.clone().try_into().unwrap();
        meta.merge_overrides("x86_64-pc-windows-msvc");

        assert_eq!(meta.pkg_fmt.as_deref(), Some("zip"));
        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("https://example.com/{ name }-{ target }.zip")
        );
    }

    #[test]
    fn merge_overrides_no_match_leaves_base() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/{ name }-{ target }.tar.gz"
            pkg-fmt = "tgz"

            [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
            pkg-fmt = "zip"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let binstall_value = doc
            .get("package")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("binstall")
            .unwrap();

        let mut meta: BinstallMeta = binstall_value.clone().try_into().unwrap();
        meta.merge_overrides("aarch64-apple-darwin");

        assert_eq!(meta.pkg_fmt.as_deref(), Some("tgz"));
        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("https://example.com/{ name }-{ target }.tar.gz")
        );
    }

    #[test]
    fn merge_overrides_partial_override() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/default"
            pkg-fmt = "tgz"
            bin-dir = "{ bin }"

            [package.metadata.binstall.overrides.aarch64-apple-darwin]
            pkg-fmt = "txz"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let binstall_value = doc
            .get("package")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("binstall")
            .unwrap();

        let mut meta: BinstallMeta = binstall_value.clone().try_into().unwrap();
        meta.merge_overrides("aarch64-apple-darwin");

        // pkg-fmt overridden
        assert_eq!(meta.pkg_fmt.as_deref(), Some("txz"));
        // pkg-url and bin-dir unchanged
        assert_eq!(meta.pkg_url.as_deref(), Some("https://example.com/default"));
        assert_eq!(meta.bin_dir.as_deref(), Some("{ bin }"));
    }

    #[test]
    fn missing_metadata_returns_none() {
        let toml_content = r#"
            [package]
            name = "some-crate"
            version = "1.0.0"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let result = doc
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("binstall"));
        assert!(result.is_none());
    }

    #[test]
    fn missing_pkg_url_in_metadata() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-fmt = "tgz"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let binstall_value = doc
            .get("package")
            .unwrap()
            .get("metadata")
            .unwrap()
            .get("binstall")
            .unwrap();

        let meta: BinstallMeta = binstall_value.clone().try_into().unwrap();
        assert!(meta.pkg_url.is_none());
    }

    #[test]
    fn get_repo_url_github_forge() {
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitHub {
                        custom_url: None,
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = BinstallProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn get_repo_url_gitlab_forge() {
        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::Forge {
                    forge: Forge::GitLab {
                        custom_url: None,
                        owner: "myowner".to_string(),
                        repo: "myrepo".to_string(),
                    },
                    commit: "abc123".to_string(),
                },
            },
            crate_path: PathBuf::from("/nonexistent"),
        };

        let url = BinstallProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://gitlab.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn get_repo_url_crates_io_with_repository() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
repository = "https://github.com/myowner/myrepo"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::CratesIo,
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = BinstallProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, Some("https://github.com/myowner/myrepo".to_string()));
    }

    #[test]
    fn get_repo_url_crates_io_without_repository() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        fs::write(
            &cargo_toml,
            r#"
[package]
name = "mytool"
version = "1.0.0"
"#,
        )
        .unwrap();

        let krate = DownloadedCrate {
            resolved: crate::crate_resolver::ResolvedCrate {
                name: "mytool".to_string(),
                version: Version::new(1, 0, 0),
                source: ResolvedSource::CratesIo,
            },
            crate_path: temp_dir.path().to_path_buf(),
        };

        let url = BinstallProvider::get_repo_url(&krate).unwrap();
        assert_eq!(url, None);
    }

    #[test]
    fn binstall_archive_suffixes_match_cargo_binstall() {
        assert_eq!(binstall_archive_suffixes(Some("tgz")), &[".tgz", ".tar.gz"]);
        assert_eq!(binstall_archive_suffixes(None), &[".tgz", ".tar.gz"]);
        assert_eq!(binstall_archive_suffixes(Some("txz")), &[".txz", ".tar.xz"]);
        assert_eq!(
            binstall_archive_suffixes(Some("tzstd")),
            &[".tzstd", ".tzst", ".tar.zst"]
        );
        assert_eq!(binstall_archive_suffixes(Some("tbz2")), &[".tbz2", ".tar.bz2"]);
        assert_eq!(binstall_archive_suffixes(Some("zip")), &[".zip"]);
        assert_eq!(binstall_archive_suffixes(Some("tar")), &[".tar"]);
    }
}
