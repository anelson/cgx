use std::path::PathBuf;

use semver::VersionReq;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use url::Url;

use crate::{
    Result,
    cli::CliArgs,
    config::{Config, ToolConfig},
    error,
    git::GitSelector,
};

/// A specification of a crate that the user wants to execute.
///
/// Note that "crate" here doesn't necessarily mean "crate on Crates.io".  We support various ways
/// of referring to a crate to run, which is why this enum type is needed.  It abstracts away the
/// various ways the user might specify a crate to run.  Ultimately all of these need to be
/// resolved to a path in the local filesystem, controlled by cgx, from which we can build and run.
///
/// ## Versioning
///
/// For crate specs that point to registries (which store multiple versions of a crate), the
/// default is to choose the latest version.  If a version is specified, then the most recent
/// version that matches the specification is chosen.  If no such version exists then an error
/// ocurrs.
///
/// For crate specs that point to local paths, forges, or git repos, there is no choice of
/// version; the version of the crate is whatever it is at the specified location.  In those cases,
/// if the `version` field is present, it is validated against the version found at the location,
/// and if it's not compatible then an error ocurrs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrateSpec {
    /// A crate on Crates.io, specified by its name and optional version.
    CratesIo {
        name: String,
        version: Option<VersionReq>,
    },

    /// A crate on some other registry, specified by its name and optional version.
    Registry {
        /// The registry source (either a named registry or a direct index URL)
        source: RegistrySource,
        name: String,
        version: Option<VersionReq>,
    },

    /// A crate in a git repository, specified by the repository URL and optional branch, tag, or
    /// commit hash.
    ///
    /// The `name` field is optional. If omitted, it will be discovered from the repository
    /// (which must contain exactly one crate). If the repository contains multiple crates,
    /// the name must be specified.
    ///
    /// If the `version` field is present, the crate found at the specified repo must have a
    /// version that is compatible with the version specification or an error ocurrs.
    Git {
        repo: String,
        selector: GitSelector,
        name: Option<String>,
        version: Option<VersionReq>,
    },

    /// A crate in a repo in some software Forge, specified by its repo, optional path within that
    /// repo, and optional branch, tag, or commit hash.
    ///
    /// The `name` field is optional. If omitted, it will be discovered from the repository
    /// (which must contain exactly one crate). If the repository contains multiple crates,
    /// the name must be specified.
    Forge {
        /// A repository within a software forge
        forge: Forge,

        /// A branch, tag, or commit hash within the repository
        selector: GitSelector,

        name: Option<String>,

        version: Option<VersionReq>,
    },

    /// A crate in a local directory, specified by the path to the directory containing the crate's
    /// `Cargo.toml` or a workspace `Cargo.toml` to which the crate belongs.
    ///
    /// The `name` field is optional. If omitted, it will be discovered from the path
    /// (which must contain exactly one crate). If the path contains multiple crates
    /// (i.e., a workspace), the name must be specified.
    LocalDir {
        path: PathBuf,
        name: Option<String>,
        version: Option<VersionReq>,
    },
}

