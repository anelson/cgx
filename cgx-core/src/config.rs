use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

use etcetera::{AppStrategy, AppStrategyArgs, choose_app_strategy};
use figment::{
    Figment,
    providers::{Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use strum::{Display, EnumIter, EnumString, IntoStaticStr, VariantNames};

use crate::{
    Result,
    cli::{ConfigArgs, ConfigInputs, HttpArgs},
};

const DEFAULT_RESOLVE_CACHE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HTTP_RETRIES: usize = 2;
const DEFAULT_HTTP_BACKOFF_BASE: Duration = Duration::from_millis(500);
const DEFAULT_HTTP_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The user's preference for using pre-built binaries.
#[derive(
    Default, Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, EnumString, Display, VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum UsePrebuiltBinaries {
    /// Use pre-built binaries when possible (subject to the configured allowed binary providers),
    /// fall back to building from source when no suitable binary is found.
    #[default]
    Auto,
    /// Only ever use pre-built binaries.  If a particular crate invocation cannot be satisfied
    /// with a pre-built binary then fail the invocation rather than building from source
    Always,
    /// Never look for or use pre-built binaries, always build from source.
    Never,
}

/// Represents the sources to check for pre-built binaries before building from source.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    EnumString,
    Display,
    IntoStaticStr,
    EnumIter,
    VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum BinaryProvider {
    /// Use the crate's declared `[package.metadata.binstall]` metadata (if present) to find
    /// pre-built binaries
    Binstall,
    /// Check GitHub releases on the crate's repository
    GithubReleases,
    /// Check GitLab releases on the crate's repository
    GitlabReleases,
    /// Use the community-driven quickinstall repository
    Quickinstall,
}

/// Configuration for how (and whether) to look for pre-built binaries when running a crate.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrebuiltBinariesConfig {
    /// Whether and how to use pre-built binaries.
    pub use_prebuilt_binaries: UsePrebuiltBinaries,

    /// List of sources to check for pre-built binaries before building from source.
    ///
    /// If this list is empty and [`Self::use_prebuilt_binaries`] is not set to `Never`, config
    /// loading will fail with an error. To disable prebuilt binaries, set
    /// [`Self::use_prebuilt_binaries`] to `Never` rather than using an empty provider list.
    pub binary_providers: Vec<BinaryProvider>,

    /// If enabled, when downloading a binary check for a checksum file and if found verify that
    /// the download matches the checksum.
    ///
    /// This adds minimal overhead and is recommended for security, therefore is on by default.
    pub verify_checksums: bool,

    /// If enabled, when dowloading a binary check for a signature file and if found verify that
    /// the download matches the signature.
    ///
    /// This is not quite as simple as [`Self::verify_checksums`] since it requires having the
    /// minisign tooling  available to perform verification.  However it adds stronger security
    /// against malicious binaries.
    pub verify_signatures: bool,
}

impl Default for PrebuiltBinariesConfig {
    fn default() -> Self {
        Self {
            use_prebuilt_binaries: UsePrebuiltBinaries::Auto,
            binary_providers: vec![
                BinaryProvider::Binstall,
                BinaryProvider::GithubReleases,
                BinaryProvider::GitlabReleases,
                BinaryProvider::Quickinstall,
            ],
            verify_checksums: true,
            verify_signatures: true,
        }
    }
}

/// HTTP client settings for registry queries, binary downloads, API calls, and git operations.
///
/// For git operations, proxy, user agent, and connect timeout are applied via gix config
/// overrides (backed by the curl HTTP backend). Retry and backoff settings are applied by
/// cgx's own retry wrapper around git fetches. The timeout setting is intentionally used for
/// both connection timeout and stalled-transfer timeout detection for git-over-HTTP.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// Request timeout for HTTP operations.
    ///
    /// For git operations over HTTP/S this value is also used as both:
    /// - connection timeout
    /// - stalled-transfer timeout threshold
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,

    /// Maximum number of retries for transient HTTP failures (429, 5xx, connection errors).
    pub retries: usize,

    /// Base delay for exponential backoff between retries.
    #[serde(with = "humantime_serde")]
    pub backoff_base: Duration,

    /// Maximum delay between retries (caps exponential growth).
    #[serde(with = "humantime_serde")]
    pub backoff_max: Duration,

    /// HTTP or SOCKS5 proxy URL for all HTTP requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_HTTP_TIMEOUT,
            retries: DEFAULT_HTTP_RETRIES,
            backoff_base: DEFAULT_HTTP_BACKOFF_BASE,
            backoff_max: DEFAULT_HTTP_BACKOFF_MAX,
            proxy: None,
        }
    }
}

/// Raw HTTP config from config file, with optional fields for detecting whether values were set.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfigFile {
    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<usize>,

    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_base: Option<Duration>,

    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_max: Option<Duration>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Configuration for a specific tool, matching Cargo.toml dependency format.
///
/// This can be a simple version string like `"1.0"` or a more complex specification
/// with version, features, registry, git repo, etc.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum ToolConfig {
    /// Simple version specification (e.g., "1.0", "*")
    Version(String),
    /// Detailed configuration with version, features, registry, etc.
    Detailed {
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        features: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        registry: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        git: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rev: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
}

impl ToolConfig {
    /// Return configured features for a [`ToolConfig::Detailed`] tool.
    pub fn features(&self) -> Option<&[String]> {
        match self {
            ToolConfig::Version(_) => None,
            ToolConfig::Detailed { features, .. } => features.as_deref(),
        }
    }
}

/// Intermediate structure for deserializing config files from TOML.
///
/// This matches the structure of cgx.toml files and is used during the deserialization
/// process. Fields are then mapped to the final [`Config`] struct.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_optional_expanded_path")]
    pub bin_dir: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_optional_expanded_path")]
    pub build_dir: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_optional_expanded_path")]
    pub cache_dir: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "humantime_serde")]
    pub resolve_cache_timeout: Option<Duration>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_registry: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub prebuilt_binaries: Option<PrebuiltBinariesConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConfigFile>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<HashMap<String, ToolConfig>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<HashMap<String, String>>,
}

impl ConfigFile {
    /// Returns the base configuration with sensible defaults.
    ///
    /// This is distinct from [`Default`] which returns all `None` values. The `Default` impl
    /// is used by serde to represent fields missing from a config file, so it must be all `None`.
    ///
    /// This method provides the actual default values that serve as the lowest-precedence layer
    /// in the config hierarchy, before any config files are applied.
    pub fn base_config() -> Self {
        Self {
            bin_dir: None,
            build_dir: None,
            cache_dir: None,
            locked: Some(true),
            log_level: None,
            offline: Some(false),
            resolve_cache_timeout: Some(DEFAULT_RESOLVE_CACHE_TIMEOUT),
            toolchain: None,
            default_registry: None,
            prebuilt_binaries: Some(PrebuiltBinariesConfig::default()),
            http: None,
            tools: None,
            aliases: None,
        }
    }
}

/// Custom deserializer for optional [`PathBuf`] that expands ~ to home directory.
fn deserialize_optional_expanded_path<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt_string: Option<String> = Option::deserialize(deserializer)?;
    match opt_string {
        None => Ok(None),
        Some(s) => {
            let expanded = shellexpand::tilde(&s);
            Ok(Some(PathBuf::from(expanded.as_ref())))
        }
    }
}

