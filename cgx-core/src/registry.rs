use std::{fs, io::ErrorKind};

use backon::{BlockingRetryable, ExponentialBuilder};
use semver::Version;
use snafu::ResultExt;
use tame_index::{
    CRATES_IO_HTTP_INDEX, Error as TameIndexError, HttpError as TameHttpError, IndexKrate, IndexLocation,
    IndexPath, IndexUrl, KrateName, SparseIndex,
    index::{IndexConfig, RemoteSparseIndex},
    utils::flock::{FileLock, LockOptions},
};

use crate::{
    Result,
    config::HttpConfig,
    cratespec::RegistrySource,
    error,
    http::{HttpClient, SMALL_DOWNLOAD_LIMIT_BYTES},
};

/// File name of the sparse registry configuration stored at the root of Cargo's index cache.
const SPARSE_INDEX_CONFIG_FILENAME: &str = "config.json";

/// Base crates.io API URL recorded in Cargo's crates.io sparse index config.
const CRATES_IO_API_URL: &str = "https://crates.io";

/// crates.io download API URL recorded in Cargo's crates.io sparse index config.
const CRATES_IO_DOWNLOAD_API_URL: &str = "https://crates.io/api/v1/crates";

/// Result of looking up a download URL for a specific crate version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DownloadUrlLookup {
    Url(String),
    CrateNotFound,
    VersionNotFound,
    UrlUnavailable,
}

/// Lightweight version metadata exposed to callers without leaking tame-index internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryVersionInfo {
    pub(crate) version: String,
    pub(crate) yanked: bool,
}

/// Shared registry client for all sparse-index operations.
///
/// This centralizes all tame-index usage (index URL resolution, lock acquisition,
/// sparse index fetch behavior, and retry policy).
pub(crate) struct RegistryClient {
    remote_index: RemoteSparseIndex,
    lock: FileLock,
    http_config: HttpConfig,
    http_client: HttpClient,
    index_path: tame_index::PathBuf,
    index_url: String,
}

impl RegistryClient {
    /// Build a registry client for crates.io (None) or a custom registry source.
    pub(crate) fn new(
        source: Option<&RegistrySource>,
        http_client: &HttpClient,
        http: &HttpConfig,
    ) -> Result<Self> {
        // Resolve IndexUrl based on source type.
        let index_url = resolve_index_url(source).context(error::RegistrySnafu)?;

        // Use the sparse index for this registry and connect to it remotely.
        // NOTE: We currently assume remote registries only.
        let (index_path, resolved_index_url) = IndexLocation::new(index_url)
            .into_parts()
            .context(error::RegistrySnafu)?;
        let sparse_index = SparseIndex::new(IndexLocation {
            url: IndexUrl::from(resolved_index_url.as_str()),
            root: IndexPath::Exact(index_path.clone()),
            cargo_version: None,
        })
        .context(error::RegistrySnafu)?;

        // Use the same cache lock as cargo itself to maximize cache hits and compatibility.
        // The tradeoff is potential contention if cargo is simultaneously reading/updating
        // the package cache, but this is generally preferable to maintaining a separate
        // sparse index cache and lock ecosystem.
        let lock = LockOptions::cargo_package_lock(None)
            .context(error::RegistrySnafu)?
            .lock(|_| None)
            .context(error::RegistrySnafu)?;

        let remote_index = RemoteSparseIndex::new(sparse_index, http_client.inner().clone());

        Ok(Self {
            remote_index,
            lock,
            http_config: http.clone(),
            http_client: http_client.clone(),
            index_path,
            index_url: resolved_index_url,
        })
    }

    /// Fetch available versions for a crate from the sparse index.
    ///
    /// Returns `Ok(None)` when the crate is not present in the selected registry.
    pub(crate) fn crate_versions(
        &self,
        name: &str,
        offline: bool,
    ) -> Result<Option<Vec<RegistryVersionInfo>>> {
        let Some(krate) = self.fetch_krate(name, offline)? else {
            return Ok(None);
        };

        Ok(Some(
            krate
                .versions
                .iter()
                .map(|v| RegistryVersionInfo {
                    version: v.version.to_string(),
                    yanked: v.is_yanked(),
                })
                .collect(),
        ))
    }

    /// Fetch a direct tarball download URL for an exact crate version.
    pub(crate) fn crate_download_url(
        &self,
        name: &str,
        version: &Version,
        offline: bool,
    ) -> Result<DownloadUrlLookup> {
        let Some(krate) = self.fetch_krate(name, offline)? else {
            return Ok(DownloadUrlLookup::CrateNotFound);
        };

        // Find the specific version we need.
        let Some(index_version) = krate
            .versions
            .iter()
            .find(|v| Version::parse(&v.version).ok().is_some_and(|ver| &ver == version))
        else {
            return Ok(DownloadUrlLookup::VersionNotFound);
        };

        // Get the index config to construct the download URL.
        let index_config = self.load_or_bootstrap_index_config(offline)?;

        // Get download URL for this exact version.
        let Some(download_url) = index_version.download_url(&index_config) else {
            return Ok(DownloadUrlLookup::UrlUnavailable);
        };

        Ok(DownloadUrlLookup::Url(download_url))
    }