impl CrateSpec {
    /// Load a crate spec from the command line, respecting config-based overrides.
    ///
    /// This method applies config-based transformations and overrides:
    /// 1. Alias resolution: Maps short names to full crate names (e.g., `rg` → `ripgrep`)
    /// 2. Tool pinning: Applies version pinning from config for known tools
    /// 3. Default registry: Uses config's default registry when no registry specified
    ///
    /// Priority order for version selection:
    /// 1. CLI `--version` flag (highest)
    /// 2. `@version` suffix in crate name
    /// 3. Config tool pinning
    /// 4. Latest version (lowest)
    pub fn load(config: &Config, args: &CliArgs) -> Result<Self> {
        // Parse the base crate spec from CLI args, including special cargo handling
        let (name, at_version) = if let Some(crate_spec) = &args.crate_spec {
            if crate_spec == "cargo" && !args.args.is_empty() {
                // Special case: `cgx cargo deny` -> crate name is `cargo-deny`
                let subcommand = &args.args[0];
                let (subcommand_name, subcommand_version) = Self::parse_crate_name_and_version(subcommand)?;
                let cargo_crate_name = format!("cargo-{}", subcommand_name);
                (Some(cargo_crate_name), subcommand_version)
            } else {
                let (n, v) = Self::parse_crate_name_and_version(crate_spec)?;
                (Some(n), v)
            }
        } else {
            (None, None)
        };

        // Apply alias resolution from config
        let name = name.map(|n| config.aliases.get(&n).cloned().unwrap_or(n));

        // Reconcile version from @version syntax and --version flag
        let flag_version = args
            .version
            .as_ref()
            .filter(|v| !v.is_empty())
            .map(|s| s.as_str());

        let cli_version = match (at_version.as_deref(), flag_version) {
            (Some(at_ver), Some(flag_ver)) => {
                if at_ver != flag_ver {
                    return error::ConflictingVersionsSnafu {
                        at_version: at_ver,
                        flag_version: flag_ver,
                    }
                    .fail();
                }
                Some(
                    VersionReq::parse(at_ver)
                        .with_context(|_| error::InvalidVersionReqSnafu { version: at_ver })?,
                )
            }
            (Some(at_ver), None) => Some(
                VersionReq::parse(at_ver)
                    .with_context(|_| error::InvalidVersionReqSnafu { version: at_ver })?,
            ),
            (None, Some(flag_ver)) => Some(
                VersionReq::parse(flag_ver)
                    .with_context(|_| error::InvalidVersionReqSnafu { version: flag_ver })?,
            ),
            (None, None) => None,
        };

        // Apply tool pinning from config if no CLI version specified
        let version = if cli_version.is_none() {
            if let Some(ref tool_name) = name {
                config
                    .tools
                    .get(tool_name)
                    .and_then(|tool_config| match tool_config {
                        ToolConfig::Version(v) | ToolConfig::Detailed { version: Some(v), .. } => {
                            VersionReq::parse(v).ok()
                        }
                        ToolConfig::Detailed { version: None, .. } => None,
                    })
            } else {
                None
            }
        } else {
            cli_version
        };

        // Construct GitSelector from CLI flags
        let git_selector = match (&args.branch, &args.tag, &args.rev) {
            (Some(branch), None, None) => GitSelector::Branch(branch.clone()),
            (None, Some(tag), None) => GitSelector::Tag(tag.clone()),
            (None, None, Some(rev)) => GitSelector::Commit(rev.clone()),
            (None, None, None) => GitSelector::DefaultBranch,
            _ => unreachable!("BUG: clap should enforce mutual exclusivity"),
        };

        let is_git_source = args.git.is_some() || args.github.is_some() || args.gitlab.is_some();

        if !matches!(git_selector, GitSelector::DefaultBranch) && !is_git_source {
            return error::GitSelectorWithoutGitSourceSnafu.fail();
        }

        // Construct the appropriate CrateSpec variant based on source flags
        if let Some(git_url) = &args.git {
            if let Some(forge) = Forge::try_parse_from_url(git_url) {
                Ok(CrateSpec::Forge {
                    forge,
                    selector: git_selector.clone(),
                    name,
                    version,
                })
            } else {
                Ok(CrateSpec::Git {
                    repo: git_url.clone(),
                    selector: git_selector.clone(),
                    name,
                    version,
                })
            }
        } else if let Some(registry) = &args.registry {
            let name = name.context(error::MissingCrateParameterSnafu)?;
            Ok(CrateSpec::Registry {
                source: RegistrySource::Named(registry.clone()),
                name,
                version,
            })
        } else if let Some(index_str) = &args.index {
            let name = name.context(error::MissingCrateParameterSnafu)?;
            let index_url =
                Url::parse(index_str).with_context(|_| error::InvalidUrlSnafu { url: index_str })?;
            Ok(CrateSpec::Registry {
                source: RegistrySource::IndexUrl(index_url),
                name,
                version,
            })
        } else if let Some(path) = &args.path {
            Ok(CrateSpec::LocalDir {
                path: path.clone(),
                name,
                version,
            })
        } else if let Some(github_repo) = &args.github {
            let (owner, repo) = Self::parse_owner_repo(github_repo)?;
            let custom_url = if let Some(url_str) = &args.github_url {
                Some(Url::parse(url_str).with_context(|_| error::InvalidUrlSnafu { url: url_str })?)
            } else {
                None
            };
            Ok(CrateSpec::Forge {
                forge: Forge::GitHub {
                    custom_url,
                    owner,
                    repo,
                },
                selector: git_selector.clone(),
                name,
                version,
            })
        } else if let Some(gitlab_repo) = &args.gitlab {
            let (owner, repo) = Self::parse_owner_repo(gitlab_repo)?;
            let custom_url = if let Some(url_str) = &args.gitlab_url {
                Some(Url::parse(url_str).with_context(|_| error::InvalidUrlSnafu { url: url_str })?)
            } else {
                None
            };
            Ok(CrateSpec::Forge {
                forge: Forge::GitLab {
                    custom_url,
                    owner,
                    repo,
                },
                selector: git_selector.clone(),
                name,
                version,
            })
        } else {
            // No CLI source flags - check tool config, then default_registry, then crates.io

            // First check if tool config specifies a source
            if let Some(ref tool_name) = name {
                if let Some(tool_config) = config.tools.get(tool_name) {
                    match tool_config {
                        ToolConfig::Detailed {
                            git: Some(git_url),
                            branch,
                            tag,
                            rev,
                            ..
                        } => {
                            // Tool config specifies git source
                            let selector = match (branch.as_ref(), tag.as_ref(), rev.as_ref()) {
                                (Some(b), None, None) => GitSelector::Branch(b.clone()),
                                (None, Some(t), None) => GitSelector::Tag(t.clone()),
                                (None, None, Some(r)) => GitSelector::Commit(r.clone()),
                                _ => GitSelector::DefaultBranch,
                            };

                            if let Some(forge) = Forge::try_parse_from_url(git_url) {
                                return Ok(CrateSpec::Forge {
                                    forge,
                                    selector,
                                    name,
                                    version,
                                });
                            } else {
                                return Ok(CrateSpec::Git {
                                    repo: git_url.clone(),
                                    selector,
                                    name,
                                    version,
                                });
                            }
                        }
                        ToolConfig::Detailed {
                            registry: Some(reg), ..
                        } => {
                            // Tool config specifies registry
                            let name = name.context(error::MissingCrateParameterSnafu)?;
                            return Ok(CrateSpec::Registry {
                                source: RegistrySource::Named(reg.clone()),
                                name,
                                version,
                            });
                        }
                        ToolConfig::Detailed { path: Some(p), .. } => {
                            // Tool config specifies local path
                            return Ok(CrateSpec::LocalDir {
                                path: p.clone(),
                                name,
                                version,
                            });
                        }
                        _ => {
                            // Tool config doesn't specify source - fall through to defaults
                        }
                    }
                }
            }

            // No tool config source - use default_registry or crates.io

            // At this point all of the possible configurations in which an explicit crate name is
            // optional have been eliminated, so we require a crate name.
            let name = name.context(error::MissingCrateParameterSnafu)?;

            if let Some(ref default_registry) = config.default_registry {
                // Use config's default registry
                Ok(CrateSpec::Registry {
                    source: RegistrySource::Named(default_registry.clone()),
                    name,
                    version,
                })
            } else {
                // Use crates.io
                Ok(CrateSpec::CratesIo { name, version })
            }
        }
    }

