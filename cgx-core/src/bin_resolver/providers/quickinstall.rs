#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use snafu::ResultExt;

use super::{ArchiveFormat, Provider};
use crate::{
    Result,
    bin_resolver::{BinaryResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedCrate,
    downloader::DownloadedCrate,
    error,
    http::{Bytes, HttpClient},
    messages::PrebuiltBinaryMessage,
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

    fn construct_url(krate: &ResolvedCrate, platform: &str) -> String {
        let base = "https://github.com/cargo-bins/cargo-quickinstall/releases/download";
        let tag = format!("{}-{}", krate.name, krate.version);
        format!("{base}/{tag}/{tag}-{platform}.tar.gz")
    }

    fn download_file(&self, url: &str) -> Result<Option<Bytes>> {
        self.http_client.try_download(url)
    }
}

impl Provider for QuickinstallProvider {
    fn kind(&self) -> BinaryProvider {
        BinaryProvider::Quickinstall
    }

    fn try_resolve(&self, krate: &DownloadedCrate, platform: &str) -> Result<BinaryResolution> {
        let url = Self::construct_url(&krate.resolved, platform);

        self.reporter
            .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::Quickinstall));

        let data = match self.download_file(&url) {
            Ok(Some(data)) => data,
            Ok(None) => {
                self.reporter.report(|| {
                    PrebuiltBinaryMessage::provider_has_no_binary(
                        BinaryProvider::Quickinstall,
                        "binary not found",
                    )
                });
                return Ok(BinaryResolution::Nonexistent);
            }
            Err(e) if e.is_transient_http_error() => {
                return Ok(BinaryResolution::Inconclusive { source: Box::new(e) });
            }
            Err(e) => return Err(e),
        };

        // TODO(#80): verify .sig (minisign) signatures when support is added

        let temp_dir = tempfile::tempdir().with_context(|_| error::TempDirCreationSnafu {
            parent: self.cache_dir.clone(),
        })?;

        let archive_path = temp_dir.path().join(ArchiveFormat::TarGz.canonical_filename());
        std::fs::write(&archive_path, &data).with_context(|_| error::IoSnafu {
            path: archive_path.clone(),
        })?;

        let binary_name = krate.default_binary_name()?;
        let extract_dir = temp_dir.path().join("extracted");
        let binary_path =
            super::extract_binary(&archive_path, ArchiveFormat::TarGz, &binary_name, &extract_dir)?;

        let final_dir = self
            .cache_dir
            .join("binaries")
            .join("quickinstall")
            .join(&krate.resolved.name)
            .join(krate.resolved.version.to_string())
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

        Ok(BinaryResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::Quickinstall,
            path: final_path,
        }))
    }
}