/// Configuration settings for cgx.
///
/// Configuration is loaded from multiple sources in order of precedence (later sources override
/// earlier ones):
/// 1. Hard-coded defaults
/// 2. System-wide config file (`/etc/cgx.toml` on Linux/macOS)
/// 3. User config file (`$XDG_CONFIG_HOME/cgx/cgx.toml` or platform equivalent)
/// 4. Directory hierarchy from filesystem root to current directory (each `cgx.toml` found)
/// 5. Command-line arguments (highest priority)
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory where config files are stored
    #[allow(dead_code)]
    pub config_dir: PathBuf,

    /// The cache directory where various levels of cache are located
    pub cache_dir: PathBuf,

    /// Directory where compiled binaries that can be re-used are stored
    pub bin_dir: PathBuf,

    /// Directory for ephemeral build artifacts.
    ///
    /// Temporary directories for source extraction and compilation are created here.
    /// Only the final compiled binary is retained; all other build artifacts are cleaned up.
    pub build_dir: PathBuf,

    /// How long to keep resolved crate information in the cache before re-resolving
    pub resolve_cache_timeout: Duration,

    pub offline: bool,

    pub locked: bool,

    pub refresh: bool,

    /// Rust toolchain to use for building (e.g., "nightly", "1.70.0", "stable")
    pub toolchain: Option<String>,

    /// Logging verbosity level (e.g., "info", "debug", "trace")
    pub log_level: Option<String>,

    /// Default registry to use instead of crates.io when no registry is explicitly specified
    pub default_registry: Option<String>,

    /// How or whether to look for pre-built binaries published for the crates being run.
    pub prebuilt_binaries: PrebuiltBinariesConfig,

    /// HTTP client configuration for registry queries, binary downloads, and API calls.
    pub http: HttpConfig,

    /// Pinned tool versions and configurations.
    ///
    /// Tools listed here will use the specified version/source instead of being resolved
    /// dynamically. This allows pinning critical tools to specific versions.
    pub tools: HashMap<String, ToolConfig>,

    /// Tool name aliases.
    ///
    /// Maps convenient names to actual crate names. For example, `rg` -> `ripgrep`.
    /// Note that aliases shadow actual crate names, so aliased crates become inaccessible.
    pub aliases: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_dir: PathBuf::default(),
            cache_dir: PathBuf::default(),
            bin_dir: PathBuf::default(),
            build_dir: PathBuf::default(),
            resolve_cache_timeout: Duration::from_secs(3600),
            offline: false,
            locked: true,
            refresh: false,
            toolchain: None,
            log_level: None,
            default_registry: None,
            prebuilt_binaries: PrebuiltBinariesConfig::default(),
            http: HttpConfig::default(),
            tools: HashMap::default(),
            aliases: HashMap::default(),
        }
    }
}

impl Config {
    /// Load the configuration, honoring config files and command line arguments.
    ///
    /// Configuration is loaded from multiple sources with the following precedence
    /// (later sources override earlier ones):
    /// 1. Hard-coded defaults
    /// 2. System-wide config file
    /// 3. User config file
    /// 4. Directory hierarchy config files (from root to current directory)
    /// 5. Command-line arguments (highest priority)
    pub fn load(inputs: &ConfigInputs) -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Self::load_from_dir(&cwd, inputs)
    }

    /// Render the merged configured tools and aliases as deterministic TOML.
    pub fn tools_toml(&self) -> Result<String> {
        #[derive(Serialize)]
        struct ToolsToml {
            tools: BTreeMap<String, ToolConfig>,
            aliases: BTreeMap<String, String>,
        }

        let tools = self
            .tools
            .iter()
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect();
        let aliases = self
            .aliases
            .iter()
            .map(|(name, target)| (name.clone(), target.clone()))
            .collect();

        let mut rendered = toml::to_string_pretty(&ToolsToml { tools, aliases })
            .context(crate::error::TomlSerializeSnafu)?;

        if !rendered.lines().any(|line| line.trim() == "[tools]") {
            rendered = format!("[tools]\n\n{}", rendered);
        }

        if !rendered.lines().any(|line| line.trim() == "[aliases]") {
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered.push_str("\n[aliases]\n");
        }

        Ok(rendered)
    }

    /// Load config from the CLI args and a specified directory which may or may not contain config
    /// files.
    pub fn load_from_dir(cwd: &Path, inputs: &ConfigInputs) -> Result<Self> {
        let ConfigInputs {
            config: config_args,
            http,
            prebuilt,
            cargo,
            toolchain,
        } = inputs;
        let strategy = Self::get_user_dirs()?;

        // Start with base config defaults, then merge config files
        let mut figment = Figment::new().merge(Serialized::defaults(ConfigFile::base_config()));

        for config_file in Self::discover_config_files(cwd, config_args)? {
            figment = figment.merge(Toml::file(config_file));
        }

        // Extract merged config file values (no CLI overrides applied yet via Figment)
        let config_file: ConfigFile = figment.extract().context(crate::error::ConfigExtractSnafu)?;

        // Override the config file values using any CLI args that were specified

        // locked: --unlocked > --locked/--frozen > config > default(true)
        let locked = if cargo.unlocked {
            false
        } else if cargo.locked || cargo.frozen {
            true
        } else {
            config_file.locked.unwrap_or(true)
        };

        // offline: --offline/--frozen > config > default(false)
        let offline = if cargo.offline || cargo.frozen {
            true
        } else {
            config_file.offline.unwrap_or(false)
        };

        // toolchain: CLI > config
        let toolchain = toolchain.clone().or(config_file.toolchain);

        // Determine config_dir based on override precedence
        let config_dir = if let Some(user_config_dir) = &config_args.user_config_dir {
            user_config_dir.clone()
        } else if let Some(app_dir) = &config_args.app_dir {
            app_dir.join("config")
        } else {
            strategy.config_dir()
        };

        // Determine cache_dir: CLI (app-dir) > config file > strategy
        let cache_dir = if let Some(app_dir) = &config_args.app_dir {
            app_dir.join("cache")
        } else {
            config_file.cache_dir.unwrap_or_else(|| strategy.cache_dir())
        };

        // Determine bin_dir: CLI (app-dir) > config file > strategy
        let bin_dir = if let Some(app_dir) = &config_args.app_dir {
            app_dir.join("bins")
        } else {
            config_file
                .bin_dir
                .unwrap_or_else(|| strategy.in_data_dir("bins"))
        };

        // Determine build_dir: CLI (app-dir) > config file > strategy
        let build_dir = if let Some(app_dir) = &config_args.app_dir {
            app_dir.join("build")
        } else {
            config_file
                .build_dir
                .unwrap_or_else(|| strategy.in_data_dir("build"))
        };

        let mut prebuilt_binaries = config_file.prebuilt_binaries.unwrap_or_default();

        // Apply CLI overrides for prebuilt binaries
        if let Some(mode) = prebuilt.prebuilt_binary {
            prebuilt_binaries.use_prebuilt_binaries = mode;
        }
        if let Some(ref providers) = prebuilt.prebuilt_binary_sources {
            prebuilt_binaries.binary_providers = providers.clone();
        }
        if prebuilt.prebuilt_binary_no_verify_checksums {
            prebuilt_binaries.verify_checksums = false;
        }
        if prebuilt.prebuilt_binary_no_verify_signatures {
            prebuilt_binaries.verify_signatures = false;
        }

        // Validate prebuilt binaries configuration
        if prebuilt_binaries.binary_providers.is_empty()
            && prebuilt_binaries.use_prebuilt_binaries != UsePrebuiltBinaries::Never
        {
            return crate::error::NoProvidersConfiguredSnafu.fail();
        }

        // Build HTTP config with precedence: CLI > config file > Cargo env vars > defaults
        let http_config_file = config_file.http.unwrap_or_default();
        let http = Self::build_http_config(&http_config_file, http)?;

        Ok(Self {
            config_dir,
            cache_dir,
            bin_dir,
            build_dir,
            resolve_cache_timeout: config_file
                .resolve_cache_timeout
                .unwrap_or(DEFAULT_RESOLVE_CACHE_TIMEOUT),
            offline,
            locked,
            refresh: cargo.refresh,
            toolchain,
            log_level: config_file.log_level,
            default_registry: config_file.default_registry,
            prebuilt_binaries,
            http,
            tools: config_file.tools.unwrap_or_default(),
            aliases: config_file.aliases.unwrap_or_default(),
        })
    }

    /// Discover all config file locations in order of precedence.
    ///
    /// Returns paths from lowest to highest precedence. Later config files override earlier ones.
    ///
    /// The search order is:
    /// 1. System config: `/etc/cgx.toml` on Unix, Windows equivalent (or override location)
    /// 2. User config: `$XDG_CONFIG_HOME/cgx/cgx.toml` or platform equivalent (or override
    ///    location)
    /// 3. Directory hierarchy: All `cgx.toml` files from filesystem root to current directory
    fn discover_config_files(cwd: &Path, config_args: &ConfigArgs) -> Result<Vec<PathBuf>> {
        let mut config_files = Vec::new();

        // If the user explicitly specified a config file, read ONLY that file
        if let Some(config_path) = &config_args.config_file {
            return Ok(vec![config_path.clone()]);
        }

        // System config (can be overridden)
        if let Some(system_config_dir) = &config_args.system_config_dir {
            let system_config = system_config_dir.join("cgx.toml");
            if system_config.exists() {
                config_files.push(system_config);
            }
        } else {
            #[cfg(unix)]
            {
                let system_config = PathBuf::from("/etc/cgx.toml");
                if system_config.exists() {
                    config_files.push(system_config);
                }
            }

            #[cfg(windows)]
            {
                if let Some(program_data) = std::env::var_os("ProgramData") {
                    let system_config = PathBuf::from(program_data).join("cgx").join("cgx.toml");
                    if system_config.exists() {
                        config_files.push(system_config);
                    }
                }
            }
        }

        // User config (can be overridden via user-config-dir or app-dir)
        let user_config = if let Some(user_config_dir) = &config_args.user_config_dir {
            // Most specific: explicit user config directory
            user_config_dir.join("cgx.toml")
        } else if let Some(app_dir) = &config_args.app_dir {
            // App dir provides a base for config
            app_dir.join("config").join("cgx.toml")
        } else {
            // Default: use platform-specific config directory
            let strategy = Self::get_user_dirs()?;
            strategy.config_dir().join("cgx.toml")
        };

        if user_config.exists() {
            config_files.push(user_config);
        }

        let mut ancestors: Vec<PathBuf> = cwd.ancestors().map(|p| p.to_path_buf()).collect();
        ancestors.reverse();

        for ancestor in ancestors {
            let config_file = ancestor.join("cgx.toml");
            if config_file.exists() {
                config_files.push(config_file);
            }
        }

        Ok(config_files)
    }

    fn get_user_dirs() -> Result<impl AppStrategy> {
        choose_app_strategy(AppStrategyArgs {
            top_level_domain: "org".to_string(),
            author: "anelson".to_string(),
            app_name: "cgx".to_string(),
        })
        .context(crate::error::EtceteraSnafu)
    }

    /// Build [`HttpConfig`] with proper precedence:
    /// 1. CLI args (highest priority)
    /// 2. Config file values
    /// 3. Cargo environment variable fallbacks
    /// 4. Defaults (lowest priority)
    fn build_http_config(config_file: &HttpConfigFile, http: &HttpArgs) -> Result<HttpConfig> {
        // Determine if CLI args were provided (they override everything)
        let cli_timeout = http.http_timeout.as_ref();
        let cli_retries = http.http_retries;
        let cli_proxy = http.http_proxy.as_ref();

        // timeout: CLI > config > CARGO_HTTP_TIMEOUT > default
        let timeout = if let Some(timeout_str) = cli_timeout {
            humantime::parse_duration(timeout_str).context(crate::error::InvalidHttpTimeoutSnafu {
                value: timeout_str.clone(),
            })?
        } else if let Some(config_timeout) = config_file.timeout {
            config_timeout
        } else if let Ok(cargo_timeout) = std::env::var("CARGO_HTTP_TIMEOUT") {
            if let Ok(secs) = cargo_timeout.parse::<u64>() {
                Duration::from_secs(secs)
            } else {
                tracing::warn!(
                    "Invalid CARGO_HTTP_TIMEOUT value '{}', falling back to default {:?}.",
                    cargo_timeout,
                    DEFAULT_HTTP_TIMEOUT
                );
                DEFAULT_HTTP_TIMEOUT
            }
        } else {
            DEFAULT_HTTP_TIMEOUT
        };

        // retries: CLI > config > CARGO_NET_RETRY > default
        let retries = if let Some(cli_retries) = cli_retries {
            cli_retries
        } else if let Some(config_retries) = config_file.retries {
            config_retries
        } else if let Ok(cargo_retry) = std::env::var("CARGO_NET_RETRY") {
            if let Ok(retries) = cargo_retry.parse::<usize>() {
                retries
            } else {
                tracing::warn!(
                    "Invalid CARGO_NET_RETRY value '{}', falling back to default {}.",
                    cargo_retry,
                    DEFAULT_HTTP_RETRIES
                );
                DEFAULT_HTTP_RETRIES
            }
        } else {
            DEFAULT_HTTP_RETRIES
        };

        // proxy: CLI > config > CARGO_HTTP_PROXY > None (let reqwest handle system proxies)
        let proxy = if let Some(p) = cli_proxy {
            Some(p.clone())
        } else if config_file.proxy.is_some() {
            config_file.proxy.clone()
        } else if let Ok(cargo_proxy) = std::env::var("CARGO_HTTP_PROXY") {
            Some(cargo_proxy)
        } else {
            None
        };

        // backoff settings: config > defaults (no CLI or Cargo env fallback)
        let backoff_base = config_file.backoff_base.unwrap_or(DEFAULT_HTTP_BACKOFF_BASE);
        let backoff_max = config_file.backoff_max.unwrap_or(DEFAULT_HTTP_BACKOFF_MAX);

        Ok(HttpConfig {
            timeout,
            retries,
            backoff_base,
            backoff_max,
            proxy,
        })
    }
}

