use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::{bin_resolver::ResolvedBinary, config::BinaryProvider, crate_resolver::ResolvedCrate};

/// Why a cached binary-resolution entry was discarded because the enabled set of binary providers
/// changed since the entry was written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderChangeReason {
    /// A provider is now enabled that was not enabled when the cache entry was created, so the
    /// cached outcome might no longer be correct (this provider was never consulted).
    RequiredProviderNotEnabled(BinaryProvider),
    /// The provider that produced the cached binary is no longer enabled, so the binary must not be
    /// served.
    SourceProviderDisabled(BinaryProvider),
}

/// Messages related to prebuilt binary resolution and binary resolution cache operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PrebuiltBinaryMessage {
    /// Looking up prebuilt binary resolution in cache
    CacheLookup { krate: ResolvedCrate },
    /// A positive prebuilt binary cache hit: a previously-resolved binary was found in the cache.
    PositiveCacheHit {
        krate: ResolvedCrate,
        path: PathBuf,
        provider: BinaryProvider,
    },
    /// A negative prebuilt binary cache hit: we previously determined, conclusively, that no
    /// prebuilt binary is available for this crate.
    NegativeCacheHit { krate: ResolvedCrate },
    /// No cached prebuilt binary resolution found
    CacheMiss { krate: ResolvedCrate },
    /// A cached prebuilt binary resolution existed but was discarded because the enabled set of
    /// binary providers changed, so the crate will be re-resolved.
    CacheInvalidatedByProviderChange {
        krate: ResolvedCrate,
        reason: ProviderChangeReason,
    },
    /// Checking a specific binary provider for prebuilt binaries
    CheckingProvider {
        krate: ResolvedCrate,
        provider: BinaryProvider,
    },
    /// A provider was checked but had no binary available
    ProviderHasNoBinary {
        provider: BinaryProvider,
        reason: String,
    },
    /// A provider failed before it could make a conclusive determination.
    ProviderFailed {
        provider: BinaryProvider,
        reason: String,
    },
    /// Downloading a prebuilt binary from a provider
    DownloadingBinary { url: String, provider: BinaryProvider },
    /// Verifying checksum of downloaded binary
    VerifyingChecksum { expected: String },
    /// Checksum verification successful
    ChecksumVerified,
    /// Successfully resolved a prebuilt binary
    Resolved { binary: ResolvedBinary },
    /// Stored resolved binary information in cache
    CacheStored { path: PathBuf },
    /// No prebuilt binary found from any provider
    NoBinaryFound {
        krate: ResolvedCrate,
        reasons: Vec<String>,
    },
    /// Prebuilt binary resolution was inconclusive: a transient failure (such as a rate limit or
    /// network error) prevented at least one provider from providing a definitive answer, so the
    /// result is not cached and the crate falls back to building from source.
    ResolutionInconclusive { reason: String },
    /// Prebuilt binary cannot be used due to build customization
    DisqualifiedDueToCustomization { reason: String },
    /// Prebuilt binaries are disabled in config
    PrebuiltBinariesDisabled,
}

impl PrebuiltBinaryMessage {
    pub fn cache_lookup(krate: &ResolvedCrate) -> Self {
        Self::CacheLookup { krate: krate.clone() }
    }

    pub fn positive_cache_hit(
        krate: &ResolvedCrate,
        path: &std::path::Path,
        provider: BinaryProvider,
    ) -> Self {
        Self::PositiveCacheHit {
            krate: krate.clone(),
            path: path.to_path_buf(),
            provider,
        }
    }

    pub fn negative_cache_hit(krate: &ResolvedCrate) -> Self {
        Self::NegativeCacheHit { krate: krate.clone() }
    }

    pub fn cache_miss(krate: &ResolvedCrate) -> Self {
        Self::CacheMiss { krate: krate.clone() }
    }

    pub fn cache_invalidated_by_provider_change(krate: &ResolvedCrate, reason: ProviderChangeReason) -> Self {
        Self::CacheInvalidatedByProviderChange {
            krate: krate.clone(),
            reason,
        }
    }

    pub fn checking_provider(krate: &ResolvedCrate, provider: BinaryProvider) -> Self {
        Self::CheckingProvider {
            krate: krate.clone(),
            provider,
        }
    }

    pub fn provider_has_no_binary(provider: BinaryProvider, reason: impl Into<String>) -> Self {
        Self::ProviderHasNoBinary {
            provider,
            reason: reason.into(),
        }
    }

    pub fn provider_failed(provider: BinaryProvider, reason: impl Into<String>) -> Self {
        Self::ProviderFailed {
            provider,
            reason: reason.into(),
        }
    }

    pub fn downloading_binary(url: impl Into<String>, provider: BinaryProvider) -> Self {
        Self::DownloadingBinary {
            url: url.into(),
            provider,
        }
    }

    pub fn verifying_checksum(expected: impl Into<String>) -> Self {
        Self::VerifyingChecksum {
            expected: expected.into(),
        }
    }

    pub fn checksum_verified() -> Self {
        Self::ChecksumVerified
    }

    pub fn resolved(binary: &ResolvedBinary) -> Self {
        Self::Resolved {
            binary: binary.clone(),
        }
    }

    pub fn cache_stored(path: &std::path::Path) -> Self {
        Self::CacheStored {
            path: path.to_path_buf(),
        }
    }

    pub fn no_binary_found(krate: &ResolvedCrate, reasons: Vec<String>) -> Self {
        Self::NoBinaryFound {
            krate: krate.clone(),
            reasons,
        }
    }

    pub fn resolution_inconclusive(reason: impl Into<String>) -> Self {
        Self::ResolutionInconclusive {
            reason: reason.into(),
        }
    }

    pub fn disqualified_due_to_customization(reason: impl Into<String>) -> Self {
        Self::DisqualifiedDueToCustomization {
            reason: reason.into(),
        }
    }

    pub fn prebuilt_binaries_disabled() -> Self {
        Self::PrebuiltBinariesDisabled
    }
}

impl From<PrebuiltBinaryMessage> for Message {
    fn from(msg: PrebuiltBinaryMessage) -> Self {
        Message::PrebuiltBinary(msg)
    }
}
