use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::{builder::BuildOptions, crate_resolver::ResolvedCrate};

/// Messages related to build artifact cache operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BuildCacheMessage {
    CacheLookup {
        name: String,
        version: String,
        options: BuildOptions,
    },
    CacheHit {
        binary_path: PathBuf,
        sbom_path: PathBuf,
    },
    CacheMiss {
        name: String,
        version: String,
    },
    CacheStored {
        binary_path: PathBuf,
        sbom_path: PathBuf,
    },
    SkippingCacheLocalDir,
}

impl BuildCacheMessage {
    pub fn cache_lookup(krate: &ResolvedCrate, options: &BuildOptions) -> Self {
        Self::CacheLookup {
            name: krate.name.clone(),
            version: krate.version.to_string(),
            options: options.clone(),
        }
    }

    pub fn cache_hit(binary_path: &std::path::Path, sbom_path: &std::path::Path) -> Self {
        Self::CacheHit {
            binary_path: binary_path.to_path_buf(),
            sbom_path: sbom_path.to_path_buf(),
        }
    }

    pub fn cache_miss(krate: &ResolvedCrate) -> Self {
        Self::CacheMiss {
            name: krate.name.clone(),
            version: krate.version.to_string(),
        }
    }

    pub fn cache_stored(binary_path: &std::path::Path, sbom_path: &std::path::Path) -> Self {
        Self::CacheStored {
            binary_path: binary_path.to_path_buf(),
            sbom_path: sbom_path.to_path_buf(),
        }
    }

    pub fn skipping_cache_local_dir() -> Self {
        Self::SkippingCacheLocalDir
    }
}

impl From<BuildCacheMessage> for Message {
    fn from(msg: BuildCacheMessage) -> Self {
        Message::BuildCache(msg)
    }
}