/// Create a fake, isolated config environment for testing, with all of the path config
/// settings pointing to a [`tempfile::TempDir`] directory.
#[cfg(test)]
pub(crate) fn create_test_env() -> (tempfile::TempDir, Config) {
    let temp_dir = tempfile::tempdir().unwrap();
    let config = Config {
        config_dir: temp_dir.path().join("config"),
        cache_dir: temp_dir.path().join("cache"),
        bin_dir: temp_dir.path().join("bins"),
        build_dir: temp_dir.path().join("build"),
        resolve_cache_timeout: Duration::from_secs(3600),
        locked: true,
        ..Default::default()
    };

    (temp_dir, config)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use assert_matches::assert_matches;

    use super::*;
    use crate::cli::Cli;

    /// Apply test-local config directory overrides so config loading cannot read
    /// host-level `/etc/cgx.toml` or user-level cgx config on the machine running tests.
    ///
    /// This keeps tests deterministic on developer systems that actively use cgx.
    fn with_isolated_global_config(mut inputs: ConfigInputs, root: &Path) -> ConfigInputs {
        inputs.config.system_config_dir = Some(root.join("system"));
        inputs.config.user_config_dir = Some(root.join("user"));
        inputs
    }

    #[test]
    fn test_deserialize_basic_config() {
        let toml_content = r#"
            bin_dir = "/usr/local/bin"
            cache_dir = "/tmp/cache"
            offline = true
            locked = false
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(config.bin_dir, Some(PathBuf::from("/usr/local/bin")));
        assert_eq!(config.cache_dir, Some(PathBuf::from("/tmp/cache")));
        assert_eq!(config.offline, Some(true));
        assert_eq!(config.locked, Some(false));
    }

    #[test]
    fn test_deserialize_duration() {
        let toml_content = r#"
            resolve_cache_timeout = "2h"
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(
            config.resolve_cache_timeout,
            Some(Duration::from_secs(2 * 60 * 60))
        );
    }

    #[test]
    fn test_deserialize_tilde_expansion() {
        let toml_content = r#"
            bin_dir = "~/.local/bin"
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap();
        let expected = PathBuf::from(home).join(".local/bin");
        assert_eq!(config.bin_dir, Some(expected));
    }

    #[test]
    fn test_deserialize_binary_providers() {
        let toml_content = r#"
            [prebuilt_binaries]
            binary_providers = ["github-releases", "quickinstall"]
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(
            config.prebuilt_binaries.unwrap().binary_providers,
            vec![BinaryProvider::GithubReleases, BinaryProvider::Quickinstall,]
        );
    }

    #[test]
    fn test_deserialize_tools_simple() {
        let toml_content = r#"
            [tools]
            ripgrep = "14.0"
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        let tools = config.tools.unwrap();
        assert_eq!(
            tools.get("ripgrep"),
            Some(&ToolConfig::Version("14.0".to_string()))
        );
    }

    #[test]
    fn test_deserialize_tools_detailed() {
        let toml_content = r#"
            [tools]
            taplo-cli = { version = "1.11.0", features = ["schema"] }
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        let tools = config.tools.unwrap();

        match tools.get("taplo-cli") {
            Some(ToolConfig::Detailed {
                version, features, ..
            }) => {
                assert_eq!(*version, Some("1.11.0".to_string()));
                assert_eq!(*features, Some(vec!["schema".to_string()]));
            }
            _ => panic!("Expected Detailed tool config"),
        }
    }

    #[test]
    fn test_deserialize_aliases() {
        let toml_content = r#"
            [aliases]
            rg = "ripgrep"
            taplo = "taplo-cli"
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        let aliases = config.aliases.unwrap();
        assert_eq!(aliases.get("rg"), Some(&"ripgrep".to_string()));
        assert_eq!(aliases.get("taplo"), Some(&"taplo-cli".to_string()));
    }

    #[test]
    fn test_tools_toml_is_sorted_and_valid() {
        let mut config = Config::default();
        config
            .tools
            .insert("zeta".to_string(), ToolConfig::Version("2".to_string()));
        config
            .tools
            .insert("alpha".to_string(), ToolConfig::Version("1".to_string()));
        config.tools.insert(
            "beta".to_string(),
            ToolConfig::Detailed {
                version: Some("1.5".to_string()),
                features: Some(vec!["frobnulator".to_string()]),
                registry: None,
                git: None,
                branch: None,
                tag: None,
                rev: None,
                path: None,
            },
        );
        config.aliases.insert("zz".to_string(), "zeta".to_string());
        config.aliases.insert("aa".to_string(), "alpha".to_string());

        let rendered = config.tools_toml().unwrap();
        let parsed: ConfigFile = toml::from_str(&rendered).unwrap();

        let tools = parsed.tools.unwrap();
        assert_eq!(tools.get("alpha"), Some(&ToolConfig::Version("1".to_string())));
        assert_eq!(tools.get("zeta"), Some(&ToolConfig::Version("2".to_string())));
        assert_matches!(
            tools.get("beta"),
            Some(ToolConfig::Detailed {
                version: Some(version),
                features: Some(features),
                ..
            }) if version == "1.5" && features == &vec!["frobnulator".to_string()]
        );

        let aliases = parsed.aliases.unwrap();
        assert_eq!(aliases.get("aa"), Some(&"alpha".to_string()));
        assert_eq!(aliases.get("zz"), Some(&"zeta".to_string()));

        assert!(rendered.find("alpha").unwrap() < rendered.find("zeta").unwrap());
        assert!(rendered.find("aa").unwrap() < rendered.find("zz").unwrap());
    }

    #[test]
    fn test_config_defaults() {
        let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
        let config = Config::load(&args).unwrap();

        assert!(!config.offline);
        assert!(config.locked); // Default is true per issue #55
        assert_eq!(config.toolchain, None);
        assert_eq!(config.resolve_cache_timeout, Duration::from_secs(60 * 60));
    }

    #[test]
    fn test_cli_overrides() {
        let args =
            Cli::parse_from_test_args(["+nightly", "--offline", "--locked", "test-crate"]).config_inputs();
        let config = Config::load(&args).unwrap();

        assert!(config.offline);
        assert!(config.locked);
        assert_eq!(config.toolchain, Some("nightly".to_string()));
    }

    #[test]
    fn test_frozen_implies_locked_and_offline() {
        let args = Cli::parse_from_test_args(["--frozen", "test-crate"]).config_inputs();
        let config = Config::load(&args).unwrap();

        assert!(config.offline);
        assert!(config.locked);
    }

    #[test]
    fn test_full_config_example() {
        let toml_content = r#"
            bin_dir = "~/.local/bin"
            build_dir = "~/.local/build"
            cache_dir = "~/.cache/cgx"
            locked = true
            log_level = "info"
            offline = false
            resolve_cache_timeout = "1h"
            toolchain = "stable"
            default_registry = "my-registry"

            [prebuilt_binaries]
            binary_providers = ["github-releases", "gitlab-releases", "quickinstall"]

            [tools]
            ripgrep = "*"
            taplo-cli = { version = "1.11.0", features = ["schema"] }

            [aliases]
            rg = "ripgrep"
            taplo = "taplo-cli"
        "#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();

        assert_eq!(config.log_level, Some("info".to_string()));
        assert_eq!(config.toolchain, Some("stable".to_string()));
        assert_eq!(config.default_registry, Some("my-registry".to_string()));
        assert_eq!(config.locked, Some(true));
        assert_eq!(config.offline, Some(false));
        assert_eq!(config.resolve_cache_timeout, Some(Duration::from_secs(60 * 60)));

        let prebuilt_binaries = config.prebuilt_binaries.unwrap();

        assert_eq!(prebuilt_binaries.binary_providers.len(), 3);

        // Other prebuild binary settings should be defaults
        assert_eq!(prebuilt_binaries.use_prebuilt_binaries, UsePrebuiltBinaries::Auto);
        assert!(prebuilt_binaries.verify_checksums);
        assert!(prebuilt_binaries.verify_signatures);

        let tools = config.tools.unwrap();
        assert_eq!(tools.len(), 2);

        let aliases = config.aliases.unwrap();
        assert_eq!(aliases.len(), 2);
    }

    mod prebuilt_validation_tests {
        use std::io::Write;

        use assert_matches::assert_matches;

        use super::*;

        fn create_temp_config(toml_content: &str) -> tempfile::TempDir {
            let temp_dir = tempfile::tempdir().unwrap();
            let config_path = temp_dir.path().join("cgx.toml");
            let mut file = std::fs::File::create(&config_path).unwrap();
            file.write_all(toml_content.as_bytes()).unwrap();
            temp_dir
        }

        #[test]
        fn test_empty_providers_with_auto_fails() {
            let toml_content = r#"
                [prebuilt_binaries]
                use_prebuilt_binaries = "auto"
                binary_providers = []
            "#;

            let temp_dir = create_temp_config(toml_content);
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let result = Config::load_from_dir(temp_dir.path(), &args);
            assert_matches!(result, Err(crate::error::Error::NoProvidersConfigured));
        }

        #[test]
        fn test_empty_providers_with_always_fails() {
            let toml_content = r#"
                [prebuilt_binaries]
                use_prebuilt_binaries = "always"
                binary_providers = []
            "#;

            let temp_dir = create_temp_config(toml_content);
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let result = Config::load_from_dir(temp_dir.path(), &args);
            assert_matches!(result, Err(crate::error::Error::NoProvidersConfigured));
        }

        #[test]
        fn test_empty_providers_with_never_ok() {
            let toml_content = r#"
                [prebuilt_binaries]
                use_prebuilt_binaries = "never"
                binary_providers = []
            "#;

            let temp_dir = create_temp_config(toml_content);
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let result = Config::load_from_dir(temp_dir.path(), &args);
            assert!(result.is_ok(), "Empty providers with 'never' mode should succeed");
        }
    }

    /// Test the config loading logic that traverses up a directory hierarchy looking for config
    /// files.
    ///
    /// `testdata/configs` contains test config files constructed specificially to facilitate these
    /// tests
    mod hierarchy_tests {
        use assert_matches::assert_matches;

        use super::*;

        /// Test loading config from a 3-level hierarchy (root → work → project1).
        ///
        /// Verifies that config files are merged in order of precedence, with closer files
        /// overriding values from parent directories. The `resolve_cache_timeout` should be 3m
        /// (from project1), tools should include entries from all 3 levels (5 total), and aliases
        /// should show the `dummytool` override from project1.
        #[test]
        fn test_config_hierarchy_project1() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_project1();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(3 * 60));

            assert!(config.tools.contains_key("ripgrep"));
            assert!(config.tools.contains_key("root_tool"));
            assert!(config.tools.contains_key("taplo-cli"));
            assert!(config.tools.contains_key("work_tool"));
            assert!(config.tools.contains_key("project1_tool"));
            assert_eq!(config.tools.len(), 5);

            assert_eq!(config.aliases.get("dummytool"), Some(&"project1".to_string()));
            assert_eq!(config.aliases.get("rg"), Some(&"ripgrep".to_string()));
            assert_eq!(config.aliases.get("taplo"), Some(&"taplo-cli".to_string()));
            assert_eq!(config.aliases.len(), 3);
        }

        /// Test loading config from a parallel 3-level hierarchy (root → work → project2).
        ///
        /// Similar to project1, but verifies that sibling project directories maintain
        /// independent configurations. The `resolve_cache_timeout` should be 5m (from project2),
        /// tools should include `project2_tool` instead of `project1_tool` (5 total), and the
        /// `dummytool` alias should override to "project2".
        #[test]
        fn test_config_hierarchy_project2() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_project2();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(5 * 60));

            assert!(config.tools.contains_key("ripgrep"));
            assert!(config.tools.contains_key("root_tool"));
            assert!(config.tools.contains_key("taplo-cli"));
            assert!(config.tools.contains_key("work_tool"));
            assert!(config.tools.contains_key("project2_tool"));
            assert_eq!(config.tools.len(), 5);

            assert_eq!(config.aliases.get("dummytool"), Some(&"project2".to_string()));
            assert_eq!(config.aliases.get("rg"), Some(&"ripgrep".to_string()));
            assert_eq!(config.aliases.get("taplo"), Some(&"taplo-cli".to_string()));
            assert_eq!(config.aliases.len(), 3);
        }

        /// Test loading config from a 2-level hierarchy (root → work).
        ///
        /// Verifies config merging at an intermediate level in the hierarchy. The
        /// `resolve_cache_timeout` should be 2m (from work), tools should include entries from
        /// both root and work (4 total), and the `dummytool` alias should override to "work".
        #[test]
        fn test_config_hierarchy_work() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_work();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(2 * 60));

            assert!(config.tools.contains_key("ripgrep"));
            assert!(config.tools.contains_key("root_tool"));
            assert!(config.tools.contains_key("taplo-cli"));
            assert!(config.tools.contains_key("work_tool"));
            assert_eq!(config.tools.len(), 4);

            assert_eq!(config.aliases.get("dummytool"), Some(&"work".to_string()));
            assert_eq!(config.aliases.get("rg"), Some(&"ripgrep".to_string()));
            assert_eq!(config.aliases.get("taplo"), Some(&"taplo-cli".to_string()));
            assert_eq!(config.aliases.len(), 3);
        }

        /// Test loading config from the root level only.
        ///
        /// Establishes the baseline configuration from the root config file. The
        /// `resolve_cache_timeout` should be 1m (from root), and only root-level tools and aliases
        /// should be present (3 tools, 3 aliases including dummytool="root").
        #[test]
        fn test_config_hierarchy_root() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_root();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(60));

            assert!(config.tools.contains_key("ripgrep"));
            assert!(config.tools.contains_key("root_tool"));
            assert!(config.tools.contains_key("taplo-cli"));
            assert_eq!(config.tools.len(), 3);

            assert_eq!(config.aliases.get("dummytool"), Some(&"root".to_string()));
            assert_eq!(config.aliases.get("rg"), Some(&"ripgrep".to_string()));
            assert_eq!(config.aliases.get("taplo"), Some(&"taplo-cli".to_string()));
            assert_eq!(config.aliases.len(), 3);
        }

        /// Test that specifying `--config-file` bypasses hierarchy traversal.
        ///
        /// When an explicit config file is provided via CLI, ONLY that file is read without
        /// walking up the directory tree. This test uses a non-standard filename to verify
        /// it's the explicit path (not discovery) that loads the config. Should have only 1 tool
        /// and 1 alias from the specified file, with timeout=6m.
        #[test]
        fn test_explicit_config_file() {
            let test_case = crate::testdata::ConfigTestCase::explicit_non_standard_name();

            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(test_case.path().to_path_buf());

            let config = Config::load(&args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(6 * 60));

            assert!(config.tools.contains_key("project1_tool"));
            assert_eq!(config.tools.len(), 1);

            assert_eq!(
                config.aliases.get("dummytool"),
                Some(&"not_called_cgx_project1".to_string())
            );
            assert_eq!(config.aliases.len(), 1);
        }

        /// Test that detailed tool configurations are preserved during hierarchy merging.
        ///
        /// Verifies that tools specified with detailed configs (version, features, etc.) maintain
        /// their structure when merged across the hierarchy. The taplo-cli tool from root should
        /// retain its version="1.11.0" and features=["schema"] specification.
        #[test]
        fn test_tools_detailed_config_preserved() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_root();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            let taplo_tool = config.tools.get("taplo-cli").unwrap();
            assert_matches!(
                taplo_tool,
                ToolConfig::Detailed {
                    version: Some(v),
                    features: Some(f),
                    ..
                } if v == "1.11.0" && f == &vec!["schema".to_string()]
            );
        }

        /// Test that CLI arguments have the highest precedence over config files.
        ///
        /// Command-line flags should override any values set in config files, regardless of
        /// where those config files appear in the hierarchy. This verifies that --offline,
        /// --locked, and +toolchain flags take precedence over the merged config.
        #[test]
        fn test_cli_args_override_config_files() {
            let test_case = crate::testdata::ConfigTestCase::hierarchy_project1();

            let args =
                Cli::parse_from_test_args(["+stable", "--offline", "--locked", "test-crate"]).config_inputs();
            let config = Config::load_from_dir(test_case.path(), &args).unwrap();

            assert!(config.offline);
            assert!(config.locked);
            assert_eq!(config.toolchain, Some("stable".to_string()));
        }

        /// Test that --config-file reads only the specified file.
        ///
        /// When --config-file is specified, only that single config file should be loaded,
        /// bypassing all config discovery (system, user, and hierarchy configs).
        #[test]
        fn test_config_file_reads_only_specified_file() {
            // The hierarchy has configs with resolve_cache_timeout set to various values:
            // root=1m, work=2m, project1=3m
            let hierarchy_dir = crate::testdata::ConfigTestCase::hierarchy_project1();

            // The explicit config has a different timeout (6m)
            let explicit_config = crate::testdata::ConfigTestCase::explicit_non_standard_name();

            // Load config from project1 directory but with --config-file pointing to explicit config
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(explicit_config.path().to_path_buf());

            let config = Config::load_from_dir(hierarchy_dir.path(), &args).unwrap();

            // Should have the explicit config's timeout (6m), not any from the hierarchy (1m/2m/3m)
            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(6 * 60));

            // Should have only the tool from explicit config, not from hierarchy
            assert!(config.tools.contains_key("project1_tool"));
            assert_eq!(config.tools.len(), 1);

            // Should have only the alias from explicit config
            assert_eq!(
                config.aliases.get("dummytool"),
                Some(&"not_called_cgx_project1".to_string())
            );
            assert_eq!(config.aliases.len(), 1);
        }
    }

    mod config_file_discovery_tests {
        use std::fs;

        use super::*;

        /// Test that [`discover_config_files`] returns only the explicit file when --config-file is
        /// set.
        ///
        /// This directly tests the discovery logic to ensure hierarchy configs are not included.
        #[test]
        fn test_discover_only_explicit_file() {
            // RAII guard to ensure user config cleanup happens even if test panics
            struct UserConfigGuard {
                path: PathBuf,
                should_delete: bool,
            }

            impl Drop for UserConfigGuard {
                fn drop(&mut self) {
                    if self.should_delete {
                        fs::remove_file(&self.path).ok();
                    }
                }
            }

            let temp_dir = tempfile::tempdir().unwrap();
            let cwd = temp_dir.path();

            // Create a hierarchy of config files
            let root_config = cwd.join("cgx.toml");
            fs::write(&root_config, "resolve_cache_timeout = \"1m\"").unwrap();

            let sub_dir = cwd.join("subdir");
            fs::create_dir(&sub_dir).unwrap();
            let sub_config = sub_dir.join("cgx.toml");
            fs::write(&sub_config, "resolve_cache_timeout = \"2m\"").unwrap();

            // Create an explicit config elsewhere
            let explicit_config = temp_dir.path().join("explicit.toml");
            fs::write(&explicit_config, "resolve_cache_timeout = \"3m\"").unwrap();

            // Create a user config to trigger the bug (if it doesn't already exist)
            let strategy = Config::get_user_dirs().unwrap();
            let user_config_dir = strategy.config_dir();
            fs::create_dir_all(&user_config_dir).ok();
            let user_config_path = user_config_dir.join("cgx.toml");
            let user_config_existed = user_config_path.exists();

            // Guard ensures cleanup even if test panics
            let _guard = if !user_config_existed {
                fs::write(&user_config_path, "resolve_cache_timeout = \"99m\"").unwrap();
                UserConfigGuard {
                    path: user_config_path,
                    should_delete: true,
                }
            } else {
                UserConfigGuard {
                    path: user_config_path,
                    should_delete: false,
                }
            };

            // Test with --config-file
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(explicit_config.clone());

            let discovered = Config::discover_config_files(&sub_dir, &args.config).unwrap();

            // Should contain ONLY the explicit config file (no system, user, or hierarchy configs)
            // This will FAIL if the bug exists, showing [user_config, explicit_config]
            assert_eq!(
                discovered.len(),
                1,
                "Expected only 1 config file, got {}: {:?}",
                discovered.len(),
                discovered
            );
            assert_eq!(discovered[0], explicit_config);
        }

        /// Test that hierarchy configs are discovered when --config-file is not set.
        #[test]
        fn test_discover_hierarchy_without_explicit() {
            let temp_dir = tempfile::tempdir().unwrap();
            let cwd = temp_dir.path();

            // Create a hierarchy of config files
            let root_config = cwd.join("cgx.toml");
            fs::write(&root_config, "resolve_cache_timeout = \"1m\"").unwrap();

            let sub_dir = cwd.join("subdir");
            fs::create_dir(&sub_dir).unwrap();
            let sub_config = sub_dir.join("cgx.toml");
            fs::write(&sub_config, "resolve_cache_timeout = \"2m\"").unwrap();

            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let discovered = Config::discover_config_files(&sub_dir, &args.config).unwrap();

            // Should contain both hierarchy configs (and possibly system/user if they exist)
            // We check that at least our two configs are present
            assert!(
                discovered.contains(&root_config),
                "Root config should be discovered"
            );
            assert!(
                discovered.contains(&sub_config),
                "Sub config should be discovered"
            );
        }
    }

    mod override_tests {
        use std::fs;

        use super::*;

        mod system_config_dir_tests {
            use super::*;

            #[test]
            fn test_system_config_dir_cli_arg() {
                let temp_dir = tempfile::tempdir().unwrap();
                let system_config_dir = temp_dir.path().join("system");
                fs::create_dir_all(&system_config_dir).unwrap();
                let system_config = system_config_dir.join("cgx.toml");
                fs::write(&system_config, "resolve_cache_timeout = \"5m\"").unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                // Also set user_config_dir to ensure isolation (no real user config is loaded)
                let user_config_dir = temp_dir.path().join("user");
                fs::create_dir_all(&user_config_dir).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.system_config_dir = Some(system_config_dir);
                args.config.user_config_dir = Some(user_config_dir);

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.resolve_cache_timeout, Duration::from_secs(5 * 60));
            }

            #[test]
            fn test_system_config_dir_vs_user_config() {
                let temp_dir = tempfile::tempdir().unwrap();

                // Create system config with 10m timeout
                let system_config_dir = temp_dir.path().join("system");
                fs::create_dir_all(&system_config_dir).unwrap();
                fs::write(
                    system_config_dir.join("cgx.toml"),
                    "resolve_cache_timeout = \"10m\"",
                )
                .unwrap();

                // Create user config with 20m timeout
                let user_config_dir = temp_dir.path().join("user");
                fs::create_dir_all(&user_config_dir).unwrap();
                fs::write(
                    user_config_dir.join("cgx.toml"),
                    "resolve_cache_timeout = \"20m\"",
                )
                .unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.system_config_dir = Some(system_config_dir);
                args.config.user_config_dir = Some(user_config_dir);

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                // User config should override system config
                assert_eq!(config.resolve_cache_timeout, Duration::from_secs(20 * 60));
            }
        }

        mod app_dir_tests {
            use super::*;

            #[test]
            fn test_app_dir_config_location() {
                let temp_dir = tempfile::tempdir().unwrap();
                let app_dir = temp_dir.path().join("app");
                let config_dir = app_dir.join("config");
                fs::create_dir_all(&config_dir).unwrap();
                fs::write(config_dir.join("cgx.toml"), "resolve_cache_timeout = \"7m\"").unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.resolve_cache_timeout, Duration::from_secs(7 * 60));
                assert_eq!(config.config_dir, config_dir);
            }

            #[test]
            fn test_app_dir_cache_location() {
                let temp_dir = tempfile::tempdir().unwrap();
                let app_dir = temp_dir.path().join("app");
                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.cache_dir, app_dir.join("cache"));
            }

            #[test]
            fn test_app_dir_bins_location() {
                let temp_dir = tempfile::tempdir().unwrap();
                let app_dir = temp_dir.path().join("app");
                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.bin_dir, app_dir.join("bins"));
            }

            #[test]
            fn test_app_dir_build_location() {
                let temp_dir = tempfile::tempdir().unwrap();
                let app_dir = temp_dir.path().join("app");
                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.build_dir, app_dir.join("build"));
            }

            #[test]
            fn test_app_dir_complete_isolation() {
                let temp_dir = tempfile::tempdir().unwrap();
                let app_dir = temp_dir.path().join("app");
                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();

                // All directories should be under app_dir
                assert!(config.config_dir.starts_with(&app_dir));
                assert!(config.cache_dir.starts_with(&app_dir));
                assert!(config.bin_dir.starts_with(&app_dir));
                assert!(config.build_dir.starts_with(&app_dir));
            }
        }

        mod user_config_dir_tests {
            use super::*;

            #[test]
            fn test_user_config_dir_cli_arg() {
                let temp_dir = tempfile::tempdir().unwrap();
                let user_config_dir = temp_dir.path().join("user");
                fs::create_dir_all(&user_config_dir).unwrap();
                fs::write(user_config_dir.join("cgx.toml"), "resolve_cache_timeout = \"8m\"").unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.user_config_dir = Some(user_config_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();
                assert_eq!(config.resolve_cache_timeout, Duration::from_secs(8 * 60));
                assert_eq!(config.config_dir, user_config_dir);
            }

            #[test]
            fn test_user_config_dir_overrides_app_dir() {
                let temp_dir = tempfile::tempdir().unwrap();

                // Create app_dir with config
                let app_dir = temp_dir.path().join("app");
                let app_config_dir = app_dir.join("config");
                fs::create_dir_all(&app_config_dir).unwrap();
                fs::write(app_config_dir.join("cgx.toml"), "resolve_cache_timeout = \"9m\"").unwrap();

                // Create user_config_dir with different config
                let user_config_dir = temp_dir.path().join("user");
                fs::create_dir_all(&user_config_dir).unwrap();
                fs::write(
                    user_config_dir.join("cgx.toml"),
                    "resolve_cache_timeout = \"11m\"",
                )
                .unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());
                args.config.user_config_dir = Some(user_config_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();

                // user_config_dir should override app_dir for config location
                assert_eq!(config.resolve_cache_timeout, Duration::from_secs(11 * 60));
                assert_eq!(config.config_dir, user_config_dir);

                // But cache/bins/build should still come from app_dir
                assert_eq!(config.cache_dir, app_dir.join("cache"));
                assert_eq!(config.bin_dir, app_dir.join("bins"));
                assert_eq!(config.build_dir, app_dir.join("build"));
            }
        }

        mod combined_tests {
            use super::*;

            #[test]
            fn test_all_three_overrides() {
                let temp_dir = tempfile::tempdir().unwrap();

                // System config
                let system_config_dir = temp_dir.path().join("system");
                fs::create_dir_all(&system_config_dir).unwrap();
                fs::write(
                    system_config_dir.join("cgx.toml"),
                    "[tools]\nsystem_tool = \"1\"\n[aliases]\ndummytool = \"system\"",
                )
                .unwrap();

                // App dir with config
                let app_dir = temp_dir.path().join("app");
                let app_config_dir = app_dir.join("config");
                fs::create_dir_all(&app_config_dir).unwrap();
                fs::write(app_config_dir.join("cgx.toml"), "[tools]\napp_tool = \"1\"").unwrap();

                // User config dir
                let user_config_dir = temp_dir.path().join("user");
                fs::create_dir_all(&user_config_dir).unwrap();
                fs::write(
                    user_config_dir.join("cgx.toml"),
                    "resolve_cache_timeout = \"12m\"\n[tools]\nuser_tool = \"1\"\n[aliases]\ndummytool = \
                     \"user\"",
                )
                .unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.system_config_dir = Some(system_config_dir);
                args.config.app_dir = Some(app_dir.clone());
                args.config.user_config_dir = Some(user_config_dir.clone());

                let config = Config::load_from_dir(&cwd, &args).unwrap();

                // Should have merged tools from all configs
                assert!(config.tools.contains_key("system_tool"));
                assert!(config.tools.contains_key("user_tool"));
                assert_eq!(config.tools.len(), 2);

                // User config should override alias
                assert_eq!(config.aliases.get("dummytool"), Some(&"user".to_string()));

                // Config dir from user_config_dir
                assert_eq!(config.config_dir, user_config_dir);

                // Other dirs from app_dir
                assert_eq!(config.cache_dir, app_dir.join("cache"));
                assert_eq!(config.bin_dir, app_dir.join("bins"));
                assert_eq!(config.build_dir, app_dir.join("build"));
            }

            #[test]
            fn test_hierarchy_still_works_with_overrides() {
                let temp_dir = tempfile::tempdir().unwrap();

                // App dir
                let app_dir = temp_dir.path().join("app");

                // Create hierarchy with configs
                let root = temp_dir.path().join("work");
                fs::create_dir_all(&root).unwrap();
                fs::write(root.join("cgx.toml"), "[tools]\nroot_tool = \"1\"").unwrap();

                let sub = root.join("sub");
                fs::create_dir_all(&sub).unwrap();
                fs::write(sub.join("cgx.toml"), "[tools]\nsub_tool = \"1\"").unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir);

                let config = Config::load_from_dir(&sub, &args).unwrap();

                // Should have tools from both hierarchy configs
                assert!(config.tools.contains_key("root_tool"));
                assert!(config.tools.contains_key("sub_tool"));
                assert_eq!(config.tools.len(), 2);
            }

            #[test]
            fn test_app_dir_takes_precedence_over_config_file() {
                let temp_dir = tempfile::tempdir().unwrap();

                // App dir
                let app_dir = temp_dir.path().join("app");
                let app_config_dir = app_dir.join("config");
                fs::create_dir_all(&app_config_dir).unwrap();

                // Config file with explicit settings that should be overridden
                let config_file = temp_dir.path().join("explicit.toml");
                let test_config = ConfigFile {
                    cache_dir: Some(temp_dir.path().join("my-cache")),
                    bin_dir: Some(temp_dir.path().join("my-bins")),
                    build_dir: Some(temp_dir.path().join("my-build")),
                    ..Default::default()
                };
                fs::write(&config_file, toml::to_string(&test_config).unwrap()).unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                args.config.app_dir = Some(app_dir.clone());
                args.config.config_file = Some(config_file);

                let config = Config::load_from_dir(&cwd, &args).unwrap();

                // CLI --app-dir should win over config file settings
                assert_eq!(config.cache_dir, app_dir.join("cache"));
                assert_eq!(config.bin_dir, app_dir.join("bins"));
                assert_eq!(config.build_dir, app_dir.join("build"));
            }

            #[test]
            fn test_config_file_paths_used_when_no_app_dir() {
                let temp_dir = tempfile::tempdir().unwrap();

                // Config file with explicit path settings
                let config_file = temp_dir.path().join("explicit.toml");
                let test_config = ConfigFile {
                    cache_dir: Some(temp_dir.path().join("my-cache")),
                    bin_dir: Some(temp_dir.path().join("my-bins")),
                    build_dir: Some(temp_dir.path().join("my-build")),
                    ..Default::default()
                };
                fs::write(&config_file, toml::to_string(&test_config).unwrap()).unwrap();

                let cwd = temp_dir.path().join("work");
                fs::create_dir_all(&cwd).unwrap();

                let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
                // No --app-dir specified
                args.config.config_file = Some(config_file);

                let config = Config::load_from_dir(&cwd, &args).unwrap();

                // Config file paths should be used when --app-dir is not specified
                assert_eq!(config.cache_dir, temp_dir.path().join("my-cache"));
                assert_eq!(config.bin_dir, temp_dir.path().join("my-bins"));
                assert_eq!(config.build_dir, temp_dir.path().join("my-build"));
            }
        }
    }

    mod http_config_deserialization_tests {
        use super::*;

        #[test]
        fn test_deserialize_http_config_full() {
            let toml_content = r#"
                [http]
                timeout = "2m"
                retries = 5
                backoff_base = "1s"
                backoff_max = "30s"
                proxy = "http://proxy.example.com:3128"
            "#;

            let config: ConfigFile = toml::from_str(toml_content).unwrap();
            let http = config.http.unwrap();
            assert_eq!(http.timeout, Some(Duration::from_secs(120)));
            assert_eq!(http.retries, Some(5));
            assert_eq!(http.backoff_base, Some(Duration::from_secs(1)));
            assert_eq!(http.backoff_max, Some(Duration::from_secs(30)));
            assert_eq!(http.proxy, Some("http://proxy.example.com:3128".to_string()));
        }

        #[test]
        fn test_deserialize_http_config_partial() {
            let toml_content = r#"
                [http]
                timeout = "45s"
                retries = 3
            "#;

            let config: ConfigFile = toml::from_str(toml_content).unwrap();
            let http = config.http.unwrap();
            assert_eq!(http.timeout, Some(Duration::from_secs(45)));
            assert_eq!(http.retries, Some(3));
            assert_eq!(http.backoff_base, None);
            assert_eq!(http.backoff_max, None);
            assert_eq!(http.proxy, None);
        }

        #[test]
        fn test_deserialize_http_config_empty_section() {
            let toml_content = r#"
                [http]
            "#;

            let config: ConfigFile = toml::from_str(toml_content).unwrap();
            let http = config.http.unwrap();
            assert_eq!(http.timeout, None);
            assert_eq!(http.retries, None);
            assert_eq!(http.backoff_base, None);
            assert_eq!(http.backoff_max, None);
            assert_eq!(http.proxy, None);
        }

        #[test]
        fn test_deserialize_http_config_unknown_field_rejected() {
            let toml_content = r#"
                [http]
                timeoutt = "30s"
            "#;

            let result: std::result::Result<ConfigFile, _> = toml::from_str(toml_content);
            assert!(result.is_err(), "Expected error for unknown field 'timeoutt'");
        }

        #[test]
        fn test_http_config_default_values() {
            let defaults = HttpConfig::default();
            assert_eq!(defaults.timeout, DEFAULT_HTTP_TIMEOUT);
            assert_eq!(defaults.retries, DEFAULT_HTTP_RETRIES);
            assert_eq!(defaults.backoff_base, DEFAULT_HTTP_BACKOFF_BASE);
            assert_eq!(defaults.backoff_max, DEFAULT_HTTP_BACKOFF_MAX);
            assert_eq!(defaults.proxy, None);
        }
    }

    mod build_http_config_tests {
        use std::io::Write;

        use assert_matches::assert_matches;

        use super::*;

        fn create_temp_config(toml_content: &str) -> tempfile::TempDir {
            let temp_dir = tempfile::tempdir().unwrap();
            let config_path = temp_dir.path().join("cgx.toml");
            let mut file = std::fs::File::create(&config_path).unwrap();
            file.write_all(toml_content.as_bytes()).unwrap();
            temp_dir
        }

        #[test]
        fn test_http_config_all_defaults() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(30));
            assert_eq!(config.http.retries, 2);
            assert_eq!(config.http.backoff_base, Duration::from_millis(500));
            assert_eq!(config.http.backoff_max, Duration::from_secs(5));
            assert_eq!(config.http.proxy, None);
        }

        #[test]
        fn test_http_config_from_config_file() {
            let toml_content = r#"
                [http]
                timeout = "2m"
                retries = 5
                proxy = "http://proxy:3128"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(120));
            assert_eq!(config.http.retries, 5);
            assert_eq!(config.http.proxy, Some("http://proxy:3128".to_string()));
        }

        #[test]
        fn test_http_config_cli_overrides_config_file() {
            let toml_content = r#"
                [http]
                timeout = "2m"
                retries = 5
                proxy = "http://proxy:3128"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let mut args = Cli::parse_from_test_args([
                "--http-timeout",
                "10s",
                "--http-retries",
                "0",
                "--http-proxy",
                "socks5://other:1080",
                "test-crate",
            ])
            .config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(10));
            assert_eq!(config.http.retries, 0);
            assert_eq!(config.http.proxy, Some("socks5://other:1080".to_string()));
        }

        #[test]
        fn test_http_config_cli_overrides_partial() {
            let toml_content = r#"
                [http]
                timeout = "2m"
                retries = 5
                proxy = "http://proxy:3128"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let mut args = Cli::parse_from_test_args(["--http-timeout", "10s", "test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(10));
            assert_eq!(config.http.retries, 5);
            assert_eq!(config.http.proxy, Some("http://proxy:3128".to_string()));
        }

        #[test]
        fn test_http_config_invalid_timeout_duration() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut args =
                Cli::parse_from_test_args(["--http-timeout", "not-a-duration", "test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let result = Config::load_from_dir(temp_dir.path(), &args);
            assert_matches!(result, Err(crate::error::Error::InvalidHttpTimeout { .. }));
        }

        #[test]
        fn test_http_config_zero_retries() {
            let temp_dir = tempfile::tempdir().unwrap();
            let mut args = Cli::parse_from_test_args(["--http-retries", "0", "test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.retries, 0);
        }

        #[test]
        fn test_http_config_backoff_from_config_file() {
            let toml_content = r#"
                [http]
                backoff_base = "2s"
                backoff_max = "60s"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.backoff_base, Duration::from_secs(2));
            assert_eq!(config.http.backoff_max, Duration::from_secs(60));
        }

        #[test]
        fn test_http_config_backoff_defaults_when_not_in_file() {
            let toml_content = r#"
                [http]
                timeout = "45s"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.backoff_base, Duration::from_millis(500));
            assert_eq!(config.http.backoff_max, Duration::from_secs(5));
        }

        #[test]
        /// Verifies hierarchy merge behavior where a child config overrides timeout
        /// while inheriting retries from its parent `[http]` section.
        fn test_http_config_hierarchy_merging_preserves_parent_fields() {
            let temp_dir = tempfile::tempdir().unwrap();

            let parent = temp_dir.path().join("parent");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::write(
                parent.join("cgx.toml"),
                r#"
                [http]
                timeout = "1m"
                retries = 3
                "#,
            )
            .unwrap();

            let child = parent.join("child");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(
                child.join("cgx.toml"),
                r#"
                [http]
                timeout = "45s"
                "#,
            )
            .unwrap();

            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(&child, &args).unwrap();
            // Timeout comes from the child, overriding the parent, but since retries wasn't
            // specified in the child, it should be inherited from the parent config
            assert_eq!(config.http.timeout, Duration::from_secs(45));
            assert_eq!(config.http.retries, 3);
        }

        #[test]
        /// Verifies hierarchy merge behavior where a child config explicitly
        /// overrides parent timeout and retries fields in `[http]`.
        fn test_http_config_hierarchy_merging_child_overrides_parent_fields() {
            let temp_dir = tempfile::tempdir().unwrap();

            let parent = temp_dir.path().join("parent");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::write(
                parent.join("cgx.toml"),
                r#"
                [http]
                timeout = "1m"
                retries = 3
                "#,
            )
            .unwrap();

            let child = parent.join("child");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(
                child.join("cgx.toml"),
                r#"
                [http]
                timeout = "45s"
                retries = 5
                "#,
            )
            .unwrap();

            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(&child, &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(45));
            assert_eq!(config.http.retries, 5);
        }
    }

    mod build_http_config_env_tests {
        use std::io::Write;

        use sealed_test::prelude::*;

        use super::*;

        fn create_temp_config(toml_content: &str) -> tempfile::TempDir {
            let temp_dir = tempfile::tempdir().unwrap();
            let config_path = temp_dir.path().join("cgx.toml");
            let mut file = std::fs::File::create(&config_path).unwrap();
            file.write_all(toml_content.as_bytes()).unwrap();
            temp_dir
        }

        #[sealed_test(env = [("CARGO_HTTP_TIMEOUT", "45")])]
        /// Verifies `CARGO_HTTP_TIMEOUT` is used when neither CLI nor config file sets timeout.
        fn test_env_timeout_used_when_no_cli_or_config() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(45));
        }

        #[sealed_test(env = [("CARGO_NET_RETRY", "7")])]
        /// Verifies `CARGO_NET_RETRY` is used when neither CLI nor config file sets retries.
        fn test_env_retries_used_when_no_cli_or_config() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.retries, 7);
        }

        #[sealed_test(env = [("CARGO_HTTP_PROXY", "socks5://env-proxy:1080")])]
        /// Verifies `CARGO_HTTP_PROXY` is used when neither CLI nor config file sets proxy.
        fn test_env_proxy_used_when_no_cli_or_config() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.proxy, Some("socks5://env-proxy:1080".to_string()));
        }

        #[sealed_test(env = [
            ("CARGO_HTTP_TIMEOUT", "45"),
            ("CARGO_NET_RETRY", "7"),
            ("CARGO_HTTP_PROXY", "http://env-proxy:3128")
        ])]
        /// Verifies CLI HTTP flags take precedence over Cargo HTTP environment variables.
        fn test_cli_overrides_env() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args([
                    "--http-timeout",
                    "10s",
                    "--http-retries",
                    "1",
                    "--http-proxy",
                    "socks5://cli-proxy:1080",
                    "test-crate",
                ])
                .config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(10));
            assert_eq!(config.http.retries, 1);
            assert_eq!(config.http.proxy, Some("socks5://cli-proxy:1080".to_string()));
        }

        #[sealed_test(env = [
            ("CARGO_HTTP_TIMEOUT", "45"),
            ("CARGO_NET_RETRY", "7"),
            ("CARGO_HTTP_PROXY", "http://env-proxy:3128")
        ])]
        /// Verifies config file `[http]` values take precedence over Cargo HTTP env variables.
        fn test_config_file_overrides_env() {
            let toml_content = r#"
                [http]
                timeout = "2m"
                retries = 5
                proxy = "http://config-proxy:8080"
            "#;
            let temp_dir = create_temp_config(toml_content);
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, Duration::from_secs(120));
            assert_eq!(config.http.retries, 5);
            assert_eq!(config.http.proxy, Some("http://config-proxy:8080".to_string()));
        }

        #[sealed_test(env = [("CARGO_HTTP_TIMEOUT", "not-a-number")])]
        /// Verifies invalid `CARGO_HTTP_TIMEOUT` falls back to the built-in default timeout.
        fn test_invalid_env_timeout_falls_back_to_default() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.timeout, DEFAULT_HTTP_TIMEOUT);
        }

        #[sealed_test(env = [("CARGO_NET_RETRY", "not-a-number")])]
        /// Verifies invalid `CARGO_NET_RETRY` falls back to the built-in default retries value.
        fn test_invalid_env_retries_falls_back_to_default() {
            let temp_dir = tempfile::tempdir().unwrap();
            let args = with_isolated_global_config(
                Cli::parse_from_test_args(["test-crate"]).config_inputs(),
                temp_dir.path(),
            );

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();
            assert_eq!(config.http.retries, DEFAULT_HTTP_RETRIES);
        }
    }

    mod build_http_config_direct_tests {
        use super::*;

        #[test]
        fn test_config_file_timeout_overrides_defaults() {
            let config_file = HttpConfigFile {
                timeout: Some(Duration::from_secs(120)),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.timeout, Duration::from_secs(120));
            assert_eq!(http.retries, DEFAULT_HTTP_RETRIES);
        }

        #[test]
        fn test_config_file_retries_overrides_defaults() {
            let config_file = HttpConfigFile {
                retries: Some(10),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.retries, 10);
        }

        #[test]
        fn test_config_file_proxy_overrides_defaults() {
            let config_file = HttpConfigFile {
                proxy: Some("http://proxy:3128".to_string()),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.proxy, Some("http://proxy:3128".to_string()));
        }

        #[test]
        fn test_cli_timeout_overrides_config_file() {
            let config_file = HttpConfigFile {
                timeout: Some(Duration::from_secs(120)),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["--http-timeout", "10s", "test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.timeout, Duration::from_secs(10));
        }

        #[test]
        fn test_cli_retries_overrides_config_file() {
            let config_file = HttpConfigFile {
                retries: Some(10),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["--http-retries", "0", "test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.retries, 0);
        }

        #[test]
        fn test_cli_proxy_overrides_config_file() {
            let config_file = HttpConfigFile {
                proxy: Some("http://old:3128".to_string()),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["--http-proxy", "socks5://new:1080", "test-crate"])
                .config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.proxy, Some("socks5://new:1080".to_string()));
        }

        #[test]
        fn test_empty_config_file_yields_defaults() {
            let config_file = HttpConfigFile::default();
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.timeout, DEFAULT_HTTP_TIMEOUT);
            assert_eq!(http.retries, DEFAULT_HTTP_RETRIES);
            assert_eq!(http.backoff_base, DEFAULT_HTTP_BACKOFF_BASE);
            assert_eq!(http.backoff_max, DEFAULT_HTTP_BACKOFF_MAX);
            assert_eq!(http.proxy, None);
        }

        #[test]
        fn test_backoff_from_config_file() {
            let config_file = HttpConfigFile {
                backoff_base: Some(Duration::from_secs(2)),
                backoff_max: Some(Duration::from_secs(60)),
                ..Default::default()
            };
            let args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            let http = Config::build_http_config(&config_file, &args.http).unwrap();
            assert_eq!(http.backoff_base, Duration::from_secs(2));
            assert_eq!(http.backoff_max, Duration::from_secs(60));
        }
    }

    mod error_tests {
        use assert_matches::assert_matches;

        use super::*;

        #[test]
        fn test_invalid_toml_syntax() {
            let test_case = crate::testdata::ConfigTestCase::invalid_toml();

            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(test_case.path().to_path_buf());

            let result = Config::load(&args);
            assert_matches!(result, Err(crate::error::Error::ConfigExtract { .. }));
        }

        #[test]
        fn test_invalid_config_options_raise_error() {
            let test_case = crate::testdata::ConfigTestCase::invalid_options();

            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(test_case.path().to_path_buf());

            let result = Config::load(&args);
            assert_matches!(result, Err(crate::error::Error::ConfigExtract { .. }));
        }

        #[test]
        fn test_nonexistent_explicit_config_file() {
            let test_case = crate::testdata::ConfigTestCase::nonexistent();

            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            args.config.config_file = Some(test_case.path().to_path_buf());

            let config = Config::load(&args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(60 * 60));
        }

        #[test]
        fn test_no_config_files_uses_defaults() {
            let temp_dir = tempfile::tempdir().unwrap();

            let mut args = Cli::parse_from_test_args(["test-crate"]).config_inputs();
            // Ensure isolation from developer's real cgx config on their system.
            // Without these overrides, this test would load ~/.config/cgx/cgx.toml if it exists,
            // causing the test to fail with config values from the developer's actual config.
            args.config.system_config_dir = Some(temp_dir.path().join("system"));
            args.config.user_config_dir = Some(temp_dir.path().join("user"));

            let config = Config::load_from_dir(temp_dir.path(), &args).unwrap();

            assert_eq!(config.resolve_cache_timeout, Duration::from_secs(60 * 60));
            assert!(!config.offline);
            assert!(config.locked); // Default is true per issue #55
            assert_eq!(config.toolchain, None);
            assert_eq!(config.tools.len(), 0);
            assert_eq!(config.aliases.len(), 0);
        }
    }
}