    fn fetch_krate(&self, name: &str, offline: bool) -> Result<Option<IndexKrate>> {
        // In offline mode, use cached_krate which only queries the local cache.
        // Otherwise, use krate which may perform network I/O and can trigger retries.
        if offline {
            let krate_name = KrateName::try_from(name).context(error::RegistrySnafu)?;
            return self
                .remote_index
                .cached_krate(krate_name, &self.lock)
                .context(error::RegistrySnafu);
        }

        let operation = || {
            let krate_name = KrateName::try_from(name)?;
            self.remote_index.krate(krate_name, true, &self.lock)
        };

        run_with_retry(&self.http_config, name, operation).context(error::RegistrySnafu)
    }

    /// Load the sparse registry config needed for tarball download URL construction.
    ///
    /// Cargo stores `config.json` beside the sparse index cache, but
    /// [`RemoteSparseIndex`] only fetches per-crate index entries. It's quite possible, especially
    /// when using prebuilt binaries or as part of the Github Action `anelson/cgx` running on a
    /// fresh runner, that `cargo` hasn't run yet and thus the sparse index cache is empty.
    ///
    /// This function will detect that case and attempt to bootstrap the index config.
    fn load_or_bootstrap_index_config(&self, offline: bool) -> Result<IndexConfig> {
        match self.remote_index.index.index_config() {
            Ok(config) => Ok(config),
            Err(source) if !offline && Self::is_missing_index_config(&source) => {
                let (config, contents) = self.bootstrap_index_config()?;
                self.persist_index_config(&contents);
                Ok(config)
            }
            Err(source) => Err(source).context(error::RegistrySnafu),
        }
    }

    /// Obtain (or generate) a sparse registry config for a cache that does not have `config.json`
    /// yet.
    ///
    /// For crates.io, Cargo's documented config is stable and [`IndexConfig`] already has a
    /// crates.io-specific download URL fast path keyed off this exact `dl` value. For other
    /// registries, we have to fetch the config from the sparse index itself.
    fn bootstrap_index_config(&self) -> Result<(IndexConfig, Vec<u8>)> {
        if self.is_crates_io() {
            let config = Self::crates_io_index_config();
            let contents = serde_json::to_vec(&config).context(error::JsonSnafu)?;
            return Ok((config, contents));
        }

        let config_url = self.sparse_config_url();
        let Some(contents) = self
            .http_client
            .try_download_bytes(&config_url, SMALL_DOWNLOAD_LIMIT_BYTES)?
        else {
            return Err(error::HttpStatusSnafu {
                url: config_url,
                status: reqwest::StatusCode::NOT_FOUND.as_u16(),
            }
            .build());
        };
        let config = serde_json::from_slice(&contents).context(error::JsonSnafu)?;
        Ok((config, contents.to_vec()))
    }

    /// Persist the registry config into Cargo's sparse index cache on a best-effort basis.
    ///
    /// The in-memory config is enough for the current `cgx` run, so write failures are logged and
    /// ignored, matching Cargo's own tolerance for config cache writes. The write goes through a
    /// temporary file and rename so a failed write cannot leave `config.json` truncated.
    fn persist_index_config(&self, contents: &[u8]) {
        let path = self.index_path.join(SPARSE_INDEX_CONFIG_FILENAME);
        let temp_path = self.index_path.join(format!(
            "{SPARSE_INDEX_CONFIG_FILENAME}.{}.tmp",
            uuid::Uuid::new_v4()
        ));

        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent.as_std_path()) {
                tracing::debug!(
                    "Failed to create sparse index config directory '{}': {}",
                    parent,
                    err
                );
                return;
            }
        }

        if let Err(err) = fs::write(temp_path.as_std_path(), contents) {
            tracing::debug!("Failed to write sparse index config '{}': {}", temp_path, err);
            return;
        }

        if let Err(err) = fs::rename(temp_path.as_std_path(), path.as_std_path()) {
            let _ = fs::remove_file(temp_path.as_std_path());
            tracing::debug!("Failed to move sparse index config into '{}': {}", path, err);
        }
    }

    /// Build the standard crates.io sparse index config.
    fn crates_io_index_config() -> IndexConfig {
        IndexConfig {
            dl: CRATES_IO_DOWNLOAD_API_URL.to_string(),
            api: Some(CRATES_IO_API_URL.to_string()),
            auth_required: false,
        }
    }

    /// Return true when this client uses Cargo's canonical crates.io sparse index.
    fn is_crates_io(&self) -> bool {
        self.index_url == CRATES_IO_HTTP_INDEX
    }

    /// Build the sparse registry config endpoint URL for custom registries.
    fn sparse_config_url(&self) -> String {
        if self.index_url.ends_with('/') {
            format!("{}{}", self.index_url, SPARSE_INDEX_CONFIG_FILENAME)
        } else {
            format!("{}/{}", self.index_url, SPARSE_INDEX_CONFIG_FILENAME)
        }
    }

    /// Return true only for a missing local sparse `config.json` error.
    fn is_missing_index_config(err: &TameIndexError) -> bool {
        matches!(
            err,
            TameIndexError::IoPath(source, path)
                if source.kind() == ErrorKind::NotFound
                    && path.file_name() == Some(SPARSE_INDEX_CONFIG_FILENAME)
        )
    }
}

