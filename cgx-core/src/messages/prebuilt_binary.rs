use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::{bin_resolver::ResolvedBinary, config::BinaryProvider, crate_resolver::ResolvedCrate};

/// Messages related to prebuilt binary resolution and binary resolution cache operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PrebuiltBinaryMessage {
    /// Looking up prebuilt binary resolution in cache
    CacheLookup { krate: ResolvedCrate },
    /// Found cached prebuilt binary resolution
    CacheHit { path: PathBuf, provider: BinaryProvider },
    /// No cached prebuilt binary resolution found
    CacheMiss { krate: ResolvedCrate },
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
    /// Prebuilt binary cannot be used due to build customization
    DisqualifiedDueToCustomization { reason: String },
    /// Prebuilt binaries are disabled in config
    PrebuiltBinariesDisabled,
}

impl PrebuiltBinaryMessage {
    pub fn cache_lookup(krate: &ResolvedCrate) -> Self {
        Self::CacheLookup { krate: krate.clone() }
    }

    pub fn cache_hit(path: &std::path::Path, provider: BinaryProvider) -> Self {
        Self::CacheHit {
            path: path.to_path_buf(),
            provider,
        }
    }

    pub fn cache_miss(krate: &ResolvedCrate) -> Self {
        Self::CacheMiss { krate: krate.clone() }
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
