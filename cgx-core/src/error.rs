use std::path::PathBuf;

pub use reqwest::StatusCode;
use snafu::prelude::*;

use crate::config::BinaryProvider;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
#[non_exhaustive]
pub enum Error {
    #[snafu(display("Crate name is required"))]
    MissingCrateParameter,

    #[snafu(display("Missing crate name in crate spec '{spec}'"))]
    MissingCrateName { spec: String },

    #[snafu(display("Repository format must be 'owner/repo', got '{repo}'"))]
    InvalidRepoFormat { repo: String },

    #[snafu(display(
        "Git selectors (--branch, --tag, --rev) can only be used with git sources (--git, --github, \
         --gitlab)"
    ))]
    GitSelectorWithoutGitSource,

    #[snafu(display("Invalid version requirement '{version}': {source}"))]
    InvalidVersionReq { version: String, source: semver::Error },

    #[snafu(display("Invalid URL '{url}': {source}"))]
    InvalidUrl { url: String, source: url::ParseError },

    #[snafu(display(
        "Crate versions in the crate name ({at_version}) and the --crate-version flag ({flag_version}) are \
         mutually exclusive; specify one or the other but not both"
    ))]
    ConflictingVersions {
        at_version: String,
        flag_version: String,
    },

    #[snafu(display(
        "cgx cannot run cargo itself, and pinning a cargo version is not supported. To run a cargo \
         subcommand through cgx, use `cgx cargo <subcommand>` (e.g. `cgx cargo deny`) or the plugin crate \
         name directly (e.g. `cgx cargo-deny`)"
    ))]
    CargoNotRunnable,

    // Resolution errors
    #[snafu(display("Crate '{name}' not found in registry"))]
    CrateNotFoundInRegistry { name: String },

    #[snafu(display("No version of crate '{name}' matches requirement '{requirement}'"))]
    NoMatchingVersion { name: String, requirement: String },

    #[snafu(display(
        "Package '{}' not found in workspace. Available packages: {}",
        name,
        available.join(", ")
    ))]
    PackageNotFoundInWorkspace { name: String, available: Vec<String> },

    #[snafu(display(
        "Ambiguous package name: found {count} packages in workspace, but no name was specified. Specify \
         which package to use with the 'name' field."
    ))]
    AmbiguousPackageName { count: usize },

    #[snafu(display("The crate '{krate}' does not have any binary targets so it cannot be executed"))]
    NoPackageBinaries { krate: String },

    #[snafu(display(
        "Package '{}' has multiple binary targets [{}], but no default was specified. Use --bin to \
         specify which binary to build, or set 'default-run' in Cargo.toml",
        package,
        available.join(", ")
    ))]
    AmbiguousBinaryTarget { package: String, available: Vec<String> },

    #[snafu(display(
        "Package '{package}' does not contain a {kind} target named '{target}'. Available {kind} targets: {}",
        available.join(", ")
    ))]
    RunnableTargetNotFound {
        kind: &'static str,
        package: String,
        target: String,
        available: Vec<String>,
    },

    #[snafu(display("Version mismatch: required version '{requirement}' but found '{found}'"))]
    VersionMismatch {
        requirement: String,
        found: semver::Version,
    },

    #[snafu(transparent)]
    Git {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to query registry: {source}"))]
    Registry { source: tame_index::Error },

    #[snafu(display("Error invoking `{}` to read metadata from source dir `{}`: {}",
        cargo_path.display(),
        source_dir.display(),
        source
    ))]
    CargoMetadata {
        cargo_path: PathBuf,
        source_dir: PathBuf,
        source: cargo_metadata::Error,
    },

    #[snafu(display("Cargo.toml not found in {}", source_dir.display()))]
    CargoTomlNotFound { source_dir: PathBuf },

    #[snafu(display("Failed to parse version '{version}': {source}"))]
    InvalidVersion { version: String, source: semver::Error },

    #[snafu(display("{}: {}", path.display(), source))]
    Io { path: PathBuf, source: std::io::Error },

    #[snafu(display("Failed to rename {} to {}: {}", src.display(), dst.display(), source))]
    RenameFile {
        src: PathBuf,
        dst: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to copy binary from {} to {}: {}", src.display(), dst.display(), source))]
    CopyBinary {
        src: PathBuf,
        dst: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to create temporary directory in {}: {}", parent.display(), source))]
    TempDirCreation { parent: PathBuf, source: std::io::Error },

    #[snafu(display("Failed to execute command: {}", source))]
    CommandExecution { source: std::io::Error },

    #[snafu(display("Failed to build SBOM component: {}", message))]
    SbomBuilder { message: String },

    #[snafu(display("JSON serialization error: {source}"))]
    Json { source: serde_json::Error },

    #[snafu(display("TOML serialization error: {source}"))]
    TomlSerialize { source: toml::ser::Error },

    #[snafu(display("Cannot download '{name}' v{version}: network required but offline mode enabled"))]
    OfflineMode { name: String, version: String },

    #[snafu(display("Failed to download registry crate: {source}"))]
    RegistryDownload { source: reqwest::Error },

    #[snafu(display("Failed to extract crate tarball: {source}"))]
    TarExtraction { source: std::io::Error },

    #[snafu(display("Download URL not available for crate '{name}' version '{version}'"))]
    DownloadUrlUnavailable { name: String, version: String },

    #[snafu(display("Executable '{name}' not found in PATH or standard locations"))]
    ExecutableNotFound { name: String },

    #[snafu(display("Toolchain '{toolchain}' specified but rustup not found"))]
    RustupNotFound { toolchain: String },

    #[snafu(display("Expected binary not found in cargo build output"))]
    BinaryNotFoundInOutput,

    #[snafu(display(
        "cargo build failed with exit code {}",
        exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
    ))]
    CargoBuildFailed { exit_code: Option<i32> },

    #[snafu(display("Failed to copy source tree from {} to {}: {}", src.display(), dst.display(), source))]
    CopySourceTree {
        src: PathBuf,
        dst: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    // Configuration loading errors
    #[snafu(display("Failed to load configuration from {}: {}", path.display(), source))]
    ConfigLoad { path: PathBuf, source: figment::Error },

    #[snafu(display("Invalid configuration value for '{}': {}", field, message))]
    InvalidConfigValue { field: String, message: String },

    #[snafu(display("Failed to extract configuration: {}", source))]
    ConfigExtract { source: figment::Error },

    // Binary execution errors
    #[snafu(display("Failed to execute binary at {}: {source}", path.display()))]
    ExecFailed { path: PathBuf, source: std::io::Error },

    #[snafu(display("Failed to spawn process at {}: {source}", path.display()))]
    SpawnFailed { path: PathBuf, source: std::io::Error },

    #[snafu(display("Failed to wait for child process: {source}"))]
    WaitFailed { source: std::io::Error },

    #[cfg(windows)]
    #[snafu(display("Failed to set up Windows console control handler"))]
    ConsoleHandlerFailed { source: ctrlc::Error },

    #[snafu(display("Error determining home directory"))]
    Etcetera { source: etcetera::HomeDirError },

    // Prebuilt binary resolution errors
    #[snafu(display(
        "No binary providers are configured, but prebuilt binaries are enabled. Either enable at least one \
         binary provider or set use_prebuilt_binaries to 'never'."
    ))]
    NoProvidersConfigured,

    #[snafu(display(
        "Prebuilt binary required (--prebuilt-binary always) but no prebuilt binary found for crate \
         '{name}' version '{version}'"
    ))]
    PrebuiltBinaryRequired { name: String, version: String },

    #[snafu(display(
        "Prebuilt binary required (--prebuilt-binary always) but resolution could not be completed for \
         crate '{name}' version '{version}'"
    ))]
    PrebuiltBinaryResolutionFailed {
        name: String,
        version: String,
        source: Box<Error>,
    },

    #[snafu(display(
        "Prebuilt binary required (--prebuilt-binary always) but {reason}, which requires building crate \
         '{name}' version '{version}' from source"
    ))]
    PrebuiltBinaryDisqualified {
        name: String,
        version: String,
        reason: String,
    },

    #[snafu(display(
        "Checksum verification failed for downloaded binary: expected {expected}, got {actual}"
    ))]
    ChecksumMismatch { expected: String, actual: String },

    #[snafu(display("Failed to parse SHA256 checksum contents: {contents}"))]
    ChecksumUnparsable { contents: String },

    #[snafu(display("Provider {provider:?} failed to prepare asset {url}: {source}"))]
    ProviderAssetPreparationFailed {
        provider: BinaryProvider,
        url: String,
        source: Box<Error>,
    },

    #[snafu(display("Unsupported archive format: {format}"))]
    UnsupportedArchiveFormat { format: String },

    #[snafu(display("GitHub API error: {source}"))]
    GithubApiError {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Quickinstall API error: {source}"))]
    QuickinstallApiError {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to download prebuilt binary from {url}: {source}"))]
    BinaryDownloadFailed { url: String, source: reqwest::Error },

    #[snafu(display("HTTP {} downloading prebuilt binary from {url}: {source}", status.as_u16()))]
    BinaryDownloadHttpError {
        url: String,
        status: StatusCode,
        source: reqwest::Error,
    },

    #[snafu(display("Failed to extract binary archive: {source}"))]
    ArchiveExtractionFailed {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to parse {}: {}", path.display(), source))]
    CargoTomlParse { path: PathBuf, source: toml::de::Error },

    #[snafu(display("Invalid [package.metadata.binstall]: {source}"))]
    BinstallMetadataInvalidInCargoToml { source: toml::de::Error },

    #[snafu(display("Invalid binstall template '{template}': {source}"))]
    BinstallTemplateParse {
        template: String,
        source: leon::ParseError,
    },

    #[snafu(display("Failed to render binstall template '{template}': {source}"))]
    BinstallTemplateRender {
        template: String,
        source: leon::RenderError,
    },

    #[snafu(display("Invalid target triple '{target}': {message}"))]
    InvalidTargetTriple { target: String, message: String },

    #[snafu(display("Failed to build HTTP client: {message}"))]
    HttpClientBuild { message: String },

    #[snafu(display("HTTP request to {url} failed: {source}"))]
    HttpRequest { url: String, source: reqwest::Error },

    #[snafu(display("Failed to stream HTTP response from {url} to {}: {source}", path.display()))]
    HttpDownloadToFile {
        url: String,
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("HTTP response from {url} exceeded maximum size of {limit} bytes"))]
    HttpResponseTooLarge { url: String, limit: usize },

    #[snafu(display("Failed to read HTTP response body from {url}: {source}"))]
    HttpResponseRead { url: String, source: std::io::Error },

    #[snafu(display("HTTP response from {url} is not valid UTF-8: {source}"))]
    HttpResponseUtf8 {
        url: String,
        source: std::string::FromUtf8Error,
    },

    #[snafu(display("HTTP {status} from {url}"))]
    HttpStatus { url: String, status: u16 },

    #[snafu(display("Provider request to {url} was throttled: HTTP {status}"))]
    ProviderThrottled { url: String, status: u16 },

    #[snafu(display(
        "Failed to prefetch {} configured tool(s): {}",
        failures.len(),
        failures.join("; ")
    ))]
    PrefetchAllFailed { failures: Vec<String> },

    #[snafu(display("Invalid HTTP timeout duration '{value}': {source}"))]
    InvalidHttpTimeout {
        value: String,
        source: humantime::DurationError,
    },
}

impl Error {
    /// Check whether an HTTP operation error should be retried by [`crate::http::HttpClient`].
    pub(crate) fn is_retryable_http_error(&self) -> bool {
        match self {
            Self::HttpStatus { .. } => true,
            Self::HttpRequest { source, .. } => {
                source.is_connect() || source.is_timeout() || source.is_request()
            }
            _ => false,
        }
    }
}

impl From<crate::git::Error> for Error {
    fn from(e: crate::git::Error) -> Self {
        Self::Git {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
