#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use leon::{Template, Values};
use serde::Deserialize;
use snafu::ResultExt;
use target_lexicon::{OperatingSystem, Triple};

use super::{ArchiveFormat, Provider};
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedSource,
    downloader::DownloadedCrate,
    error,
    http::HttpClient,
    messages::PrebuiltBinaryMessage,
};

/// Provider for binaries described by `[package.metadata.binstall]` in a crate manifest.
pub(in crate::bin_resolver) struct BinstallProvider {
    reporter: crate::messages::MessageReporter,
    cache_dir: PathBuf,
    verify_checksums: bool,
    http_client: HttpClient,
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
        BinstallMetaRaw::try_from_cargo_toml(&doc).map(|raw| raw.map(|meta| meta.render_for_target(target)))
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

    fn try_download_to_file(&self, url: &str, path: &Path) -> Result<bool> {
        self.http_client.try_download_to_file(url, path)
    }
}

impl Provider for BinstallProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::Binstall
    }

    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<ConclusiveResolution> {
        let resolved = &krate.resolved;
        let target_info = BinstallTargetInfo::from_str(platform)?;

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
        let binary_ext = target_info.binary_ext();
        let repo_url = Self::get_repo_url(krate)?;
        let version_string = resolved.version.to_string();
        let suffixes = binstall_archive_suffixes(pkg_fmt);
        let binary_name = krate.default_binary_name()?;

        let mut last_url = String::new();
        let mut selected_suffix = "";

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir.path().join(format.canonical_filename());
        let mut downloaded = false;

        for &suffix in suffixes {
            let ctx = BinstallTemplateContext {
                name: &resolved.name,
                version: &version_string,
                target: platform,
                archive_suffix: Some(suffix),
                binary_ext,
                bin: &binary_name,
                repo: repo_url.as_deref(),
                target_info: &target_info,
                kind: BinstallTemplateKind::PackageUrl,
            };

            let url = ctx.render_template(pkg_url_template)?;

            self.reporter
                .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::Binstall));

            match self.try_download_to_file(&url, &archive_path) {
                Ok(true) => {
                    downloaded = true;
                    last_url = url;
                    selected_suffix = suffix;
                    break;
                }
                Ok(false) => {
                    last_url = url;
                }
                Err(e) => return Err(e),
            }
        }

        if !downloaded {
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
        }
        let url = last_url;

        if self.verify_checksums {
            let asset_filename = super::checksum::asset_filename_from_url(&url);
            let checksum_url = format!("{}.sha256", url);
            let checksum_path = temp_dir.path().join("checksum.sha256");
            let checksum_found =
                self.try_download_to_file(&checksum_url, &checksum_path)
                    .map_err(|source| {
                        super::provider_asset_preparation_failed(BinaryProvider::Binstall, &url, source)
                    })?;
            if checksum_found {
                super::checksum::verify_sha256_checksum(
                    &archive_path,
                    &checksum_path,
                    asset_filename,
                    &self.reporter,
                )
                .map_err(|source| {
                    super::provider_asset_preparation_failed(BinaryProvider::Binstall, &url, source)
                })?;
            }
        }

        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = if let Some(bin_dir_template) = meta.bin_dir.as_deref() {
            let ctx = BinstallTemplateContext {
                name: &resolved.name,
                version: &version_string,
                target: platform,
                archive_suffix: Some(selected_suffix),
                binary_ext,
                bin: &binary_name,
                repo: repo_url.as_deref(),
                target_info: &target_info,
                kind: BinstallTemplateKind::BinDir,
            };
            let rendered_path = ctx.render_template(bin_dir_template)?;
            super::extract_binary_at_archive_relative_path(
                &archive_path,
                format,
                &binary_name,
                Path::new(&rendered_path),
                &extract_dir,
            )
        } else {
            let expected_binary_names = super::expected_binary_names(&binary_name, None, &resolved.name);
            super::extract_binary_by_candidate_names(
                &archive_path,
                format,
                &expected_binary_names,
                &extract_dir,
            )
        }
        .map_err(|source| super::provider_asset_preparation_failed(BinaryProvider::Binstall, &url, source))?;

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

