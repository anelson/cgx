use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::crate_resolver::{ResolvedCrate, ResolvedSource};

/// Messages related to source code downloading and source cache operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SourceMessage {
    CacheLookup {
        name: String,
        version: String,
        source: ResolvedSource,
    },
    CacheHit {
        path: PathBuf,
    },
    CacheMiss {
        name: String,
        version: String,
        source: ResolvedSource,
    },
    Downloading {
        name: String,
        version: String,
        source: ResolvedSource,
    },
    Downloaded {
        path: PathBuf,
    },
    CacheStored {
        path: PathBuf,
    },
}

impl SourceMessage {
    pub fn cache_lookup(resolved: &ResolvedCrate) -> Self {
        Self::CacheLookup {
            name: resolved.name.clone(),
            version: resolved.version.to_string(),
            source: resolved.source.clone(),
        }
    }

    pub fn cache_hit(path: &std::path::Path) -> Self {
        Self::CacheHit {
            path: path.to_path_buf(),
        }
    }

    pub fn cache_miss(resolved: &ResolvedCrate) -> Self {
        Self::CacheMiss {
            name: resolved.name.clone(),
            version: resolved.version.to_string(),
            source: resolved.source.clone(),
        }
    }

    pub fn downloading(resolved: &ResolvedCrate) -> Self {
        Self::Downloading {
            name: resolved.name.clone(),
            version: resolved.version.to_string(),
            source: resolved.source.clone(),
        }
    }

    pub fn downloaded(path: &std::path::Path) -> Self {
        Self::Downloaded {
            path: path.to_path_buf(),
        }
    }

    pub fn cache_stored(path: &std::path::Path) -> Self {
        Self::CacheStored {
            path: path.to_path_buf(),
        }
    }
}

impl From<SourceMessage> for Message {
    fn from(msg: SourceMessage) -> Self {
        Message::Source(msg)
    }
}
