//! Git operations for cgx
//!
//! This module implements a two-tier git caching system inspired by cargo:
//! 1. Git database cache (bare repositories) - one per URL
//! 2. Git checkout cache (working trees) - one per commit
//!
//! This architecture enables:
//! - Targeted refspec fetches which can be much more efficient for large repos
//! - Warm cache reuse when multiple commits from the same repo are used over time
//! - Correct handling of submodules, filters, and line endings via native gix checkout

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use backon::{BlockingRetryable, ExponentialBuilder};
use gix::{ObjectId, bstr::BString, protocol::transport::IsSpuriousError, remote::Direction};
use serde::{Deserialize, Serialize};
use snafu::{IntoError, ResultExt, prelude::*};

use crate::{
    cache::Cache,
    config::HttpConfig,
    messages::{GitMessage, MessageReporter},
};

/// Errors specific to git operations
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub(crate) enum Error {
    #[snafu(display("Git commit hash is invalid: {hash}"))]
    InvalidCommitHash {
        hash: String,
        #[snafu(source(from(gix::hash::decode::Error, Box::new)))]
        source: Box<gix::hash::decode::Error>,
    },

    #[snafu(display("Failed to initialize bare repository at {}", path.display()))]
    InitBareRepo {
        path: PathBuf,
        #[snafu(source(from(gix::init::Error, Box::new)))]
        source: Box<gix::init::Error>,
    },

    #[snafu(display("Failed to open git repository at {}", path.display()))]
    OpenRepo {
        path: PathBuf,
        #[snafu(source(from(gix::open::Error, Box::new)))]
        source: Box<gix::open::Error>,
    },

    #[snafu(display("Failed to resolve git selector: {message}"))]
    ResolveSelector {
        message: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to fetch ref from '{url}'"))]
    FetchRef {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to checkout from database to {}", path.display()))]
    CheckoutFromDb {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to create git directory at {}", path.display()))]
    CreateDirectory { path: PathBuf, source: std::io::Error },

    #[snafu(display("Failed to write marker file at {}", path.display()))]
    WriteMarkerFile { path: PathBuf, source: std::io::Error },
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Git reference selector for fetching specific refs.
///
/// This enum represents the different ways to specify which ref to checkout
/// from a git repository, matching cargo's `GitReference` semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitSelector {
    /// No branch has been explicitly specified, so sse the remote's default branch (fetches HEAD).
    DefaultBranch,
    /// Explicit branch name.
    Branch(String),
    /// Explicit tag name.
    Tag(String),
    /// Explicit commit hash.
    Commit(String),
}

/// Client for git operations using cached bare repositories and checkouts.
///
/// This type orchestrates all git operations through a two-tier cache:
/// - Database cache: bare repos (one per URL) for efficient fetching
/// - Checkout cache: working trees (one per commit) for final source code
///
/// The checkout path returned by [`GitClient::checkout_ref`] IS the final source code,
/// ready to build. No additional copying is needed.
#[derive(Clone, Debug)]
pub(crate) struct GitClient {
    cache: Cache,
    reporter: MessageReporter,
    http_config: HttpConfig,
}

impl GitClient {
    /// Create a new [`GitClient`] with the given cache, message reporter, and HTTP config.
    pub(crate) fn new(cache: Cache, reporter: MessageReporter, http_config: HttpConfig) -> Self {
        Self {
            cache,
            reporter,
            http_config,
        }
    }

    /// Checkout a git ref and return the path to the working tree.
    ///
    /// This uses a two-tier cache:
    /// 1. Bare repository cache (one per URL) - for efficient fetching
    /// 2. Checkout cache (one per commit) - the actual source code
    ///
    /// Returns a tuple of (`checkout_path`, `commit_hash`) where:
    /// - `checkout_path`: Path to the checked-out working tree (the final source code)
    /// - `commit_hash`: Full 40-character SHA-1 hash of the checked-out commit
    pub(crate) fn checkout_ref(&self, url: &str, selector: GitSelector) -> Result<(PathBuf, String)> {
        let db_path = self.ensure_db(url)?;

        // About to check if ref exists locally
        self.reporter.report(|| GitMessage::resolving_ref(url, &selector));

        let commit_str = if let Ok(oid) = resolve_selector(&db_path, &selector) {
            // Ref found locally - no network needed
            let commit_str = oid.to_string();
            self.reporter
                .report(|| GitMessage::ref_found_locally(url, &selector, &commit_str));
            commit_str
        } else {
            // Ref not present - need to fetch from network
            self.reporter.report(|| GitMessage::fetching_repo(url, &selector));
            fetch_ref(&db_path, url, &selector, &self.http_config)?;
            let oid = resolve_selector(&db_path, &selector)?;
            let commit_str = oid.to_string();
            self.reporter.report(|| GitMessage::resolved_ref(&commit_str));
            commit_str
        };

        let checkout_path = self.ensure_checkout(&db_path, url, &commit_str)?;
        Ok((checkout_path, commit_str))
    }

    fn ensure_db(&self, url: &str) -> Result<PathBuf> {
        let db_path = self.cache.git_db_path(url);

        if !db_path.exists() {
            fs::create_dir_all(&db_path).with_context(|_| CreateDirectorySnafu {
                path: db_path.clone(),
            })?;
            init_bare_repo(&db_path)?;
        }

        Ok(db_path)
    }

    fn ensure_checkout(&self, db_path: &Path, url: &str, commit: &str) -> Result<PathBuf> {
        let checkout_path = self.cache.git_checkout_path(url, commit);

        // Check if valid checkout exists (use .cgx-ok marker like cargo's .cargo-ok)
        if checkout_path.exists() && checkout_path.join(".cgx-ok").exists() {
            self.reporter
                .report(|| GitMessage::checkout_exists(commit, &checkout_path));
            return Ok(checkout_path);
        }

        // Need to perform checkout - emit CheckingOut before extraction
        self.reporter
            .report(|| GitMessage::checking_out(commit, &checkout_path));

        fs::create_dir_all(&checkout_path).with_context(|_| CreateDirectorySnafu {
            path: checkout_path.clone(),
        })?;
        let _ = fs::remove_file(checkout_path.join(".cgx-ok"));

        let commit_oid = ObjectId::from_hex(commit.as_bytes())
            .map_err(|e| InvalidCommitHashSnafu { hash: commit }.into_error(e))?;

        checkout_from_db(db_path, commit_oid, &checkout_path)?;

        // Mark as ready
        let marker_path = checkout_path.join(".cgx-ok");
        fs::write(&marker_path, "").with_context(|_| WriteMarkerFileSnafu {
            path: marker_path.clone(),
        })?;

        // Extraction complete
        self.reporter
            .report(|| GitMessage::checkout_complete(&checkout_path));

        Ok(checkout_path)
    }
}

// Low-level git operations (private functions)

fn init_bare_repo(path: &Path) -> Result<()> {
    gix::init_bare(path)
        .map_err(|e| {
            InitBareRepoSnafu {
                path: path.to_path_buf(),
            }
            .into_error(e)
        })
        .map(|_| ())
}

fn fetch_ref(db_path: &Path, url: &str, selector: &GitSelector, http_config: &HttpConfig) -> Result<()> {
    let backoff = ExponentialBuilder::default()
        .with_min_delay(http_config.backoff_base)
        .with_max_delay(http_config.backoff_max)
        .with_max_times(http_config.retries)
        .with_jitter();

    (|| fetch_ref_impl(db_path, url, selector, http_config))
        .retry(backoff)
        .when(is_retryable_error)
        .sleep(std::thread::sleep)
        .call()
}

/// Determine whether a failed fetch should be retried.
///
/// Only [`Error::FetchRef`] errors are candidates. We downcast the boxed source to the three
/// concrete gix error types produced by [`fetch_ref_impl`] and delegate to gix's
/// [`is_spurious()`](gix::protocol::transport::IsSpuriousError::is_spurious), which recursively
/// inspects the error chain for transient conditions: 5xx HTTP status codes (mapped to
/// `ConnectionAborted`), connection timeouts/resets/refused, curl transport failures (DNS, proxy,
/// SSL, HTTP/2, partial file), broken pipe, interrupted, and unexpected EOF. It correctly returns
/// `false` for 4xx errors like 401, 403, and 404.
///
/// One gap: gix maps HTTP 429 (Too Many Requests) to `io::ErrorKind::Other` which
/// `is_spurious()` considers non-retryable. We want to retry on 429, so we also walk the
/// error source chain looking for the `io::Error` with gix's exact format string.
fn is_retryable_error(e: &Error) -> bool {
    let Error::FetchRef { source, .. } = e else {
        return false;
    };
    let err = source.as_ref();

    let spurious = if let Some(e) = err.downcast_ref::<gix::remote::connect::Error>() {
        e.is_spurious()
    } else if let Some(e) = err.downcast_ref::<gix::remote::fetch::prepare::Error>() {
        e.is_spurious()
    } else if let Some(e) = err.downcast_ref::<gix::remote::fetch::Error>() {
        e.is_spurious()
    } else {
        false
    };

    if spurious {
        return true;
    }

    // Check for HTTP 429 by walking the source chain for an io::Error with gix's exact message.
    let mut source: Option<&(dyn std::error::Error)> = Some(err);
    while let Some(current) = source {
        if let Some(io_err) = current.downcast_ref::<std::io::Error>() {
            if io_err.to_string().contains("Received HTTP status 429") {
                return true;
            }
        }
        source = current.source();
    }

    false
}

fn http_config_overrides(http_config: &HttpConfig) -> Vec<BString> {
    let ua = crate::http::user_agent();

    // `connectTimeout` only covers the TCP handshake. To also abort on stalled transfers
    // (server accepted the connection but stops sending data), we set curl's low-speed
    // threshold: if fewer than 1 byte/sec is sustained for `timeout` seconds, curl aborts
    // with CURLE_OPERATION_TIMEDOUT, which gix surfaces as a spurious/retryable error.
    let low_speed_time_secs = http_config.timeout.as_secs().max(1);

    let mut overrides = vec![
        // Controls the git protocol `agent` value (and acts as gix's fallback UA source).
        // We set it so servers/proxies see cgx identity at the git protocol layer, not the
        // default `git/oxide-*`. If omitted, protocol-layer identity reverts to gix default.
        format!("gitoxide.userAgent={ua}").into(),
        // Controls the HTTP backend's configured user-agent option (`http.userAgent`).
        // This keeps transport-level UA settings aligned with cgx identity. If omitted,
        // gix falls back to its default `oxide-*` transport agent for this setting.
        format!("http.userAgent={ua}").into(),
        // Forces an explicit `User-Agent` HTTP header on each request.
        // This is currently required for our observed behavior with gix+curl: without this,
        // requests in integration tests carry `User-Agent: git/oxide-*` instead of cgx UA.
        format!("http.extraHeader=User-Agent: {ua}").into(),
        format!("gitoxide.http.connectTimeout={}", http_config.timeout.as_millis()).into(),
        "http.lowSpeedLimit=1".into(),
        format!("http.lowSpeedTime={low_speed_time_secs}").into(),
    ];

    if let Some(ref proxy) = http_config.proxy {
        overrides.push(format!("http.proxy={proxy}").into());
    }

    overrides
}

fn http_open_options(http_config: &HttpConfig) -> gix::open::Options {
    let overrides = http_config_overrides(http_config);
    gix::open::Options::default().config_overrides(overrides)
}

fn fetch_ref_impl(db_path: &Path, url: &str, selector: &GitSelector, http_config: &HttpConfig) -> Result<()> {
    let repo = gix::open_opts(db_path, http_open_options(http_config)).map_err(|e| {
        OpenRepoSnafu {
            path: db_path.to_path_buf(),
        }
        .into_error(e)
    })?;

    // Build targeted refspec
    let refspec = match selector {
        GitSelector::DefaultBranch => "+HEAD:refs/remotes/origin/HEAD".to_string(),
        GitSelector::Branch(b) => format!("+refs/heads/{b}:refs/remotes/origin/{b}"),
        GitSelector::Tag(t) => format!("+refs/tags/{t}:refs/remotes/origin/tags/{t}"),
        GitSelector::Commit(c) if c.len() == 40 => {
            // Full hash: try targeted fetch (may fail if commit not advertised)
            // NOTE: This implementation assumes git servers support fetching arbitrary commits
            // via protocol v2's allow-any-sha1-in-want capability (true for GitHub, GitLab.com).
            // Servers that don't support this will fail for non-advertised commits.
            // A fallback to broader fetch could be added if needed for restrictive servers.
            // As of this writing I haven't even been able to *find* a public git server that
            // doesn't support fetching arbitrary commits, so this is probably fine.
            format!("+{c}:refs/commit/{c}")
        }
        GitSelector::Commit(_) => {
            // Short hash or potentially unadvertised commit: fetch default branch with history
            // so that we can search the commits and find the one that has this commit hash prefix.
            "+HEAD:refs/remotes/origin/HEAD".to_string()
        }
    };

    // Fetch with explicit refspec
    let remote = repo
        .remote_at(url)
        .map_err(|e| FetchRefSnafu { url: url.to_string() }.into_error(Box::new(e)))?
        .with_refspecs([refspec.as_str()], Direction::Fetch)
        .map_err(|e| FetchRefSnafu { url: url.to_string() }.into_error(Box::new(e)))?;

    let connection = remote
        .connect(Direction::Fetch)
        .map_err(|e| FetchRefSnafu { url: url.to_string() }.into_error(Box::new(e)))?;

    connection
        .prepare_fetch(&mut gix::progress::Discard, Default::default())
        .map_err(|e| FetchRefSnafu { url: url.to_string() }.into_error(Box::new(e)))?
        .receive(&mut gix::progress::Discard, &AtomicBool::new(false))
        .map_err(|e| FetchRefSnafu { url: url.to_string() }.into_error(Box::new(e)))?;

    Ok(())
}

fn resolve_selector(db_path: &Path, selector: &GitSelector) -> Result<ObjectId> {
    let repo = gix::open(db_path).map_err(|e| {
        OpenRepoSnafu {
            path: db_path.to_path_buf(),
        }
        .into_error(e)
    })?;

    let oid = match selector {
        GitSelector::DefaultBranch => {
            let ref_name = "refs/remotes/origin/HEAD";
            let reference = repo.find_reference(ref_name).map_err(|e| {
                ResolveSelectorSnafu {
                    message: format!("Failed to find {}", ref_name),
                }
                .into_error(Box::new(e))
            })?;
            reference
                .into_fully_peeled_id()
                .map_err(|e| {
                    ResolveSelectorSnafu {
                        message: "Failed to peel reference".to_string(),
                    }
                    .into_error(Box::new(e))
                })?
                .detach()
        }
        GitSelector::Branch(b) => {
            let ref_name = format!("refs/remotes/origin/{}", b);
            let reference = repo.find_reference(&ref_name).map_err(|e| {
                ResolveSelectorSnafu {
                    message: format!("Branch '{}' not found", b),
                }
                .into_error(Box::new(e))
            })?;
            reference
                .into_fully_peeled_id()
                .map_err(|e| {
                    ResolveSelectorSnafu {
                        message: format!("Failed to peel branch '{}'", b),
                    }
                    .into_error(Box::new(e))
                })?
                .detach()
        }
        GitSelector::Tag(t) => {
            let ref_name = format!("refs/remotes/origin/tags/{}", t);
            let reference = repo.find_reference(&ref_name).map_err(|e| {
                ResolveSelectorSnafu {
                    message: format!("Tag '{}' not found", t),
                }
                .into_error(Box::new(e))
            })?;
            // Peel annotated tags to get commit
            reference
                .into_fully_peeled_id()
                .map_err(|e| {
                    ResolveSelectorSnafu {
                        message: format!("Failed to peel tag '{}'", t),
                    }
                    .into_error(Box::new(e))
                })?
                .detach()
        }
        GitSelector::Commit(c) => {
            // Use rev_parse_single to resolve both short and full commit hashes
            let spec = repo.rev_parse_single(c.as_bytes()).map_err(|e| {
                ResolveSelectorSnafu {
                    message: format!("Failed to resolve commit '{}'", c),
                }
                .into_error(Box::new(e))
            })?;
            spec.object()
                .map_err(|e| {
                    ResolveSelectorSnafu {
                        message: format!("Failed to get object for commit '{}'", c),
                    }
                    .into_error(Box::new(e))
                })?
                .id
        }
    };

    Ok(oid)
}

fn checkout_from_db(db_path: &Path, commit_oid: ObjectId, dest: &Path) -> Result<()> {
    let repo = gix::open(db_path).map_err(|e| {
        OpenRepoSnafu {
            path: db_path.to_path_buf(),
        }
        .into_error(e)
    })?;

    // Get commit and tree
    let commit = repo.find_commit(commit_oid).map_err(|e| {
        CheckoutFromDbSnafu {
            path: dest.to_path_buf(),
        }
        .into_error(Box::new(e))
    })?;

    let tree_id = commit.tree_id().map_err(|e| {
        CheckoutFromDbSnafu {
            path: dest.to_path_buf(),
        }
        .into_error(Box::new(e))
    })?;

    // Create index from tree
    let mut index = repo.index_from_tree(&tree_id).map_err(|e| {
        CheckoutFromDbSnafu {
            path: dest.to_path_buf(),
        }
        .into_error(Box::new(e))
    })?;

    // Get checkout options (handles .gitattributes, filters, line endings)
    let options = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(|e| {
            CheckoutFromDbSnafu {
                path: dest.to_path_buf(),
            }
            .into_error(Box::new(e))
        })?;

    // Use gix native checkout
    gix::worktree::state::checkout(
        &mut index,
        dest,
        repo.objects.clone(),
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        options,
    )
    .map_err(|e| {
        CheckoutFromDbSnafu {
            path: dest.to_path_buf(),
        }
        .into_error(Box::new(e))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use tempfile::TempDir;

    use super::*;

    fn test_git_client() -> (GitClient, TempDir) {
        let (temp_dir, config) = crate::config::create_test_env();
        let reporter = MessageReporter::null();
        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = GitClient::new(cache, reporter, config.http);
        (git_client, temp_dir)
    }

    mod http_config_overrides {
        use super::*;

        fn overrides_to_strings(overrides: Vec<BString>) -> Vec<String> {
            overrides
                .into_iter()
                .map(|override_value| String::from_utf8_lossy(override_value.as_ref()).into_owned())
                .collect()
        }

        #[test]
        fn includes_user_agent_and_timeout_settings() {
            let (_temp_dir, config) = crate::config::create_test_env();
            let overrides = overrides_to_strings(http_config_overrides(&config.http));

            assert!(overrides.iter().any(|o| o.starts_with("gitoxide.userAgent=")));
            assert!(overrides.iter().any(|o| o.starts_with("http.userAgent=")));
            assert!(
                overrides
                    .iter()
                    .any(|o| o.starts_with("gitoxide.http.connectTimeout="))
            );
            assert!(overrides.iter().any(|o| o == "http.lowSpeedLimit=1"));
        }

        #[test]
        fn includes_proxy_when_configured() {
            let (_temp_dir, mut config) = crate::config::create_test_env();
            config.http.proxy = Some("http://proxy.example:8080".to_string());
            let overrides = overrides_to_strings(http_config_overrides(&config.http));

            assert!(
                overrides
                    .iter()
                    .any(|o| o == "http.proxy=http://proxy.example:8080")
            );
        }

        #[test]
        fn omits_proxy_when_not_configured() {
            let (_temp_dir, config) = crate::config::create_test_env();
            let overrides = overrides_to_strings(http_config_overrides(&config.http));

            assert!(!overrides.iter().any(|o| o.starts_with("http.proxy=")));
        }
    }

    mod checkout_ref {
        use super::*;

        #[test]
        fn checkout_default_branch() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let (checkout_path, _commit_hash) =
                git_client.checkout_ref(url, GitSelector::DefaultBranch).unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join(".cgx-ok").exists());
        }

        #[test]
        fn checkout_specific_branch() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let (checkout_path, _commit_hash) = git_client
                .checkout_ref(url, GitSelector::Branch("main".to_string()))
                .unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join("Cargo.toml").exists());
        }

        #[test]
        fn checkout_specific_tag() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let (checkout_path, commit_hash) = git_client
                .checkout_ref(url, GitSelector::Tag("v6.0.0".to_string()))
                .unwrap();
            assert!(checkout_path.exists());

            // I happen to know what the commit hash is for this tag
            assert_eq!("28d2bb04326d7036514245d73f10fb72b9ed108c", &commit_hash);
        }

        /// Checkout a specific commit that I happen to know is advertised by the remote, because
        /// this commit is associated with the v6.0.0 tag.
        #[test]
        fn checkout_specific_advertised_commit() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            // Known stable commit corresponding to tag v6.0.0
            let commit = "28d2bb04326d7036514245d73f10fb72b9ed108c";

            let (checkout_path, commit_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(commit.to_string()))
                .unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join(".cgx-ok").exists());
            assert_eq!(commit, &commit_hash);

            // Try again with a fresh client and clean cache, with a short commit; expect the same
            // result
            drop(_temp);
            let (git_client, _temp) = test_git_client();
            let short_commit = &commit[..7];
            let (checkout_path, commit_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(short_commit.to_string()))
                .unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join(".cgx-ok").exists());
            assert_eq!(commit, &commit_hash);
        }

        /// Checkout a specific commit that I happen to know just a regular commot that is NOT
        /// adverstised by the remote.  This triggers fallback fetch logic and thus must be tested
        /// separately from advertised commits.
        #[test]
        fn checkout_specific_non_advertised_commit() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            // This is a random commit from 2024-07-02 that I don't think is advertised
            let commit = "6cf75d569bd0dd33a041e37c59cb75d28664bd7b";

            let (checkout_path, commit_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(commit.to_string()))
                .unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join(".cgx-ok").exists());
            assert_eq!(commit, &commit_hash);

            // Try again with a fresh client and clean cache, with a short commit; expect the same
            // result
            drop(_temp);
            let (git_client, _temp) = test_git_client();
            let short_commit = &commit[..7];
            let (checkout_path, commit_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(short_commit.to_string()))
                .unwrap();
            assert!(checkout_path.exists());
            assert!(checkout_path.join(".cgx-ok").exists());
            assert_eq!(commit, &commit_hash);
        }

        #[test]
        fn cache_reuse_same_commit() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";
            let commit = "28d2bb04326d7036514245d73f10fb72b9ed108c";

            // First checkout
            let (first_checkout_path, first_checkout_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(commit.to_string()))
                .unwrap();

            // Second checkout should hit cache
            let (second_checkout_path, second_checkout_hash) = git_client
                .checkout_ref(url, GitSelector::Commit(commit.to_string()))
                .unwrap();

            assert_eq!(commit, &first_checkout_hash);
            assert_eq!(commit, &second_checkout_hash);

            assert_eq!(first_checkout_path, second_checkout_path);
        }

        #[test]
        fn nonexistent_branch() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let result = git_client.checkout_ref(
                url,
                GitSelector::Branch("this-branch-does-not-exist-xyzzy".to_string()),
            );
            assert_matches!(result, Err(Error::FetchRef { .. }));
        }

        #[test]
        fn nonexistent_tag() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let result = git_client.checkout_ref(url, GitSelector::Tag("v999.999.999".to_string()));
            assert_matches!(result, Err(Error::FetchRef { .. }));
        }

        #[test]
        fn nonexistent_commit() {
            let (git_client, _temp) = test_git_client();
            let url = "https://github.com/rust-lang/rustlings.git";

            let result = git_client.checkout_ref(
                url,
                GitSelector::Commit("0000000000000000000000000000000000000000".to_string()),
            );
            assert_matches!(result, Err(Error::FetchRef { .. }));
        }
    }

    /// Integration tests exercising the git fetch retry logic against a local mock HTTP server.
    ///
    /// These live here rather than in `cgx/tests/integration/` because the functions under test
    /// ([`fetch_ref`], [`is_retryable_error`]) and their gix error types are `pub(crate)` and
    /// not part of cgx-core's public API.
    mod integration {
        use std::time::Duration;

        use httpmock::prelude::*;

        use super::*;

        /// Returns an HTTP configuration with near-zero retry delays so retry behavior can be
        /// exercised without slowing the test suite down.
        fn fast_retry_config() -> HttpConfig {
            HttpConfig {
                retries: 2,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                timeout: Duration::from_secs(30),
                ..Default::default()
            }
        }

        /// Returns an HTTP configuration that disables retries so one-shot request behavior can be
        /// asserted deterministically.
        fn no_retry_config() -> HttpConfig {
            HttpConfig {
                retries: 0,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(1),
                timeout: Duration::from_secs(5),
                ..Default::default()
            }
        }

        /// Creates an empty bare repository to use as the destination object database for fetch
        /// integration tests.
        fn test_bare_repo() -> (TempDir, PathBuf) {
            let temp_dir = TempDir::new().unwrap();
            let repo_path = temp_dir.path().join("bare.git");
            fs::create_dir_all(&repo_path).unwrap();
            init_bare_repo(&repo_path).unwrap();
            (temp_dir, repo_path)
        }

        #[test]
        fn server_503_is_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(503);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = fast_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(3);
        }

        #[test]
        fn server_500_is_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(500);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = fast_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(3);
        }

        #[test]
        fn server_429_is_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(429);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = fast_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(3);
        }

        #[test]
        fn server_403_is_not_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(403);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = fast_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(1);
        }

        #[test]
        fn server_404_is_not_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(404);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = fast_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(1);
        }

        #[test]
        fn connection_timeout_is_retried() {
            let server = MockServer::start();
            let mock = server.mock(|_when, then| {
                then.status(200).delay(Duration::from_secs(3));
            });

            let (_temp, db_path) = test_bare_repo();
            let config = HttpConfig {
                retries: 2,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                timeout: Duration::from_secs(1),
                ..Default::default()
            };
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(3);
        }

        #[test]
        fn user_agent_is_applied_to_git_http_requests() {
            let server = MockServer::start();
            let expected_ua = crate::http::user_agent();
            let mock = server.mock(|when, then| {
                when.method(GET)
                    .path("/repo.git/info/refs")
                    .query_param("service", "git-upload-pack")
                    .header("User-Agent", expected_ua.as_str());
                then.status(500);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = no_retry_config();
            let result = fetch_ref(
                &db_path,
                &server.url("/repo.git"),
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(1);
        }

        #[test]
        fn proxy_setting_is_used_for_git_http_requests() {
            let server = MockServer::start();
            let expected_ua = crate::http::user_agent();
            let mock = server.mock(|when, then| {
                when.method(GET)
                    .host("example.invalid")
                    .path("/repo.git/info/refs")
                    .query_param("service", "git-upload-pack")
                    .header("User-Agent", expected_ua.as_str());
                then.status(502);
            });

            let (_temp, db_path) = test_bare_repo();
            let config = HttpConfig {
                proxy: Some(server.base_url()),
                ..no_retry_config()
            };

            let result = fetch_ref(
                &db_path,
                "http://example.invalid/repo.git",
                &GitSelector::DefaultBranch,
                &config,
            );

            assert_matches!(result, Err(Error::FetchRef { .. }));
            mock.assert_calls(1);
        }
    }
}