/// The resolved binstall metadata for a specific target, after applying any overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinstallMeta {
    pkg_url: Option<String>,
    pkg_fmt: Option<String>,
    bin_dir: Option<String>,
}

/// The raw binstall package metadata as read from a crate's Cargo.toml file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct BinstallMetaRaw {
    pkg_url: Option<String>,
    pkg_fmt: Option<String>,
    bin_dir: Option<String>,
    #[serde(default)]
    overrides: HashMap<String, BinstallMetaOverride>,
}

/// The raw binstall package metadata override for a specific target, as read from a crate's
/// Cargo.toml file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
struct BinstallMetaOverride {
    pkg_url: Option<String>,
    pkg_fmt: Option<String>,
    bin_dir: Option<String>,
}

impl BinstallMetaRaw {
    /// Check the parsed Cargo.toml for `[package.metadata.binstall]` and return the parsed metadata
    /// if present.
    fn try_from_cargo_toml(cargo_toml: &toml::Value) -> Result<Option<Self>> {
        let binstall_value = cargo_toml
            .get("package")
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("binstall"));
        let Some(binstall_value) = binstall_value else {
            return Ok(None);
        };

        let meta = binstall_value
            .clone()
            .try_into()
            .context(error::BinstallMetadataInvalidInCargoTomlSnafu)?;
        Ok(Some(meta))
    }

    /// Render the binstall metadata for a specific target, applying any overrides for that target.
    fn render_for_target(&self, target: &str) -> BinstallMeta {
        let mut meta = BinstallMeta {
            pkg_url: self.pkg_url.clone(),
            pkg_fmt: self.pkg_fmt.clone(),
            bin_dir: self.bin_dir.clone(),
        };

        if let Some(overrides) = self.overrides.get(target) {
            if let Some(pkg_url) = &overrides.pkg_url {
                meta.pkg_url = Some(pkg_url.clone());
            }
            if let Some(pkg_fmt) = &overrides.pkg_fmt {
                meta.pkg_fmt = Some(pkg_fmt.clone());
            }
            if let Some(bin_dir) = &overrides.bin_dir {
                meta.bin_dir = Some(bin_dir.clone());
            }
        }

        meta
    }
}

/// Target-derived values exposed to binstall templates.
///
/// This keeps the provider-facing API as a plain target triple string while using
/// [`target_lexicon::Triple`] internally to render cargo-binstall-compatible target placeholders.
struct BinstallTargetInfo {
    triple: Triple,
}

impl BinstallTargetInfo {
    /// The OS name in this target triple, normalized to the value cargo-binstall uses for
    /// `{ os-name }` in templates.
    fn normalized_os_name(&self) -> Cow<'static, str> {
        match self.triple.operating_system {
            OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => Cow::Borrowed("macos"),
            os => os.into_str(),
        }
    }

    /// The executable filename extension implied by this target.
    fn binary_ext(&self) -> &'static str {
        match self.triple.operating_system {
            OperatingSystem::Windows => ".exe",
            _ => "",
        }
    }
}

impl FromStr for BinstallTargetInfo {
    type Err = error::Error;

    fn from_str(target: &str) -> Result<Self> {
        let triple = Triple::from_str(target).map_err(|source| {
            error::InvalidTargetTripleSnafu {
                target: target.to_string(),
                message: source.to_string(),
            }
            .build()
        })?;
        Ok(Self { triple })
    }
}

/// Supplies target-derived placeholder values requested by [`leon`] while rendering templates.
///
/// Cargo-binstall exposes these keys independently of the full `{ target }` triple so package
/// authors can construct asset names like `wasmtime-{ target-arch }-{ target-family }`.
impl Values for BinstallTargetInfo {
    /// Return the value for one target placeholder key, or `None` when this context does not own
    /// it.
    fn get_value(&self, key: &str) -> Option<Cow<'_, str>> {
        match key {
            "target-family" => Some(self.triple.operating_system.into_str()),
            "os-name" => Some(self.normalized_os_name()),
            "target-arch" => Some(self.triple.architecture.into_str()),
            "target-libc" => Some(self.triple.environment.into_str()),
            "target-vendor" => Some(Cow::Borrowed(self.triple.vendor.as_str())),
            _ => None,
        }
    }
}

