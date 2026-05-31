use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Message;
use crate::git::GitSelector;

/// Messages related to git operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GitMessage {
    /// About to check if the ref exists in the local bare repo
    ResolvingRef { url: String, selector: GitSelector },
    /// The ref was already present in the local bare repo (no fetch needed)
    RefFoundLocally {
        url: String,
        selector: GitSelector,
        commit: String,
    },
    /// Starting a network fetch because the ref was not present locally
    FetchingRepo { url: String, selector: GitSelector },
    /// The ref was resolved to a commit (only emitted after fetching)
    ResolvedRef { commit: String },
    /// Extracting a working tree from the bare repo
    CheckingOut { commit: String, path: PathBuf },
    /// Extraction completed (only emitted after [`CheckingOut`](Self::CheckingOut))
    CheckoutComplete { path: PathBuf },
    /// The checkout directory already exists (no extraction needed)
    CheckoutExists { commit: String, path: PathBuf },
}

impl GitMessage {
    pub fn resolving_ref(url: &str, selector: &GitSelector) -> Self {
        Self::ResolvingRef {
            url: url.to_string(),
            selector: selector.clone(),
        }
    }

    pub fn ref_found_locally(url: &str, selector: &GitSelector, commit: &str) -> Self {
        Self::RefFoundLocally {
            url: url.to_string(),
            selector: selector.clone(),
            commit: commit.to_string(),
        }
    }

    pub fn fetching_repo(url: &str, selector: &GitSelector) -> Self {
        Self::FetchingRepo {
            url: url.to_string(),
            selector: selector.clone(),
        }
    }

    pub fn resolved_ref(commit: &str) -> Self {
        Self::ResolvedRef {
            commit: commit.to_string(),
        }
    }

    pub fn checking_out(commit: &str, path: &std::path::Path) -> Self {
        Self::CheckingOut {
            commit: commit.to_string(),
            path: path.to_path_buf(),
        }
    }

    pub fn checkout_complete(path: &std::path::Path) -> Self {
        Self::CheckoutComplete {
            path: path.to_path_buf(),
        }
    }

    pub fn checkout_exists(commit: &str, path: &std::path::Path) -> Self {
        Self::CheckoutExists {
            commit: commit.to_string(),
            path: path.to_path_buf(),
        }
    }
}

impl From<GitMessage> for Message {
    fn from(msg: GitMessage) -> Self {
        Message::Git(msg)
    }
}
