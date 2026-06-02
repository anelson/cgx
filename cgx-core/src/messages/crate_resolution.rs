use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use super::Message;
use crate::{crate_resolver::ResolvedCrate, cratespec::CrateSpec};

/// Messages related to crate resolution and resolution cache operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CrateResolutionMessage {
    CacheLookup {
        spec: CrateSpec,
    },
    CacheHit {
        path: PathBuf,
        age_secs: u64,
        ttl_remaining_secs: u64,
    },
    CacheMiss {
        spec: CrateSpec,
    },
    CacheStale {
        spec: CrateSpec,
        age_secs: u64,
    },
    Resolving {
        spec: CrateSpec,
    },
    Resolved {
        resolved: ResolvedCrate,
    },
    CacheStored {
        path: PathBuf,
    },
    UsingStaleFallback {
        spec: CrateSpec,
        age_secs: u64,
    },
}

impl CrateResolutionMessage {
    pub fn cache_lookup(spec: &CrateSpec) -> Self {
        Self::CacheLookup { spec: spec.clone() }
    }

    pub fn cache_hit(path: &std::path::Path, age: Duration, ttl_remaining: Duration) -> Self {
        Self::CacheHit {
            path: path.to_path_buf(),
            age_secs: age.as_secs(),
            ttl_remaining_secs: ttl_remaining.as_secs(),
        }
    }

    pub fn cache_miss(spec: &CrateSpec) -> Self {
        Self::CacheMiss { spec: spec.clone() }
    }

    pub fn cache_stale(spec: &CrateSpec, age: Duration) -> Self {
        Self::CacheStale {
            spec: spec.clone(),
            age_secs: age.as_secs(),
        }
    }

    pub fn resolving(spec: &CrateSpec) -> Self {
        Self::Resolving { spec: spec.clone() }
    }

    pub fn resolved(resolved: &ResolvedCrate) -> Self {
        Self::Resolved {
            resolved: resolved.clone(),
        }
    }

    pub fn cache_stored(path: &std::path::Path) -> Self {
        Self::CacheStored {
            path: path.to_path_buf(),
        }
    }

    pub fn using_stale_fallback(spec: &CrateSpec, age: Duration) -> Self {
        Self::UsingStaleFallback {
            spec: spec.clone(),
            age_secs: age.as_secs(),
        }
    }
}

impl From<CrateResolutionMessage> for Message {
    fn from(msg: CrateResolutionMessage) -> Self {
        Message::CrateResolution(msg)
    }
}