    /// Parse a crate name that may include an @version suffix.
    ///
    /// Examples:
    /// - `"ripgrep"` → `("ripgrep", None)`
    /// - `"ripgrep@14"` → `("ripgrep", Some("14"))`
    fn parse_crate_name_and_version(spec: &str) -> Result<(String, Option<String>)> {
        if let Some((name, version)) = spec.split_once('@') {
            Ok((name.to_string(), Some(version.to_string())))
        } else {
            Ok((spec.to_string(), None))
        }
    }

    /// Parse owner/repo format used by GitHub and GitLab.
    fn parse_owner_repo(repo_str: &str) -> Result<(String, String)> {
        if let Some((owner, repo)) = repo_str.split_once('/') {
            if owner.is_empty() || repo.is_empty() {
                return error::InvalidRepoFormatSnafu { repo: repo_str }.fail();
            }
            Ok((owner.to_string(), repo.to_string()))
        } else {
            error::InvalidRepoFormatSnafu { repo: repo_str }.fail()
        }
    }

    /// Get the arguments that should be passed to the executed binary.
    ///
    /// For the special case of `cgx cargo <subcommand>`, the first argument is consumed
    /// as part of the crate spec (to form `cargo-<subcommand>`), so we skip it.
    /// Otherwise, all trailing args are passed to the binary.
    pub fn get_binary_args(args: &CliArgs) -> Vec<std::ffi::OsString> {
        let skip = if args.crate_spec.as_deref() == Some("cargo") && !args.args.is_empty() {
            // Skip the first arg (the cargo subcommand name)
            1
        } else {
            0
        };

        args.args
            .iter()
            .skip(skip)
            .map(std::ffi::OsString::from)
            .collect()
    }
}

