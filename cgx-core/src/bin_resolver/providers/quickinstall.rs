#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use snafu::ResultExt;

use super::{ArchiveFormat, Provider};
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedCrate,
    downloader::DownloadedCrate,
    error,
    http::HttpClient,
    messages::PrebuiltBinaryMessage,
    target::TargetTriple,
};

pub(in crate::bin_resolver) struct QuickinstallProvider {
    reporter: crate::messages::MessageReporter,
    cache_dir: PathBuf,
    http_client: HttpClient,
}

impl QuickinstallProvider {
    pub(in crate::bin_resolver) fn new(
        reporter: crate::messages::MessageReporter,
        cache_dir: PathBuf,
        http_client: HttpClient,
    ) -> Self {
        Self {
            reporter,
            cache_dir,
            http_client,
        }
    }

    fn construct_url(krate: &ResolvedCrate, target: &TargetTriple) -> String {
        let base = "https://github.com/cargo-bins/cargo-quickinstall/releases/download";
        let tag = format!("{}-{}", krate.name, krate.version);
        format!("{base}/{tag}/{tag}-{}.tar.gz", target.as_str())
    }
}

impl Provider for QuickinstallProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::Quickinstall
    }

    fn try_resolve(&self, krate: &DownloadedCrate, target: &TargetTriple) -> Result<ConclusiveResolution> {
        let temp_dir = tempfile::tempdir().context(error::TempDirCreationSnafu)?;

        let archive_path = temp_dir.path().join(ArchiveFormat::TarGz.canonical_filename());

        // Try the exact host target first, then each ABI-compatible fallback target; the first one
        // that quickinstall actually publishes wins.
        let mut resolved_url = None;
        for candidate_target in target.compatible_targets() {
            let url = Self::construct_url(&krate.resolved, &candidate_target);

            self.reporter
                .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::Quickinstall));

            match self.http_client.try_download_to_file(&url, &archive_path) {
                Ok(true) => {
                    resolved_url = Some(url);
                    break;
                }
                Ok(false) => {}
                Err(e) => return Err(e),
            }
        }

        let Some(url) = resolved_url else {
            self.reporter.report(|| {
                PrebuiltBinaryMessage::provider_has_no_binary(
                    BinaryProvider::Quickinstall,
                    "binary not found",
                )
            });
            return Ok(ConclusiveResolution::Nonexistent);
        };

        // TODO(#80): verify .sig (minisign) signatures when support is added

        let binary_name = krate.default_binary_name()?;
        let extract_dir = temp_dir.path().join("extracted");
        let binary_path = super::extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &[&binary_name],
            &extract_dir,
        )
        .map_err(|source| {
            super::provider_asset_preparation_failed(BinaryProvider::Quickinstall, &url, source)
        })?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("quickinstall")
            .join(&krate.resolved.name)
            .join(krate.resolved.version.to_string())
            .join(target.as_str());

        std::fs::create_dir_all(&final_dir).with_context(|_| error::IoSnafu {
            path: final_dir.clone(),
        })?;

        let final_path = final_dir.join(format!("{}{}", binary_name, target.binary_ext()));
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
            krate: krate.resolved.clone(),
            provider: BinaryProvider::Quickinstall,
            path: final_path,
        }))
    }
}
