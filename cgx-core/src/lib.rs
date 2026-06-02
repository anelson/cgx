pub mod bin_resolver;
pub mod builder;
pub(crate) mod cache;
pub mod cargo;
pub mod cli;
pub mod config;
pub mod crate_resolver;
pub mod cratespec;
pub mod downloader;
pub mod error;
pub mod git;
pub(crate) mod helpers;
pub mod http;
pub(crate) mod logging;
pub mod messages;
pub(crate) mod registry;
pub mod runner;
pub(crate) mod sbom;
#[cfg(test)]
pub(crate) mod testdata;

use std::sync::Arc;

use bin_resolver::BinaryResolver;
use builder::{BuildOptions, CrateBuilder};
use cache::Cache;
// Re-export this third-party crate type that is nonetheless part of this crate's public API
pub use cargo_metadata::Target;
use config::Config;
use crate_resolver::CrateResolver;
use cratespec::CrateSpec;
use downloader::CrateDownloader;
use error::Result;
use http::HttpClient;

/// Instance of the engine that powers the `cgx` tool.
///
/// This is packaged this way so that our `main.rs` is as minimal as possible.  That's useful for a
/// few reasons, but in our particular case it's because we want to be able to add `cgx` as a crate
/// in others' workspaces so that it can be invoked with `cargo run` or aliases and always
/// available to everyone using the project whether or not they previously installed `cgx` on their
/// systems.
pub struct Cgx {
    resolver: Arc<dyn CrateResolver>,
    bin_resolver: Arc<dyn BinaryResolver>,
    downloader: Arc<dyn CrateDownloader>,
    builder: Arc<dyn CrateBuilder>,
}

impl Cgx {
    /// Create a new instance from a loaded configuration.
    ///
    /// The config should be loaded using [`Config::load()`] with the CLI args.
    pub fn new(config: Config, reporter: messages::MessageReporter) -> Result<Self> {
        tracing::debug!("Using config: {:#?}", config);

        let http_client = HttpClient::new(&config.http)?;

        let cache = Cache::new(config.clone(), reporter.clone());
        let git_client = git::GitClient::new(cache.clone(), reporter.clone(), config.http.clone());

        let cargo_runner = Arc::new(cargo::find_cargo(reporter.clone())?);

        let resolver = Arc::new(crate_resolver::create_resolver(
            config.clone(),
            cache.clone(),
            git_client.clone(),
            cargo_runner.clone(),
            http_client.clone(),
        ));

        let bin_resolver = Arc::new(bin_resolver::create_resolver(
            config.clone(),
            cache.clone(),
            reporter.clone(),
            http_client.clone(),
        ));

        let downloader = Arc::new(downloader::create_downloader(
            config.clone(),
            cache.clone(),
            git_client,
            http_client,
        ));

        let builder = Arc::new(builder::create_builder(config, cache, cargo_runner));

        Ok(Self {
            resolver,
            bin_resolver,
            downloader,
            builder,
        })
    }

    /// Run the cgx engine with the given crate spec and build options.
    ///
    /// This is the main execution path that:
    /// - Resolves the crate spec to a concrete version
    /// - Downloads the crate source to the cache
    /// - Attempts to find a pre-built binary (if enabled)
    /// - If no pre-built binary, builds the crate binary from source
    /// - Returns the path to the binary
    ///
    /// This method does NOT execute the binary - that's left to the caller.
    pub fn crate_to_bin(
        &self,
        crate_spec: &CrateSpec,
        build_options: &BuildOptions,
    ) -> Result<std::path::PathBuf> {
        tracing::debug!("Got crate spec: {:?}", crate_spec);

        tracing::info!("Resolving crate...");
        let resolved_crate = self.resolver.resolve(crate_spec)?;

        tracing::info!(
            "Resolved crate {}@{}",
            resolved_crate.name,
            resolved_crate.version
        );

        let downloaded_crate = self.downloader.download(resolved_crate)?;

        tracing::debug!("Downloaded crate to cache: {:#?}", downloaded_crate);

        // Try to resolve a pre-built binary, now with access to the downloaded source
        tracing::debug!("Attempting to resolve pre-built binary");
        if let Some(resolved_binary) = self.bin_resolver.resolve(&downloaded_crate, build_options)? {
            tracing::info!(
                "Found pre-built binary from {:?} at: {}",
                resolved_binary.provider,
                resolved_binary.path.display()
            );
            return Ok(resolved_binary.path);
        }

        // No pre-built binary available, fall back to building from source
        tracing::info!(
            "Pre-built binary not found, excluded by config, or or disabled; building crate from source..."
        );

        let bin_path = self.builder.build(&downloaded_crate, build_options)?;

        tracing::info!("Built crate binary at: {}", bin_path.display());

        Ok(bin_path)
    }

    /// List the available targets (binaries and examples) in a crate.
    ///
    /// Returns a tuple of:
    /// - `String`: The crate name
    /// - `Option<Target>`: The default target if one is specified
    /// - `Vec<Target>`: All binary targets
    /// - `Vec<Target>`: All example targets
    #[allow(clippy::type_complexity)]
    pub fn list_targets(
        &self,
        crate_spec: &CrateSpec,
        build_options: &BuildOptions,
    ) -> Result<(String, Option<Target>, Vec<Target>, Vec<Target>)> {
        let resolved_crate = self.resolver.resolve(crate_spec)?;
        let crate_name = resolved_crate.name.clone();
        let downloaded_crate = self.downloader.download(resolved_crate)?;
        let (default, bins, examples) = self.builder.list_targets(&downloaded_crate, build_options)?;
        Ok((crate_name, default, bins, examples))
    }
}