/// Specifies how to identify a registry source.
///
/// Registries can be specified either by a named configuration in `.cargo/config.toml` or by
/// directly providing the index URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RegistrySource {
    /// A named registry configured in `.cargo/config.toml` (corresponds to `--registry`).
    Named(String),

    /// A direct registry index URL (corresponds to `--index`).
    IndexUrl(Url),
}

/// Supported software forges where crates can be hosted
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Forge {
    GitHub {
        /// Custom URL for Github Enterprise instances; None for github.com
        custom_url: Option<Url>,
        owner: String,
        repo: String,
    },
    GitLab {
        /// Custom URL for self-hosted GitLab instances; None for gitlab.com
        custom_url: Option<Url>,
        owner: String,
        repo: String,
    },
}

impl Forge {
    /// The HTTPS URL to the repository root (no `.git` suffix).
    ///
    /// This is the URL that is intended for humans to view to look at the repo in a browser.
    /// This is also typically what would be placed in the Cargo.toml `repository` field for a
    /// crate.
    ///
    /// Use this for API and release URLs. Use [`Forge::git_url`] when a
    /// `.git`-suffixed clone URL is needed.
    pub fn repo_url(&self) -> String {
        match self {
            Forge::GitHub {
                custom_url,
                owner,
                repo,
            }
            | Forge::GitLab {
                custom_url,
                owner,
                repo,
            } => {
                let base = custom_url
                    .as_ref()
                    .map_or(self.default_host(), |u| u.as_str().trim_end_matches('/'));
                format!("{}/{}/{}", base, owner, repo)
            }
        }
    }

    /// Convert this forge reference into a git URL
    pub fn git_url(&self) -> String {
        format!("{}.git", self.repo_url())
    }