/// The binstall template currently being rendered.
///
/// Some placeholder names are intentionally context-sensitive. In particular, `{ format }` means
/// archive format in `pkg-url` templates and binary extension in `bin-dir` templates.
#[derive(Clone, Copy)]
enum BinstallTemplateKind {
    /// Rendering `pkg-url`, where archive placeholders such as `{ archive-format }` are available.
    PackageUrl,
    /// Rendering `bin-dir`, where `{ format }` is a compatibility alias for `{ binary-ext }`.
    BinDir,
}

/// Values used to render a binstall template with [`leon`].
///
/// The context combines package metadata, the selected archive suffix, repository information, and
/// target-derived placeholders. [`BinstallTemplateKind`] selects the small set of placeholders
/// whose meaning differs between `pkg-url` and `bin-dir`.
struct BinstallTemplateContext<'a> {
    name: &'a str,
    version: &'a str,
    target: &'a str,
    archive_suffix: Option<&'a str>,
    binary_ext: &'a str,
    bin: &'a str,
    repo: Option<&'a str>,
    target_info: &'a BinstallTargetInfo,
    kind: BinstallTemplateKind,
}

impl BinstallTemplateContext<'_> {
    fn render_template(&self, template: &str) -> Result<String> {
        let template_src = template;
        let template = Template::parse(template_src).with_context(|_| error::BinstallTemplateParseSnafu {
            template: template_src.to_string(),
        })?;
        template
            .render(self)
            .with_context(|_| error::BinstallTemplateRenderSnafu {
                template: template_src.to_string(),
            })
    }

    fn archive_format_from_suffix(&self) -> Option<Cow<'_, str>> {
        self.archive_suffix.map(|archive_suffix| {
            if archive_suffix.is_empty() {
                Cow::Borrowed("bin")
            } else {
                Cow::Borrowed(archive_suffix.trim_start_matches('.'))
            }
        })
    }
}

