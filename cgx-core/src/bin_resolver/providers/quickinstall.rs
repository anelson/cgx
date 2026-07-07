use std::path::PathBuf;

use tempfile::TempDir;

use super::{ArchiveFormat, Provider};
use crate::{
    Result,
    bin_resolver::{ConclusiveResolution, ResolvedBinary},
    config::BinaryProvider,
    crate_resolver::ResolvedCrate,
    downloader::DownloadedCrate,
    http::HttpClient,
    messages::PrebuiltBinaryMessage,
    target::TargetTriple,
};

pub(in crate::bin_resolver) struct QuickinstallProvider {
    reporter: crate::messages::MessageReporter,
    staging_dir: PathBuf,
    http_client: HttpClient,
}

impl QuickinstallProvider {
    pub(in crate::bin_resolver) fn new(
        reporter: crate::messages::MessageReporter,
        staging_dir: &TempDir,
        http_client: HttpClient,
    ) -> Self {
        Self {
            reporter,
            staging_dir: staging_dir
                .path()
                .join(<&'static str>::from(BinaryProvider::Quickinstall)),
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
        let work_dir = super::recreate_staging_work_dir(&self.staging_dir, &krate.resolved)?;

        let archive_path = work_dir.join(ArchiveFormat::TarGz.canonical_filename());

        // Try the exact host target first, then each ABI-compatible fallback target; the first one
        // that quickinstall actually publishes wins.
        let mut resolved = None;
        for candidate_target in target.compatible_targets() {
            // Quickinstall names its release assets after exact rustc target triples, so probing
            // an asset-name pseudo-target (eg `universal-apple-darwin`) is pointless. The host
            // itself is exempt: even when its triple is unparsable it is still a real rustc
            // triple that quickinstall may publish.
            if candidate_target.triple().is_none() && candidate_target != *target {
                continue;
            }

            let url = Self::construct_url(&krate.resolved, &candidate_target);

            self.reporter
                .report(|| PrebuiltBinaryMessage::downloading_binary(&url, BinaryProvider::Quickinstall));

            match self.http_client.try_download_to_file(&url, &archive_path) {
                Ok(true) => {
                    resolved = Some((url, candidate_target));
                    break;
                }
                Ok(false) => {}
                Err(e) => return Err(e),
            }
        }

        let Some((url, matched_target)) = resolved else {
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
        let extract_dir = work_dir.join("extracted");
        let binary_path = super::extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &[&binary_name],
            &extract_dir,
        )
        .map_err(|source| {
            super::provider_asset_preparation_failed(BinaryProvider::Quickinstall, &url, source)
        })?;

        let staged_path = super::stage_extracted_binary(&work_dir, &binary_name, target, &binary_path)?;
        Ok(ConclusiveResolution::Found(ResolvedBinary {
            krate: krate.resolved.clone(),
            provider: BinaryProvider::Quickinstall,
            path: staged_path,
            target: matched_target.to_string(),
        }))
    }
}
