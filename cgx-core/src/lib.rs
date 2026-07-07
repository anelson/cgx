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
pub(crate) mod http;
pub(crate) mod logging;
pub mod messages;
pub(crate) mod registry;
pub mod runner;
pub(crate) mod sbom;
pub(crate) mod target;
#[cfg(test)]
pub(crate) mod testdata;

use std::sync::Arc;

use bin_resolver::BinaryResolver;
use builder::{BuildOptions, BuildOverrides, CrateBuilder};
use cache::Cache;
// Re-export this third-party crate type that is nonetheless part of this crate's public API
pub use cargo_metadata::Target;
use config::Config;
use crate_resolver::CrateResolver;
use cratespec::{CrateRequest, CrateSpec};
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
    config: Config,
    resolver: Arc<dyn CrateResolver>,
    bin_resolver: Arc<dyn BinaryResolver>,
    downloader: Arc<dyn CrateDownloader>,
    builder: Arc<dyn CrateBuilder>,
    reporter: messages::MessageReporter,
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

        let cargo_runner = Arc::new(cargo::create_cargo_runner(config.clone(), reporter.clone())?);

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
        )?);

        let downloader = Arc::new(downloader::create_downloader(
            config.clone(),
            cache.clone(),
            git_client,
            http_client,
        ));

        let builder = Arc::new(builder::create_builder(config.clone(), cache, cargo_runner));

        Ok(Self {
            config,
            resolver,
            bin_resolver,
            downloader,
            builder,
            reporter,
        })
    }

    /// Resolve a crate request into its build plan and produce the path to its binary.
    ///
    /// This is the shared load-and-build impl that powers `cgx <crate>`, `--no-exec`, `--prefetch`,
    /// and each crate prefetched by `--prefetch-all`:
    /// - loads the [`CrateSpec`] and [`BuildOptions`] from the engine's config plus `overrides`,
    /// - resolves the spec to a concrete version and downloads the source,
    /// - returns a pre-built binary if one is available and enabled, otherwise builds from source.
    ///
    /// Returns the fully-qualified path to the crate's binary. Does NOT execute it.
    pub fn prepare_bin_crate(
        &self,
        request: &CrateRequest,
        overrides: &BuildOverrides,
    ) -> Result<std::path::PathBuf> {
        let (crate_spec, build_options) = self.resolve_crate_request(request, overrides)?;
        let downloaded_crate = self.resolve_and_download_crate_spec(&crate_spec, &build_options)?;

        // Try to resolve a pre-built binary, now with access to the downloaded source
        tracing::debug!("Attempting to resolve pre-built binary");
        if let Some(resolved_binary) = self.bin_resolver.resolve(&downloaded_crate, &build_options)? {
            let provider = resolved_binary.provider;
            tracing::info!(
                "Found pre-built binary from {:?} at: {}",
                provider,
                resolved_binary.path.display()
            );
            self.reporter.report(|| {
                messages::CgxMessage::crate_provenance_prebuilt(
                    &downloaded_crate.resolved,
                    &downloaded_crate.crate_path,
                    &build_options,
                    &resolved_binary,
                )
            });
            return Ok(resolved_binary.path);
        }

        // No pre-built binary available, fall back to building from source
        tracing::info!(
            "Pre-built binary not found, excluded by config, or disabled; building crate from source..."
        );

        let (bin_path, target_binary) = self.builder.build(&downloaded_crate, &build_options)?;

        tracing::info!("Built crate binary at: {}", bin_path.display());
        self.reporter.report(|| {
            messages::CgxMessage::crate_provenance_built_from_source(
                &downloaded_crate.resolved,
                &downloaded_crate.crate_path,
                &build_options,
                &bin_path,
                target_binary,
            )
        });

        Ok(bin_path)
    }

    /// Resolve a crate request and list its runnable targets without building or executing.
    ///
    /// Loads the [`CrateSpec`]/[`BuildOptions`] from config plus `overrides`  and downloads the
    /// source if needed, but does not build from source or look for a pre-built binary.
    ///
    /// Returns `(crate_name, default_target, bin_targets, example_targets)`.
    #[expect(
        clippy::type_complexity,
        reason = "the returned 4-tuple is documented above and clearer here than a one-off named struct"
    )]
    pub fn list_targets(
        &self,
        request: &CrateRequest,
        overrides: &BuildOverrides,
    ) -> Result<(String, Option<Target>, Vec<Target>, Vec<Target>)> {
        let (crate_spec, build_options) = self.resolve_crate_request(request, overrides)?;
        let downloaded_crate = self.resolve_and_download_crate_spec(&crate_spec, &build_options)?;
        let crate_name = downloaded_crate.resolved.name.clone();
        let (default, bins, examples) = self.builder.list_targets(&downloaded_crate, &build_options)?;
        Ok((crate_name, default, bins, examples))
    }

    /// Enumerate the configured tools and aliases in the config, then return the rendered
    /// `[tools]`/`[aliases]` TOML.
    pub fn list_configured_tools(&self) -> Result<String> {
        // Emit appropriate messages as we enumerate the tools/aliases
        for (name, tool_config) in self.config.sorted_tools() {
            self.reporter
                .report(|| messages::RunnerMessage::list_tool(name, tool_config));
        }
        for (name, target) in self.config.sorted_aliases() {
            self.reporter
                .report(|| messages::RunnerMessage::list_alias(name, target));
        }

        self.config.tools_toml()
    }

    /// Prefetch a single crate request: prepare its binary without executing it, reporting
    /// progress via [`messages::RunnerMessage::PrefetchStarted`] and
    /// [`messages::RunnerMessage::PrefetchCompleted`].
    ///
    /// This is the single-crate counterpart to [`Cgx::prefetch_all`]. `label` is the user-facing
    /// identifier shown in those messages: the raw CLI crate spec (e.g. `ripgrep@1.0`), the
    /// resolved crate name, or a `<source>` placeholder for source-only invocations.
    pub fn prefetch(&self, label: &str, request: &CrateRequest, overrides: &BuildOverrides) -> Result<()> {
        self.reporter
            .report(|| messages::RunnerMessage::prefetch_started(label));

        let bin_path = self.prepare_bin_crate(request, overrides)?;

        self.reporter
            .report(|| messages::RunnerMessage::prefetch_completed(label, &bin_path));

        Ok(())
    }

    /// Prefetch every tool and alias configured in the `[tools]`/`[aliases]` config sections.
    ///
    /// Tools and aliases are grouped by the crate they resolve to, so each distinct crate is
    /// prefetched exactly once no matter how many configured names point at it; the configured
    /// names are reported alongside it. Every configured tool is attempted even if some fail; if
    /// any failed, this returns [`error::Error::PrefetchAllFailed`] listing them.
    pub fn prefetch_all(&self, overrides: &BuildOverrides) -> Result<()> {
        let mut failures = Vec::new();

        for tool in self.config.configured_tools() {
            self.reporter
                .report(|| messages::RunnerMessage::prefetch_all_started(&tool.name, &tool.aliases));

            let request = CrateRequest::for_configured_tool(&tool.name);
            match self.prepare_bin_crate(&request, overrides) {
                Ok(bin_path) => {
                    self.reporter.report(|| {
                        messages::RunnerMessage::prefetch_all_completed(&tool.name, &tool.aliases, &bin_path)
                    });
                }
                Err(err) => {
                    self.reporter.report(|| {
                        messages::RunnerMessage::prefetch_all_failed(&tool.name, &tool.aliases, &err)
                    });
                    failures.push(format!("{}: {}", tool.name, err));
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            error::PrefetchAllFailedSnafu { failures }.fail()
        }
    }

    /// Load the resolved [`CrateSpec`] and [`BuildOptions`] for a crate request.
    ///
    /// Applies the engine's config and `overrides` to turn a [`CrateRequest`] into the concrete
    /// crate spec and build options.
    fn resolve_crate_request(
        &self,
        request: &CrateRequest,
        overrides: &BuildOverrides,
    ) -> Result<(CrateSpec, BuildOptions)> {
        let crate_spec = CrateSpec::load(&self.config, request)?;
        let build_options = BuildOptions::load_for_crate(&self.config, overrides, &crate_spec)?;
        Ok((crate_spec, build_options))
    }

    /// Resolve and download a crate given an already-resolved [`CrateSpec`], producing a
    /// [`downloader::DownloadedCrate`].
    ///
    /// NOTE: This is downloading the crate source code, which must be done even if we eventually
    /// end up selecting a prebuilt binary instead of building from source.
    fn resolve_and_download_crate_spec(
        &self,
        crate_spec: &CrateSpec,
        build_options: &BuildOptions,
    ) -> Result<downloader::DownloadedCrate> {
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

        self.reporter.report(|| {
            messages::CgxMessage::crate_plan(
                &downloaded_crate.resolved,
                &downloaded_crate.crate_path,
                build_options,
            )
        });

        Ok(downloaded_crate)
    }
}