/// Supplies package, archive, binary, and delegated target values to [`leon`] during rendering.
///
/// This is the main template namespace for Binstall metadata. Unknown keys are delegated to
/// [`BinstallTargetInfo`] so target placeholders share the same rendering path as ordinary package
/// placeholders.
impl Values for BinstallTemplateContext<'_> {
    /// Return the value for one Binstall template key in the namespace selected by `kind`.
    fn get_value(&self, key: &str) -> Option<Cow<'_, str>> {
        match key {
            "name" => Some(Cow::Borrowed(self.name)),
            "version" => Some(Cow::Borrowed(self.version)),
            "repo" => self.repo.map(Cow::Borrowed),
            "target" => Some(Cow::Borrowed(self.target)),
            "bin" => Some(Cow::Borrowed(self.bin)),
            "binary-ext" => Some(Cow::Borrowed(self.binary_ext)),
            "archive-suffix" if matches!(self.kind, BinstallTemplateKind::PackageUrl) => {
                self.archive_suffix.map(Cow::Borrowed)
            }
            "archive-format" if matches!(self.kind, BinstallTemplateKind::PackageUrl) => {
                self.archive_format_from_suffix()
            }
            "format" => match self.kind {
                BinstallTemplateKind::PackageUrl => self.archive_format_from_suffix(),
                BinstallTemplateKind::BinDir => Some(Cow::Borrowed(self.binary_ext)),
            },
            key => self.target_info.get_value(key),
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

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, time::Duration};

    use flate2::{Compression, write::GzEncoder};
    use httpmock::prelude::*;
    use semver::Version;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::{
        config::HttpConfig, crate_resolver::ResolvedSource, cratespec::Forge, error::Error,
        messages::MessageReporter,
    };

    fn fast_retry_config() -> HttpConfig {
        HttpConfig {
            retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(10),
            ..Default::default()
        }
    }

    fn test_provider(verify_checksums: bool) -> (BinstallProvider, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let http_client = HttpClient::new(&fast_retry_config()).unwrap();
        (
            BinstallProvider::new(
                MessageReporter::null(),
                temp_dir.path().to_path_buf(),
                verify_checksums,
                http_client,
            ),
            temp_dir,
        )
    }

    fn downloaded_crate_with_toml(cargo_toml: &str) -> (DownloadedCrate, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        fs::write(temp_dir.path().join("Cargo.toml"), cargo_toml).unwrap();
        (
            DownloadedCrate {
                resolved: crate::crate_resolver::ResolvedCrate {
                    name: "package-name".to_string(),
                    version: Version::new(1, 0, 0),
                    source: ResolvedSource::CratesIo,
                },
                crate_path: temp_dir.path().to_path_buf(),
            },
            temp_dir,
        )
    }

    fn tar_gz_with_binary(relative_path: &str) -> Vec<u8> {
        let mut archive_data = Vec::new();
        {
            let encoder = GzEncoder::new(&mut archive_data, Compression::default());
            let mut tar = tar::Builder::new(encoder);
            let payload = b"#!/bin/sh\necho test";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, relative_path, &payload[..]).unwrap();
            tar.finish().unwrap();
        }
        archive_data
    }

    fn zip_with_binary(relative_path: &str) -> Vec<u8> {
        let mut archive_data = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut archive_data);
            let mut zip = zip::ZipWriter::new(cursor);
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);
            zip.start_file(relative_path, options).unwrap();
            zip.write_all(b"#!/bin/sh\necho test").unwrap();
            zip.finish().unwrap();
        }
        archive_data
    }

    fn render_pkg_template(template: &str, target: &str, archive_suffix: &str) -> String {
        let target_info = BinstallTargetInfo::from_str(target).unwrap();
        let binary_ext = target_info.binary_ext();
        let ctx = BinstallTemplateContext {
            name: "package-name",
            version: "1.0.0",
            target,
            archive_suffix: Some(archive_suffix),
            binary_ext,
            bin: "tool",
            repo: Some("https://github.com/example/package-name"),
            target_info: &target_info,
            kind: BinstallTemplateKind::PackageUrl,
        };

        ctx.render_template(template).unwrap()
    }

    fn render_bin_dir_template(template: &str, target: &str, archive_suffix: &str) -> String {
        let target_info = BinstallTargetInfo::from_str(target).unwrap();
        let binary_ext = target_info.binary_ext();
        let ctx = BinstallTemplateContext {
            name: "package-name",
            version: "1.0.0",
            target,
            archive_suffix: Some(archive_suffix),
            binary_ext,
            bin: "tool",
            repo: Some("https://github.com/example/package-name"),
            target_info: &target_info,
            kind: BinstallTemplateKind::BinDir,
        };

        ctx.render_template(template).unwrap()
    }

    #[test]
    fn render_template_compact_and_spaced_placeholders() {
        let template = "{repo}/{ repo }/{name}/{ name }/{version}/{ version }/{target}/{ target }";
        let rendered = render_pkg_template(template, "x86_64-unknown-linux-gnu", ".tgz");
        let expected = concat!(
            "https://github.com/example/package-name/",
            "https://github.com/example/package-name/",
            "package-name/package-name/",
            "1.0.0/1.0.0/",
            "x86_64-unknown-linux-gnu/x86_64-unknown-linux-gnu",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn pkg_url_template_supports_archive_format_aliases() {
        let template = "{archive-format}/{ format }/{ archive-suffix }/{archive-suffix}";
        let rendered = render_pkg_template(template, "x86_64-unknown-linux-gnu", ".tar.gz");
        assert_eq!(rendered, "tar.gz/tar.gz/.tar.gz/.tar.gz");
    }

    #[test]
    fn pkg_url_template_derives_tgz_archive_format() {
        let rendered = render_pkg_template("{ archive-format }", "x86_64-unknown-linux-gnu", ".tgz");
        assert_eq!(rendered, "tgz");
    }

    #[test]
    fn pkg_url_template_derives_bin_archive_format_from_empty_suffix() {
        let rendered = render_pkg_template("{ archive-format }", "x86_64-unknown-linux-gnu", "");
        assert_eq!(rendered, "bin");
    }

    #[test]
    fn bin_dir_template_uses_format_as_binary_ext_alias() {
        let rendered = render_bin_dir_template(
            "{ bin }{ format }/{ bin }{ binary-ext }",
            "x86_64-pc-windows-msvc",
            ".zip",
        );
        assert_eq!(rendered, "tool.exe/tool.exe");
    }

    #[test]
    fn render_template_target_placeholders_for_linux() {
        let template = "{ target-arch }/{ target-family }/{ os-name }/{ target-libc }/{ target-vendor }";
        let rendered = render_pkg_template(template, "x86_64-unknown-linux-gnu", ".tgz");
        assert_eq!(rendered, "x86_64/linux/linux/gnu/unknown");
    }

    #[test]
    fn render_template_target_placeholders_for_windows() {
        let template = "{ target-arch }/{ target-family }/{ os-name }/{ target-libc }/{ target-vendor }";
        let rendered = render_pkg_template(template, "x86_64-pc-windows-msvc", ".zip");
        assert_eq!(rendered, "x86_64/windows/windows/msvc/pc");
    }

    #[test]
    fn render_template_target_placeholders_for_macos() {
        let template = "{ target-arch }/{ target-family }/{ os-name }/{ target-libc }/{ target-vendor }";
        let rendered = render_pkg_template(template, "aarch64-apple-darwin", ".zip");
        assert_eq!(rendered, "aarch64/darwin/macos/unknown/apple");
    }

    #[test]
    fn render_template_escapes_braces_and_backslash() {
        let rendered = render_pkg_template(r"\{ {name} \} \\ {name}", "x86_64-unknown-linux-gnu", ".tgz");
        assert_eq!(rendered, r"{ package-name } \ package-name");
    }

    #[test]
    fn render_template_binary_ext() {
        let template = "https://example.com/{ name }-v{ version }-{ target }{ archive-suffix }";
        let rendered = render_pkg_template(template, "x86_64-pc-windows-msvc", ".zip");
        assert_eq!(
            rendered,
            "https://example.com/package-name-v1.0.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn binary_ext_is_derived_from_target() {
        let windows = BinstallTargetInfo::from_str("x86_64-pc-windows-msvc").unwrap();
        let macos = BinstallTargetInfo::from_str("aarch64-apple-darwin").unwrap();

        assert_eq!(windows.binary_ext(), ".exe");
        assert_eq!(macos.binary_ext(), "");
    }

    #[test]
    fn render_template_bin_variable() {
        let template =
            "{ repo }/releases/download/v{ version }/{ bin }-v{ version }-{ target }{ archive-suffix }";
        let rendered = render_pkg_template(template, "aarch64-apple-darwin", ".tar.xz");
        let expected = concat!(
            "https://github.com/example/package-name/releases/download/",
            "v1.0.0/tool-v1.0.0-aarch64-apple-darwin.tar.xz",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_template_missing_repo_returns_error() {
        let target_info = BinstallTargetInfo::from_str("x86_64-unknown-linux-gnu").unwrap();
        let binary_ext = target_info.binary_ext();
        let ctx = BinstallTemplateContext {
            name: "tool",
            version: "1.0.0",
            target: "x86_64-unknown-linux-gnu",
            archive_suffix: Some(".tar.gz"),
            binary_ext,
            bin: "tool",
            repo: None,
            target_info: &target_info,
            kind: BinstallTemplateKind::PackageUrl,
        };
        let template = "{ repo }/download/{ name }";
        assert_matches::assert_matches!(
            ctx.render_template(template),
            Err(Error::BinstallTemplateRender { .. })
        );
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
    fn parse_binstall_metadata_raw_from_cargo_toml() {
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
        let meta = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap().unwrap();

        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("{ repo }/releases/download/v{ version }/{ name }_{ target }{ archive-suffix }")
        );
        assert_eq!(meta.pkg_fmt.as_deref(), Some("tgz"));
        assert_eq!(meta.bin_dir.as_deref(), Some("{ bin }{ binary-ext }"));
        assert!(meta.overrides.contains_key("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn render_for_target_applies_exact_target_overrides() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/{ name }-{ target }.tar.gz"
            pkg-fmt = "tgz"

            [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
            pkg-fmt = "zip"
            pkg-url = "https://example.com/{ name }-{ target }.zip"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let raw = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap().unwrap();
        let meta = raw.render_for_target("x86_64-pc-windows-msvc");

        assert_eq!(meta.pkg_fmt.as_deref(), Some("zip"));
        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("https://example.com/{ name }-{ target }.zip")
        );
    }

    #[test]
    fn render_for_target_no_match_leaves_base_metadata() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/{ name }-{ target }.tar.gz"
            pkg-fmt = "tgz"

            [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
            pkg-fmt = "zip"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let raw = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap().unwrap();
        let meta = raw.render_for_target("aarch64-apple-darwin");

        assert_eq!(meta.pkg_fmt.as_deref(), Some("tgz"));
        assert_eq!(
            meta.pkg_url.as_deref(),
            Some("https://example.com/{ name }-{ target }.tar.gz")
        );
    }

    #[test]
    fn render_for_target_partial_override_inherits_base_values() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-url = "https://example.com/default"
            pkg-fmt = "tgz"
            bin-dir = "{ bin }"

            [package.metadata.binstall.overrides.aarch64-apple-darwin]
            pkg-fmt = "txz"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let raw = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap().unwrap();
        let meta = raw.render_for_target("aarch64-apple-darwin");

        assert_eq!(meta.pkg_fmt.as_deref(), Some("txz"));
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
        let result = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn missing_pkg_url_in_metadata() {
        let toml_content = r#"
            [package.metadata.binstall]
            pkg-fmt = "tgz"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();
        let raw = BinstallMetaRaw::try_from_cargo_toml(&doc).unwrap().unwrap();
        let meta = raw.render_for_target("x86_64-unknown-linux-gnu");

        assert!(meta.pkg_url.is_none());
    }

    #[test]
    fn invalid_binstall_metadata_returns_snafu_error() {
        let toml_content = r#"
            [package.metadata]
            binstall = "invalid"
        "#;

        let doc: toml::Value = toml::from_str(toml_content).unwrap();

        assert_matches::assert_matches!(
            BinstallMetaRaw::try_from_cargo_toml(&doc),
            Err(Error::BinstallMetadataInvalidInCargoToml { .. })
        );
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

    #[test]
    fn resolves_using_rendered_bin_dir() {
        let server = MockServer::start();
        let asset = tar_gz_with_binary("package-name-x86_64-unknown-linux-gnu/tool");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/package-name-1.0.0-x86_64-unknown-linux-gnu.tgz");
            then.status(200).body(asset);
        });
        let cargo_toml = format!(
            r#"
[package]
name = "package-name"
version = "1.0.0"
default-run = "tool"
repository = "{}"

[package.metadata.binstall]
pkg-url = "{{ repo }}/{{ name }}-{{ version }}-{{ target }}{{ archive-suffix }}"
pkg-fmt = "tgz"
bin-dir = "{{ name }}-{{ target }}/{{ bin }}{{ binary-ext }}"
"#,
            server.base_url()
        );
        let (krate, _crate_dir) = downloaded_crate_with_toml(&cargo_toml);
        let (provider, _provider_dir) = test_provider(false);

        let result = provider.try_resolve(&krate, "x86_64-unknown-linux-gnu").unwrap();

        let ConclusiveResolution::Found(binary) = result else {
            panic!("expected binstall provider to resolve rendered bin-dir asset")
        };
        assert_eq!(binary.path.file_name().unwrap().to_string_lossy(), "tool");
        assert!(binary.path.exists());
        mock.assert_calls(1);
    }

    #[test]
    fn rendered_bin_dir_uses_windows_binary_ext_from_target() {
        let server = MockServer::start();
        let asset = zip_with_binary("tool.exe");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/package-name-1.0.0-x86_64-pc-windows-msvc.zip");
            then.status(200).body(asset);
        });
        let cargo_toml = format!(
            r#"
[package]
name = "package-name"
version = "1.0.0"
default-run = "tool"
repository = "{}"

[package.metadata.binstall]
pkg-url = "{{ repo }}/{{ name }}-{{ version }}-{{ target }}{{ archive-suffix }}"
pkg-fmt = "zip"
bin-dir = "{{ bin }}{{ binary-ext }}"
"#,
            server.base_url()
        );
        let (krate, _crate_dir) = downloaded_crate_with_toml(&cargo_toml);
        let (provider, _provider_dir) = test_provider(false);

        let result = provider.try_resolve(&krate, "x86_64-pc-windows-msvc").unwrap();

        assert_matches::assert_matches!(result, ConclusiveResolution::Found(_));
        mock.assert_calls(1);
    }

    #[test]
    fn resolves_using_upstream_template_placeholders_in_pkg_url_and_bin_dir() {
        let server = MockServer::start();
        let asset = zip_with_binary("release/.exe/x86_64/tool.exe");
        let mock = server.mock(|when, then| {
            when.method(GET).path("/package-name-x86_64-windows-zip.zip");
            then.status(200).body(asset);
        });
        let cargo_toml = format!(
            r#"
[package]
name = "package-name"
version = "1.0.0"
default-run = "tool"
repository = "{}"

[package.metadata.binstall]
pkg-url = "{{repo}}/{{name}}-{{target-arch}}-{{target-family}}-{{archive-format}}{{archive-suffix}}"
pkg-fmt = "zip"
bin-dir = "release/{{format}}/{{target-arch}}/{{bin}}{{binary-ext}}"
"#,
            server.base_url()
        );
        let (krate, _crate_dir) = downloaded_crate_with_toml(&cargo_toml);
        let (provider, _provider_dir) = test_provider(false);

        let result = provider.try_resolve(&krate, "x86_64-pc-windows-msvc").unwrap();

        assert_matches::assert_matches!(result, ConclusiveResolution::Found(_));
        mock.assert_calls(1);
    }

    #[test]
    fn missing_bin_dir_falls_back_to_bounded_archive_search() {
        let server = MockServer::start();
        let asset = tar_gz_with_binary("release/tool");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/package-name-1.0.0-x86_64-unknown-linux-gnu.tgz");
            then.status(200).body(asset);
        });
        let cargo_toml = format!(
            r#"
[package]
name = "package-name"
version = "1.0.0"
default-run = "tool"
repository = "{}"

[package.metadata.binstall]
pkg-url = "{{ repo }}/{{ name }}-{{ version }}-{{ target }}{{ archive-suffix }}"
pkg-fmt = "tgz"
"#,
            server.base_url()
        );
        let (krate, _crate_dir) = downloaded_crate_with_toml(&cargo_toml);
        let (provider, _provider_dir) = test_provider(false);

        let result = provider.try_resolve(&krate, "x86_64-unknown-linux-gnu").unwrap();

        assert_matches::assert_matches!(result, ConclusiveResolution::Found(_));
        mock.assert_calls(1);
    }

    #[test]
    fn unusable_bin_dir_returns_provider_asset_preparation_error() {
        let server = MockServer::start();
        let asset = tar_gz_with_binary("release/tool");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/package-name-1.0.0-x86_64-unknown-linux-gnu.tgz");
            then.status(200).body(asset);
        });
        let cargo_toml = format!(
            r#"
[package]
name = "package-name"
version = "1.0.0"
default-run = "tool"
repository = "{}"

[package.metadata.binstall]
pkg-url = "{{ repo }}/{{ name }}-{{ version }}-{{ target }}{{ archive-suffix }}"
pkg-fmt = "tgz"
bin-dir = "missing/tool"
"#,
            server.base_url()
        );
        let (krate, _crate_dir) = downloaded_crate_with_toml(&cargo_toml);
        let (provider, _provider_dir) = test_provider(false);

        let result = provider.try_resolve(&krate, "x86_64-unknown-linux-gnu");

        assert_matches::assert_matches!(
            result,
            Err(Error::ProviderAssetPreparationFailed {
                provider: BinaryProvider::Binstall,
                ..
            })
        );
        mock.assert_calls(1);
    }
}