/// Resolve an index URL for crates.io or a custom registry source.
fn resolve_index_url(source: Option<&RegistrySource>) -> std::result::Result<IndexUrl<'_>, TameIndexError> {
    match source {
        None => IndexUrl::crates_io(
            None, // config_root: search standard locations
            None, // cargo_home: use $CARGO_HOME
            None, // cargo_version: auto-detect version
        ),
        Some(RegistrySource::Named(registry_name)) => IndexUrl::for_registry_name(
            None, // config_root: search standard locations
            None, // cargo_home: use $CARGO_HOME
            registry_name,
        ),
        Some(RegistrySource::IndexUrl(url)) => Ok(IndexUrl::from(url.as_str())),
    }
}

fn build_registry_backoff(http: &HttpConfig) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(http.backoff_base)
        .with_max_delay(http.backoff_max)
        .with_max_times(http.retries)
        .with_jitter()
}

fn run_with_retry<T, F>(
    http: &HttpConfig,
    crate_name: &str,
    operation: F,
) -> std::result::Result<T, TameIndexError>
where
    F: FnMut() -> std::result::Result<T, TameIndexError>,
{
    operation
        .retry(build_registry_backoff(http))
        .when(is_retryable_tame_error)
        .notify(|err, dur| {
            tracing::debug!(
                "Sparse index request for crate '{}' failed, retrying in {:?}: {:?}",
                crate_name,
                dur,
                err
            );
        })
        .call()
}

fn is_retryable_tame_error(err: &TameIndexError) -> bool {
    match err {
        TameIndexError::Http(TameHttpError::Reqwest(source)) => {
            source.is_connect() || source.is_timeout() || source.is_request()
        }
        TameIndexError::Http(TameHttpError::StatusCode { code, .. }) => {
            code.as_u16() == 429 || code.is_server_error()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    fn fast_http_config(retries: usize) -> HttpConfig {
        HttpConfig {
            retries,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(2),
            ..Default::default()
        }
    }

    #[test]
    fn test_retry_classifier_for_status_codes() {
        let rate_limited = TameIndexError::Http(TameHttpError::StatusCode {
            code: tame_index::external::http::StatusCode::TOO_MANY_REQUESTS,
            msg: "rate limited",
        });
        assert!(is_retryable_tame_error(&rate_limited));

        let server_error = TameIndexError::Http(TameHttpError::StatusCode {
            code: tame_index::external::http::StatusCode::SERVICE_UNAVAILABLE,
            msg: "service unavailable",
        });
        assert!(is_retryable_tame_error(&server_error));

        let unauthorized = TameIndexError::Http(TameHttpError::StatusCode {
            code: tame_index::external::http::StatusCode::UNAUTHORIZED,
            msg: "unauthorized",
        });
        assert!(!is_retryable_tame_error(&unauthorized));
    }

    #[test]
    fn test_retryable_error_retried_then_succeeds() {
        let attempts = AtomicUsize::new(0);

        let result = run_with_retry(&fast_http_config(2), "serde", || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                Err(TameIndexError::Http(TameHttpError::StatusCode {
                    code: tame_index::external::http::StatusCode::TOO_MANY_REQUESTS,
                    msg: "rate limited",
                }))
            } else {
                Ok("ok")
            }
        });

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_non_retryable_error_not_retried() {
        let attempts = AtomicUsize::new(0);

        let result = run_with_retry(&fast_http_config(3), "serde", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TameIndexError::Http(TameHttpError::StatusCode {
                code: tame_index::external::http::StatusCode::UNAUTHORIZED,
                msg: "unauthorized",
            }))
        });

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_retryable_error_exhausts_retry_budget() {
        let attempts = AtomicUsize::new(0);

        let result = run_with_retry(&fast_http_config(2), "serde", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TameIndexError::Http(TameHttpError::StatusCode {
                code: tame_index::external::http::StatusCode::SERVICE_UNAVAILABLE,
                msg: "service unavailable",
            }))
        });

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_zero_retries_means_single_attempt() {
        let attempts = AtomicUsize::new(0);

        let result = run_with_retry(&fast_http_config(0), "serde", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TameIndexError::Http(TameHttpError::StatusCode {
                code: tame_index::external::http::StatusCode::SERVICE_UNAVAILABLE,
                msg: "service unavailable",
            }))
        });

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