    fn default_host(&self) -> &'static str {
        match self {
            Forge::GitHub { .. } => "https://github.com",
            Forge::GitLab { .. } => "https://gitlab.com",
        }
    }

    /// Attempt to parse a URL into a reference to a repo in a forge
    ///
    /// When a known forge like Github or Gitlab is used, treating it as a forge as opposed to a
    /// generic Git URL is important because we can use that forge's API to look for binary
    /// releases for the crate, which if found will dramatically speed up installation.
    ///
    /// Only HTTPS urls are recognized, and only URLs that point to the root of a repository, on
    /// the forges that we have API support for.
    pub fn try_parse_from_url(git_url: &str) -> Option<Self> {
        let url = Url::parse(git_url).ok()?;

        if url.scheme() != "https" {
            return None;
        }

        let host = url.host_str()?;

        let path = url.path();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if segments.len() != 2 {
            return None;
        }

        let owner = segments[0].to_string();
        let mut repo = segments[1].to_string();

        if repo.ends_with(".git") {
            repo = repo[..repo.len() - 4].to_string();
        }

        match host {
            "github.com" => Some(Forge::GitHub {
                custom_url: None,
                owner,
                repo,
            }),
            "gitlab.com" => Some(Forge::GitLab {
                custom_url: None,
                owner,
                repo,
            }),
            _other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;
    use crate::{
        cli::CliArgs,
        config::{Config, ToolConfig},
    };

    /// Test that config aliases are resolved before processing the crate spec.
    ///
    /// Simulated config:
    /// ```toml
    /// [aliases]
    /// rg = "ripgrep"
    /// ```
    ///
    /// Command: `cgx rg`
    ///
    /// Expected: Alias `rg` resolves to `ripgrep`, producing a crates.io spec for ripgrep.
    #[test]
    fn test_alias_resolution() {
        let mut config = Config::default();
        config.aliases.insert("rg".to_string(), "ripgrep".to_string());

        let args = CliArgs::parse_from_test_args(["rg"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, .. } if name == "ripgrep"
        );
    }

    /// Test that tools can be pinned to specific versions using simple string syntax.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// ripgrep = "14.0"
    /// ```
    ///
    /// Command: `cgx ripgrep`
    ///
    /// Expected: Uses pinned version 14.0 from config.
    #[test]
    fn test_tool_version_pinning_simple() {
        let mut config = Config::default();
        config
            .tools
            .insert("ripgrep".to_string(), ToolConfig::Version("14.0".to_string()));

        let args = CliArgs::parse_from_test_args(["ripgrep"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: Some(ref v) }
            if name == "ripgrep" && v == &VersionReq::parse("14.0").unwrap()
        );
    }

    /// Test that tools can be pinned to specific versions using detailed table syntax.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// ripgrep = { version = "14.0" }
    /// ```
    ///
    /// Command: `cgx ripgrep`
    ///
    /// Expected: Uses pinned version 14.0 from detailed config.
    #[test]
    fn test_tool_version_pinning_detailed() {
        let mut config = Config::default();
        config.tools.insert(
            "ripgrep".to_string(),
            ToolConfig::Detailed {
                version: Some("14.0".to_string()),
                features: None,
                registry: None,
                git: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["ripgrep"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: Some(ref v) }
            if name == "ripgrep" && v == &VersionReq::parse("14.0").unwrap()
        );
    }

    /// Test that tools can specify a custom registry in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { version = "1.0", registry = "my-registry" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Registry`] with the specified registry name.
    /// This should behave as if the user had run `cgx my-tool --registry my-registry --version
    /// 1.0`.
    #[test]
    fn test_tool_with_registry() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: Some("1.0".to_string()),
                registry: Some("my-registry".to_string()),
                features: None,
                git: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Registry {
                source: RegistrySource::Named(ref reg),
                ref name,
                version: Some(ref v)
            } if reg == "my-registry" && name == "my-tool" && v == &VersionReq::parse("1.0").unwrap()
        );
    }

    /// Test that tools can specify a git URL in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://example.com/repo.git" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Git`] with the specified repo URL.
    /// This should behave as if the user had run `cgx my-tool --git https://example.com/repo.git`.
    #[test]
    fn test_tool_with_git_url() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://example.com/repo.git".to_string()),
                branch: None,
                registry: None,
                features: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Git {
                ref repo,
                selector: GitSelector::DefaultBranch,
                name: Some(ref n),
                version: None
            } if repo == "https://example.com/repo.git" && n == "my-tool"
        );
    }

    /// Test that GitHub URLs in config are recognized and produce [`CrateSpec::Forge`] variants.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://github.com/owner/repo.git" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Forge`] with GitHub forge, enabling potential use of
    /// GitHub Releases API for binary downloads.
    #[test]
    fn test_tool_with_github_url() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://github.com/owner/repo.git".to_string()),
                tag: None,
                registry: None,
                features: None,
                branch: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Forge {
                forge: Forge::GitHub { custom_url: None, ref owner, ref repo },
                selector: GitSelector::DefaultBranch,
                name: Some(ref n),
                version: None
            } if owner == "owner" && repo == "repo" && n == "my-tool"
        );
    }

    /// Test that GitLab URLs in config are recognized and produce [`CrateSpec::Forge`] variants.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://gitlab.com/owner/repo.git" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Forge`] with GitLab forge, enabling potential use of
    /// GitLab Releases API for binary downloads.
    #[test]
    fn test_tool_with_gitlab_url() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://gitlab.com/owner/repo.git".to_string()),
                tag: None,
                registry: None,
                features: None,
                branch: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Forge {
                forge: Forge::GitLab { custom_url: None, ref owner, ref repo },
                selector: GitSelector::DefaultBranch,
                name: Some(ref n),
                version: None
            } if owner == "owner" && repo == "repo" && n == "my-tool"
        );
    }

    /// Test that tools can specify a local filesystem path in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { path = "/some/path" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::LocalDir`] with the specified path.
    /// This should behave as if the user had run `cgx my-tool --path /some/path`.
    #[test]
    fn test_tool_with_path() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                path: Some(PathBuf::from("/some/path")),
                registry: None,
                features: None,
                git: None,
                branch: None,
                tag: None,
                rev: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::LocalDir {
                ref path,
                name: Some(ref n),
                version: None
            } if path == &PathBuf::from("/some/path") && n == "my-tool"
        );
    }

    /// Test that tools can specify git + branch selector in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://example.com/repo.git", branch = "develop" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Git`] with [`GitSelector::Branch`].
    /// Equivalent to: `cgx my-tool --git https://example.com/repo.git --branch develop`.
    #[test]
    fn test_tool_with_git_and_branch() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://example.com/repo.git".to_string()),
                branch: Some("develop".to_string()),
                registry: None,
                features: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Git {
                ref repo,
                selector: GitSelector::Branch(ref b),
                name: Some(ref n),
                version: None
            } if repo == "https://example.com/repo.git" && b == "develop" && n == "my-tool"
        );
    }

    /// Test that tools can specify git + tag selector in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://example.com/repo.git", tag = "v1.0.0" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Git`] with [`GitSelector::Tag`].
    /// Equivalent to: `cgx my-tool --git https://example.com/repo.git --tag v1.0.0`.
    #[test]
    fn test_tool_with_git_and_tag() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://example.com/repo.git".to_string()),
                tag: Some("v1.0.0".to_string()),
                registry: None,
                features: None,
                branch: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Git {
                ref repo,
                selector: GitSelector::Tag(ref t),
                name: Some(ref n),
                version: None
            } if repo == "https://example.com/repo.git" && t == "v1.0.0" && n == "my-tool"
        );
    }

    /// Test that tools can specify git + rev (commit) selector in config.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { git = "https://example.com/repo.git", rev = "abc123" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::Git`] with [`GitSelector::Commit`].
    /// Equivalent to: `cgx my-tool --git https://example.com/repo.git --rev abc123`.
    #[test]
    fn test_tool_with_git_and_rev() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                git: Some("https://example.com/repo.git".to_string()),
                rev: Some("abc123".to_string()),
                registry: None,
                features: None,
                branch: None,
                tag: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Git {
                ref repo,
                selector: GitSelector::Commit(ref c),
                name: Some(ref n),
                version: None
            } if repo == "https://example.com/repo.git" && c == "abc123" && n == "my-tool"
        );
    }

    /// Test that CLI `--version` flag takes precedence over config tool version.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// ripgrep = "14.0"
    /// ```
    ///
    /// Command: `cgx ripgrep --version 13.0`
    ///
    /// Expected: Uses version 13.0 from CLI, not 14.0 from config.
    #[test]
    fn test_cli_version_flag_overrides_config() {
        let mut config = Config::default();
        config
            .tools
            .insert("ripgrep".to_string(), ToolConfig::Version("14.0".to_string()));

        let args = CliArgs::parse_from_test_args(["--version", "13.0", "ripgrep"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: Some(ref v) }
            if name == "ripgrep" && v == &VersionReq::parse("13.0").unwrap()
        );
    }

    /// Test that CLI `@version` syntax takes precedence over config tool version.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// ripgrep = "14.0"
    /// ```
    ///
    /// Command: `cgx ripgrep@13.0`
    ///
    /// Expected: Uses version 13.0 from CLI, not 14.0 from config.
    #[test]
    fn test_cli_at_version_overrides_config() {
        let mut config = Config::default();
        config
            .tools
            .insert("ripgrep".to_string(), ToolConfig::Version("14.0".to_string()));

        let args = CliArgs::parse_from_test_args(["ripgrep@13.0"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: Some(ref v) }
            if name == "ripgrep" && v == &VersionReq::parse("13.0").unwrap()
        );
    }

    /// Test that CLI `--registry` flag takes precedence over config git source.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { version = "1.0", git = "https://github.com/owner/repo.git" }
    /// ```
    ///
    /// Command: `cgx my-tool --registry other-registry`
    ///
    /// Expected: Uses registry from CLI, ignoring git source from config.
    /// Version 1.0 from config is preserved.
    #[test]
    fn test_cli_registry_overrides_config_git() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: Some("1.0".to_string()),
                git: Some("https://github.com/owner/repo.git".to_string()),
                registry: None,
                features: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["--registry", "other-registry", "my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Registry {
                source: RegistrySource::Named(ref reg),
                ref name,
                version: Some(ref v)
            } if reg == "other-registry"
                && name == "my-tool"
                && v == &VersionReq::parse("1.0").unwrap()
        );
    }

    /// Test that CLI `--git` flag takes precedence over config registry source.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { version = "1.0", registry = "my-registry" }
    /// ```
    ///
    /// Command: `cgx my-tool --git https://example.com/repo.git`
    ///
    /// Expected: Uses git from CLI, ignoring registry from config.
    /// Version 1.0 from config is preserved.
    #[test]
    fn test_cli_git_overrides_config_registry() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: Some("1.0".to_string()),
                registry: Some("my-registry".to_string()),
                git: None,
                features: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["--git", "https://example.com/repo.git", "my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Git {
                ref repo,
                selector: GitSelector::DefaultBranch,
                name: Some(ref n),
                version: Some(ref v)
            } if repo == "https://example.com/repo.git"
                && n == "my-tool"
                && v == &VersionReq::parse("1.0").unwrap()
        );
    }

    /// Test that alias resolution happens first, then tool config is applied.
    ///
    /// Simulated config:
    /// ```toml
    /// [aliases]
    /// rg = "ripgrep"
    ///
    /// [tools]
    /// ripgrep = "14.0"
    /// ```
    ///
    /// Command: `cgx rg`
    ///
    /// Expected: Alias `rg` resolves to `ripgrep`, then tool config for `ripgrep` applies,
    /// resulting in version 14.0 from crates.io.
    #[test]
    fn test_alias_with_tool_config() {
        let mut config = Config::default();
        config.aliases.insert("rg".to_string(), "ripgrep".to_string());
        config
            .tools
            .insert("ripgrep".to_string(), ToolConfig::Version("14.0".to_string()));

        let args = CliArgs::parse_from_test_args(["rg"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: Some(ref v) }
            if name == "ripgrep" && v == &VersionReq::parse("14.0").unwrap()
        );
    }

    /// Test that a tool with only version uses the [`Config::default_registry`] if one is
    /// configured.
    ///
    /// Simulated config:
    /// ```toml
    /// default_registry = "my-default-registry"
    ///
    /// [tools]
    /// my-tool = "1.0"
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Since no explicit source is specified in the tool config, uses the
    /// [`Config::default_registry`] instead of crates.io.
    #[test]
    fn test_default_registry_with_simple_tool() {
        let config = Config {
            default_registry: Some("my-default-registry".to_string()),
            tools: [("my-tool".to_string(), ToolConfig::Version("1.0".to_string()))]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Registry {
                source: RegistrySource::Named(ref reg),
                ref name,
                version: Some(ref v)
            } if reg == "my-default-registry"
                && name == "my-tool"
                && v == &VersionReq::parse("1.0").unwrap()
        );
    }

    /// Test that tool-specific registry takes precedence over [`Config::default_registry`].
    ///
    /// Simulated config:
    /// ```toml
    /// default_registry = "default-registry"
    ///
    /// [tools]
    /// my-tool = { version = "1.0", registry = "tool-registry" }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Uses `tool-registry` from tool config, not `default-registry`.
    #[test]
    fn test_tool_registry_overrides_default_registry() {
        let config = Config {
            default_registry: Some("default-registry".to_string()),
            tools: [(
                "my-tool".to_string(),
                ToolConfig::Detailed {
                    version: Some("1.0".to_string()),
                    registry: Some("tool-registry".to_string()),
                    features: None,
                    git: None,
                    branch: None,
                    tag: None,
                    rev: None,
                    path: None,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::Registry {
                source: RegistrySource::Named(ref reg),
                ref name,
                version: Some(ref v)
            } if reg == "tool-registry"
                && name == "my-tool"
                && v == &VersionReq::parse("1.0").unwrap()
        );
    }

    #[test]
    fn test_repo_url_github_default() {
        let forge = Forge::GitHub {
            custom_url: None,
            owner: "octocat".to_string(),
            repo: "hello".to_string(),
        };
        assert_eq!(forge.repo_url(), "https://github.com/octocat/hello");
    }

    #[test]
    fn test_repo_url_gitlab_default() {
        let forge = Forge::GitLab {
            custom_url: None,
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        assert_eq!(forge.repo_url(), "https://gitlab.com/acme/widgets");
    }

    #[test]
    fn test_repo_url_github_custom_url() {
        let forge = Forge::GitHub {
            custom_url: Some(Url::parse("https://github.example.com/").unwrap()),
            owner: "octocat".to_string(),
            repo: "hello".to_string(),
        };
        assert_eq!(forge.repo_url(), "https://github.example.com/octocat/hello");
    }

    #[test]
    fn test_repo_url_gitlab_custom_url() {
        let forge = Forge::GitLab {
            custom_url: Some(Url::parse("https://gitlab.example.com/").unwrap()),
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        assert_eq!(forge.repo_url(), "https://gitlab.example.com/acme/widgets");
    }

    #[test]
    fn test_git_url_is_repo_url_plus_dot_git() {
        let forge = Forge::GitHub {
            custom_url: None,
            owner: "octocat".to_string(),
            repo: "hello".to_string(),
        };
        assert_eq!(forge.git_url(), format!("{}.git", forge.repo_url()));
    }

    /// Test that features-only config doesn't change the [`CrateSpec`] variant.
    ///
    /// Simulated config:
    /// ```toml
    /// [tools]
    /// my-tool = { features = ["feat1", "feat2"] }
    /// ```
    ///
    /// Command: `cgx my-tool`
    ///
    /// Expected: Produces [`CrateSpec::CratesIo`] (the default).
    /// Features affect [`crate::cli::BuildOptions`], not [`CrateSpec`].
    #[test]
    fn test_tool_with_only_features() {
        let mut config = Config::default();
        config.tools.insert(
            "my-tool".to_string(),
            ToolConfig::Detailed {
                version: None,
                features: Some(vec!["feat1".to_string(), "feat2".to_string()]),
                registry: None,
                git: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );

        let args = CliArgs::parse_from_test_args(["my-tool"]);
        let spec = CrateSpec::load(&config, &args).unwrap();

        assert_matches!(
            spec,
            CrateSpec::CratesIo { ref name, version: None }
            if name == "my-tool"
        );
    }
}
