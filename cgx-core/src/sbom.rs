use std::collections::{HashMap, HashSet};

// Re-export the CycloneDx type so callers don't depend on third-party crate
pub(crate) use serde_cyclonedx::cyclonedx::v_1_4::CycloneDx;
use serde_cyclonedx::cyclonedx::v_1_4::{
    Component, ComponentBuilder, CycloneDxBuilder, Dependency, DependencyBuilder, Metadata, MetadataBuilder,
    PropertyBuilder, ToolBuilder,
};

use crate::{Result, builder::BuildOptions, crate_resolver::ResolvedCrate};

/// Generate a `CycloneDX` SBOM from cargo metadata.
///
/// Creates a Software Bill of Materials describing the crate being built, its dependencies,
/// and the build configuration used to produce the binary.
///
/// # Arguments
///
/// * `metadata` - Cargo metadata containing dependency information
/// * `resolved` - The resolved crate being built
/// * `options` - Build options that affect the output binary
///
/// # Returns
///
/// A [`CycloneDx`] struct in version 1.4 format.
pub(crate) fn generate_sbom(
    metadata: &cargo_metadata::Metadata,
    resolved: &ResolvedCrate,
    options: &BuildOptions,
) -> Result<CycloneDx> {
    // Build the main component (the crate being built)
    let main_component = build_main_component(resolved, options)?;

    // Build components for all dependencies
    let (dependencies, _kind_map) = build_dependency_components(metadata)?;

    // Build dependency relationships
    let dependency_refs = build_dependency_graph(metadata)?;

    // Build metadata section with tool information
    let sbom_metadata = build_metadata(&main_component)?;

    // Construct the CycloneDX BOM
    CycloneDxBuilder::default()
        .bom_format("CycloneDX")
        .spec_version("1.4")
        .version(1)
        .serial_number(format!("urn:uuid:{}", uuid::Uuid::new_v4()))
        .metadata(sbom_metadata)
        .components(dependencies)
        .dependencies(dependency_refs)
        .build()
        .map_err(|e| {
            crate::error::SbomBuilderSnafu {
                message: e.to_string(),
            }
            .build()
        })
}

/// Dependency kind for SBOM classification.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DepKind {
    Build,   // Build-time dependency (build scripts, proc-macros)
    Runtime, // Runtime dependency (linked into binary)
}

impl DepKind {
    fn from_cargo_kinds(kinds: &[cargo_metadata::DepKindInfo]) -> Self {
        // If any dependency kind is Normal (runtime), it's a runtime dep
        // Otherwise it's a build dep
        let has_runtime = kinds
            .iter()
            .any(|k| matches!(k.kind, cargo_metadata::DependencyKind::Normal));
        if has_runtime {
            DepKind::Runtime
        } else {
            DepKind::Build
        }
    }
}

/// Analyzes dependency graph to determine which packages are included and their kinds.
///
/// Returns a `HashMap` mapping `PackageId` → (index, `DepKind`) for packages that should
/// be included in the SBOM. Dev-only dependencies are excluded.
fn analyze_dependencies(
    metadata: &cargo_metadata::Metadata,
) -> HashMap<cargo_metadata::PackageId, (usize, DepKind)> {
    let resolve = match &metadata.resolve {
        Some(r) => r,
        None => return HashMap::new(),
    };

    let root_id = match &resolve.root {
        Some(id) => id,
        None => return HashMap::new(),
    };

    // Detect proc-macro packages
    let proc_macros: HashSet<&cargo_metadata::PackageId> = metadata
        .packages
        .iter()
        .filter(|pkg| {
            pkg.targets.len() == 1
                && pkg.targets[0].kind.len() == 1
                && pkg.targets[0].kind[0] == cargo_metadata::TargetKind::ProcMacro
        })
        .map(|pkg| &pkg.id)
        .collect();

    // Build node lookup map
    let id_to_node: HashMap<&cargo_metadata::PackageId, &cargo_metadata::Node> =
        resolve.nodes.iter().map(|n| (&n.id, n)).collect();

    // BFS traversal with kind tracking
    let mut id_to_kind: HashMap<&cargo_metadata::PackageId, DepKind> = HashMap::new();
    id_to_kind.insert(root_id, DepKind::Runtime);

    let mut queue = vec![id_to_node[root_id]];
    let mut next_queue = Vec::new();

    while !queue.is_empty() {
        for parent_node in queue.drain(..) {
            let parent_kind = id_to_kind[&parent_node.id];

            for dep in &parent_node.deps {
                let child_id = &dep.pkg;

                // Calculate child's dependency kind
                let mut child_kind = DepKind::from_cargo_kinds(&dep.dep_kinds);

                // Skip dev dependencies entirely
                let is_dev_only = dep
                    .dep_kinds
                    .iter()
                    .all(|k| matches!(k.kind, cargo_metadata::DependencyKind::Development));
                if is_dev_only {
                    continue;
                }

                // Propagate parent's kind: runtime dep of build dep → build dep
                child_kind = std::cmp::min(child_kind, parent_kind);

                // Proc-macros are always build dependencies
                if proc_macros.contains(child_id) {
                    child_kind = DepKind::Build;
                }

                // Update if new or stronger kind
                let should_visit = match id_to_kind.get(child_id) {
                    None => true,
                    Some(&existing_kind) => child_kind > existing_kind,
                };

                if should_visit {
                    id_to_kind.insert(child_id, child_kind);
                    if let Some(&child_node) = id_to_node.get(child_id) {
                        next_queue.push(child_node);
                    }
                }
            }
        }
        std::mem::swap(&mut queue, &mut next_queue);
    }

    // Convert to owned PackageId keys and add indices
    let mut result = HashMap::new();
    for (idx, (pkg_id, kind)) in id_to_kind.into_iter().enumerate() {
        result.insert(pkg_id.clone(), (idx, kind));
    }
    result
}

/// Build the main component representing the crate being built.
fn build_main_component(resolved: &ResolvedCrate, options: &BuildOptions) -> Result<Component> {
    let mut properties = vec![];

    // Add build options as properties
    if let Some(ref profile) = options.profile {
        properties.push(
            PropertyBuilder::default()
                .name("build:profile")
                .value(profile.clone())
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    if options.all_features {
        properties.push(
            PropertyBuilder::default()
                .name("build:all-features")
                .value("true")
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    if options.no_default_features {
        properties.push(
            PropertyBuilder::default()
                .name("build:no-default-features")
                .value("true")
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    if !options.features.is_empty() {
        properties.push(
            PropertyBuilder::default()
                .name("build:features")
                .value(options.features.join(","))
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    if let Some(ref target) = options.target {
        properties.push(
            PropertyBuilder::default()
                .name("build:target")
                .value(target.clone())
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    if let Some(ref toolchain) = options.toolchain {
        properties.push(
            PropertyBuilder::default()
                .name("build:toolchain")
                .value(toolchain.clone())
                .build()
                .map_err(|e| {
                    crate::error::SbomBuilderSnafu {
                        message: e.to_string(),
                    }
                    .build()
                })?,
        );
    }

    // Build package URL (purl) for the component
    let purl = format!("pkg:cargo/{}@{}", resolved.name, resolved.version);

    let mut builder = ComponentBuilder::default();
    builder
        .type_("application")
        .bom_ref(purl.clone())
        .name(resolved.name.clone())
        .version(resolved.version.to_string())
        .purl(purl);

    if !properties.is_empty() {
        builder.properties(properties);
    }

    builder.build().map_err(|e| {
        crate::error::SbomBuilderSnafu {
            message: e.to_string(),
        }
        .build()
    })
}

/// Build components for all dependencies from cargo metadata.
fn build_dependency_components(
    metadata: &cargo_metadata::Metadata,
) -> Result<(Vec<Component>, HashMap<cargo_metadata::PackageId, DepKind>)> {
    let dep_analysis = analyze_dependencies(metadata);

    // Get workspace members to exclude them from dependencies
    let workspace_member_ids: HashSet<_> = metadata.workspace_packages().iter().map(|p| &p.id).collect();

    // Collect packages that should be in SBOM
    let mut packages: Vec<(&cargo_metadata::Package, DepKind)> = metadata
        .packages
        .iter()
        .filter(|p| !workspace_member_ids.contains(&p.id))
        .filter_map(|p| dep_analysis.get(&p.id).map(|(_idx, kind)| (p, *kind)))
        .collect();

    // Sort for deterministic output
    packages.sort_by(|a, b| {
        a.0.name
            .cmp(&b.0.name)
            .then(a.0.version.cmp(&b.0.version))
            .then(a.0.id.cmp(&b.0.id))
    });

    let mut components = Vec::new();
    let mut kind_map = HashMap::new();

    for (package, dep_kind) in packages {
        let purl = format!("pkg:cargo/{}@{}", package.name, package.version);

        let mut properties = Vec::new();

        // Add dependency kind property for build dependencies
        if dep_kind == DepKind::Build {
            properties.push(
                PropertyBuilder::default()
                    .name("cdx:rustc:dependency_kind")
                    .value("build")
                    .build()
                    .map_err(|e| {
                        crate::error::SbomBuilderSnafu {
                            message: e.to_string(),
                        }
                        .build()
                    })?,
            );
        }

        let mut builder = ComponentBuilder::default();
        builder
            .type_("library")
            .bom_ref(purl.clone())
            .name(package.name.to_string())
            .version(package.version.to_string())
            .purl(purl);

        if let Some(ref desc) = package.description {
            builder.description(desc.clone());
        }

        if !properties.is_empty() {
            builder.properties(properties);
        }

        let component = builder.build().map_err(|e| {
            crate::error::SbomBuilderSnafu {
                message: e.to_string(),
            }
            .build()
        })?;

        components.push(component);
        kind_map.insert(package.id.clone(), dep_kind);
    }

    Ok((components, kind_map))
}

/// Build the dependency graph showing relationships between components.
fn build_dependency_graph(metadata: &cargo_metadata::Metadata) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::new();

    // Create a map of package ID to bom-ref
    let package_id_to_ref: HashMap<_, _> = metadata
        .packages
        .iter()
        .map(|p| {
            let purl = format!("pkg:cargo/{}@{}", p.name, p.version);
            (&p.id, purl)
        })
        .collect();

    for package in &metadata.packages {
        let purl = format!("pkg:cargo/{}@{}", package.name, package.version);

        // Collect dependencies using the resolve graph if available
        let dep_refs: Vec<String> = if let Some(ref resolve) = metadata.resolve {
            resolve
                .nodes
                .iter()
                .find(|node| node.id == package.id)
                .map(|node| {
                    node.deps
                        .iter()
                        .filter_map(|dep| package_id_to_ref.get(&dep.pkg).cloned())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut builder = DependencyBuilder::default();
        builder.ref_(purl);

        if !dep_refs.is_empty() {
            builder.depends_on(dep_refs);
        }

        let dependency = builder.build().map_err(|e| {
            crate::error::SbomBuilderSnafu {
                message: e.to_string(),
            }
            .build()
        })?;

        dependencies.push(dependency);
    }

    Ok(dependencies)
}

/// Build the metadata section with tool information.
fn build_metadata(main_component: &Component) -> Result<Metadata> {
    let tool = ToolBuilder::default()
        .name("cgx")
        .version(env!("CARGO_PKG_VERSION").to_string())
        .vendor("cgx project".to_string())
        .build()
        .map_err(|e| {
            crate::error::SbomBuilderSnafu {
                message: e.to_string(),
            }
            .build()
        })?;

    MetadataBuilder::default()
        .tools(vec![tool])
        .component(main_component.clone())
        .timestamp(chrono::Utc::now().to_rfc3339())
        .build()
        .map_err(|e| {
            crate::error::SbomBuilderSnafu {
                message: e.to_string(),
            }
            .build()
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use serde_cyclonedx::cyclonedx::v_1_4::CycloneDx;
    use snafu::ResultExt;

    use super::*;
    use crate::{
        cargo::{CargoMetadataOptions, CargoRunner},
        crate_resolver::ResolvedSource,
        testdata::CrateTestCase,
    };

    /// Get a [`CargoRunner`] for testing.
    ///
    /// Note that for the SBOM tests the actual cargo is needed, so this isn't a mock or dummy it's
    /// the real runner.
    fn test_cargo_runner() -> impl CargoRunner {
        crate::cargo::create_cargo_runner(
            crate::config::Config::default(),
            crate::messages::MessageReporter::null(),
        )
        .unwrap()
    }

    /// Generate an SBOM for a test case with the given build options.
    fn generate_sbom_for_testcase(testcase: &CrateTestCase, options: BuildOptions) -> Result<String> {
        let cargo_runner = test_cargo_runner();

        let metadata_opts = CargoMetadataOptions::from(&options);

        let metadata = cargo_runner.metadata(testcase.path(), &metadata_opts)?;

        let root_pkg = metadata.root_package().unwrap();

        let resolved = ResolvedCrate {
            name: root_pkg.name.to_string(),
            version: root_pkg.version.clone(),
            source: ResolvedSource::CratesIo,
        };

        let cyclonedx = generate_sbom(&metadata, &resolved, &options)?;
        serde_json::to_string_pretty(&cyclonedx).context(crate::error::JsonSnafu)
    }

    /// Normalize a [`CycloneDx`] SBOM by removing non-deterministic fields.
    ///
    /// Removes timestamps and serial numbers to enable deterministic comparison.
    fn normalize_sbom(mut sbom: CycloneDx) -> CycloneDx {
        if let Some(ref mut metadata) = sbom.metadata {
            metadata.timestamp = None;
        }
        sbom.serial_number = None;
        sbom
    }

    /// Read an SBOM from a file and normalize it by removing non-deterministic fields.
    fn read_and_normalize_sbom(path: &Path) -> CycloneDx {
        let json_str = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read SBOM from {}: {}", path.display(), e));
        let sbom: CycloneDx = serde_json::from_str(&json_str)
            .unwrap_or_else(|e| panic!("Failed to parse SBOM from {}: {}", path.display(), e));
        normalize_sbom(sbom)
    }

    /// Get the version of a specific component from an SBOM file.
    ///
    /// Returns [`None`] if the component is not found in the SBOM.
    pub(crate) fn get_sbom_component_version(sbom_path: &Path, component_name: &str) -> Option<String> {
        let json_str = std::fs::read_to_string(sbom_path)
            .unwrap_or_else(|e| panic!("Failed to read SBOM from {}: {}", sbom_path.display(), e));
        let sbom: CycloneDx = serde_json::from_str(&json_str)
            .unwrap_or_else(|e| panic!("Failed to parse SBOM from {}: {}", sbom_path.display(), e));

        sbom.components
            .unwrap_or_default()
            .iter()
            .find(|c| c.name.as_str() == component_name)
            .and_then(|c| c.version.clone())
    }

    /// Assert that two SBOMs are equal after normalization.
    ///
    /// Panics with a detailed JSON diff if the SBOMs differ.
    #[expect(
        dead_code,
        reason = "kept alongside assert_sboms_ne for symmetry; not currently called by any test"
    )]
    pub(crate) fn assert_sboms_eq(path1: &Path, path2: &Path) {
        let sbom1 = read_and_normalize_sbom(path1);
        let sbom2 = read_and_normalize_sbom(path2);
        assert_json_diff::assert_json_eq!(sbom1, sbom2);
    }

    /// Assert that two SBOMs are NOT equal after normalization.
    ///
    /// Panics if the SBOMs are unexpectedly equal, showing both file paths
    /// and the normalized content that matched.
    pub(crate) fn assert_sboms_ne(path1: &Path, path2: &Path) {
        let sbom1 = read_and_normalize_sbom(path1);
        let sbom2 = read_and_normalize_sbom(path2);

        let json1 = serde_json::to_value(&sbom1).unwrap();
        let json2 = serde_json::to_value(&sbom2).unwrap();

        if json1 == json2 {
            panic!(
                "SBOMs are unexpectedly equal:\n  {}\n  {}\n\nBoth normalized to:\n{}",
                path1.display(),
                path2.display(),
                serde_json::to_string_pretty(&json1).unwrap()
            );
        }
    }

    #[test]
    fn smoke_test_all_testcases() {
        for testcase in CrateTestCase::all() {
            // workspace-all-libs is a pure workspace with no root package, which doesn't
            // make sense for SBOM generation (which package would we generate an SBOM for?)
            // TODO: Modify this smoke test to just be smart enough to detect when a test case is a
            // workspace, and enumerate all crates and generate SBOMs for each one
            if testcase.name == "workspace-all-libs" || testcase.name == "workspace-multiple-bin-crates" {
                continue;
            }
            let result = generate_sbom_for_testcase(&testcase, BuildOptions::default());
            assert!(
                result.is_ok(),
                "SBOM generation failed for {}: {:?}",
                testcase.name,
                result.err()
            );
        }
    }

    /// These snapshot tests let us generate SBOMs for various test crates, and inspect them in the
    /// corresponding snapshot files see (`src/snapshots`).  By design, SBOMs contain dependencies
    /// for the specific platform being targeted, so these tests are only run on Linux.  There's no
    /// reason we couldn't run them on Windows or Mac, but that would produce different
    /// dependencies and thus different snapshots.  If you try to run these tests that use linux
    /// SBOMs on non-Linux platforms, they will fail.
    #[cfg(target_os = "linux")]
    mod snapshots {
        use super::*;

        /// Normalize SBOM JSON for snapshot testing by removing non-deterministic fields.
        fn normalize_sbom_json(json_str: &str) -> String {
            let sbom: CycloneDx = serde_json::from_str(json_str).unwrap();
            let normalized = normalize_sbom(sbom);
            serde_json::to_string_pretty(&normalized).unwrap()
        }

        #[test]
        fn snapshot_simple_bin_no_deps() {
            let tc = CrateTestCase::simple_bin_no_deps();
            let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_simple_lib_no_deps() {
            let tc = CrateTestCase::simple_lib_no_deps();
            let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_timestamp_default_features() {
            let tc = CrateTestCase::timestamp();
            let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_timestamp_no_default_features() {
            let tc = CrateTestCase::timestamp();
            let options = BuildOptions {
                no_default_features: true,
                ..Default::default()
            };
            let sbom = generate_sbom_for_testcase(&tc, options).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_timestamp_all_features() {
            let tc = CrateTestCase::timestamp();
            let options = BuildOptions {
                all_features: true,
                ..Default::default()
            };
            let sbom = generate_sbom_for_testcase(&tc, options).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_timestamp_frobnulator_only() {
            let tc = CrateTestCase::timestamp();
            let options = BuildOptions {
                features: vec!["frobnulator".to_string()],
                no_default_features: true,
                ..Default::default()
            };
            let sbom = generate_sbom_for_testcase(&tc, options).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_stale_serde() {
            let tc = CrateTestCase::stale_serde();
            let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }

        #[test]
        fn snapshot_thicc() {
            let tc = CrateTestCase::thicc();
            let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
            let normalized = normalize_sbom_json(&sbom);

            insta::with_settings!({filters => vec![
                (r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#),
                (r"@\d+\.\d+\.\d+", "@[VERSION]"),
            ]}, {
                insta::assert_snapshot!(normalized);
            });
        }
    }

    #[test]
    fn test_feature_conditional_deps_default() {
        let tc = CrateTestCase::timestamp();
        let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();
        let names: Vec<_> = components.iter().map(|c| c.name.as_str()).collect();

        assert!(names.contains(&"serde"), "Default features should include serde");
        assert!(
            !names.contains(&"chrono"),
            "Default features should not include chrono"
        );
    }

    #[test]
    fn test_feature_conditional_deps_all_features() {
        let tc = CrateTestCase::timestamp();
        let options = BuildOptions {
            all_features: true,
            ..Default::default()
        };
        let sbom = generate_sbom_for_testcase(&tc, options).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();
        let names: Vec<_> = components.iter().map(|c| c.name.as_str()).collect();

        assert!(names.contains(&"serde"), "All features should include serde");
        assert!(names.contains(&"chrono"), "All features should include chrono");
    }

    /// When building a crate that has a serde dependency, that will pull in `serde-derive`, which
    /// is a proc macro crate.  Those are, technically, not runtime dependencies because the proc
    /// macro runs at compile time.  This test compiles a project that has a serde dep and verifies
    /// that this is handled as expected
    #[test]
    fn test_proc_macro_marked_as_build_dep() {
        let tc = CrateTestCase::proc_macro_dep();
        let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();

        let serde_derive = components
            .iter()
            .find(|c| c.name.as_str() == "serde_derive")
            .unwrap();

        if let Some(ref props) = serde_derive.properties {
            let has_build_kind = props.iter().any(|p| {
                p.name.as_deref() == Some("cdx:rustc:dependency_kind") && p.value.as_deref() == Some("build")
            });
            assert!(has_build_kind, "proc-macro should be marked as build dependency");
        } else {
            panic!("proc-macro should have dependency_kind property");
        }
    }

    /// Using a crate with various OS-specific deps for each OS, verify that only the
    /// dependencies for the current platform are included in the SBOM.
    #[test]
    fn test_os_specific_deps_filtered_by_platform() {
        let tc = CrateTestCase::os_specific_deps();
        let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();
        let names: Vec<_> = components.iter().map(|c| c.name.as_str()).collect();

        // serde should always be present (universal dependency)
        assert!(
            names.contains(&"serde"),
            "serde should be present on all platforms"
        );

        #[cfg(target_os = "linux")]
        {
            // Linux-specific deps should be present
            assert!(names.contains(&"inotify"), "inotify should be present on Linux");
            assert!(names.contains(&"libc"), "libc should be present on Linux");
            assert!(names.contains(&"nix"), "nix should be present on Linux");
            assert!(names.contains(&"procfs"), "procfs should be present on Linux");

            // macOS-specific deps should NOT be present
            assert!(!names.contains(&"cocoa"), "cocoa should not be present on Linux");
            assert!(!names.contains(&"metal"), "metal should not be present on Linux");
            assert!(!names.contains(&"objc"), "objc should not be present on Linux");
            assert!(
                !names.contains(&"security-framework"),
                "security-framework should not be present on Linux"
            );
        }

        #[cfg(target_os = "macos")]
        {
            // macOS-specific deps should be present
            assert!(names.contains(&"cocoa"), "cocoa should be present on macOS");
            assert!(names.contains(&"metal"), "metal should be present on macOS");
            assert!(names.contains(&"objc"), "objc should be present on macOS");
            assert!(
                names.contains(&"security-framework"),
                "security-framework should be present on macOS"
            );

            // Linux-specific deps should NOT be present
            assert!(
                !names.contains(&"inotify"),
                "inotify should not be present on macOS"
            );
            assert!(!names.contains(&"nix"), "nix should not be present on macOS");
            assert!(
                !names.contains(&"procfs"),
                "procfs should not be present on macOS"
            );
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // On other platforms, neither Linux nor macOS specific deps should be present
            assert!(
                !names.contains(&"inotify"),
                "inotify should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"nix"),
                "nix should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"procfs"),
                "procfs should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"cocoa"),
                "cocoa should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"metal"),
                "metal should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"objc"),
                "objc should not be present on non-Linux/macOS"
            );
            assert!(
                !names.contains(&"security-framework"),
                "security-framework should not be present on non-Linux/macOS"
            );
        }
    }

    /// When present, the lockfile should pin dependency version and that should be reflected in
    /// the SBOM
    #[test]
    fn test_version_resolution_with_lockfile() {
        let tc = CrateTestCase::stale_serde();
        let sbom = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();
        let serde = components
            .iter()
            .find(|c| c.name.as_str() == "serde")
            .expect("serde should be in components");

        assert_eq!(
            serde.version.as_deref(),
            Some("1.0.5"),
            "Should use old version from lockfile"
        );
    }

    /// When the lockfile is removed, the resolver should pick a newer version of dependencies as
    /// part of getting metadata, and that should be reflected in the SBOM
    #[test]
    fn test_version_resolution_without_lockfile() {
        let tc = CrateTestCase::stale_serde();

        let lockfile = tc.path().join("Cargo.lock");
        assert!(lockfile.exists());
        std::fs::remove_file(&lockfile).unwrap();

        let sbom = generate_sbom_for_testcase(
            &tc,
            BuildOptions {
                locked: false,
                ..Default::default()
            },
        )
        .unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let components = bom.components.unwrap();
        let serde = components
            .iter()
            .find(|c| c.name.as_str() == "serde")
            .expect("serde should be in components");

        let version = serde.version.as_deref().unwrap();
        assert_ne!(version, "1.0.5", "Should resolve to newer serde without lockfile");
        assert!(version.starts_with("1.0."), "Should still be serde 1.0.x");
    }

    #[test]
    fn test_build_options_in_metadata() {
        let tc = CrateTestCase::simple_bin_no_deps();
        let options = BuildOptions {
            profile: Some("release".to_string()),
            all_features: true,
            target: Some("x86_64-unknown-linux-musl".to_string()),
            toolchain: Some("stable".to_string()),
            ..Default::default()
        };

        let sbom = generate_sbom_for_testcase(&tc, options).unwrap();
        let bom: CycloneDx = serde_json::from_str(&sbom).unwrap();

        let metadata = bom.metadata.unwrap();
        let component = metadata.component.unwrap();
        let props = component.properties.unwrap();

        assert!(
            props
                .iter()
                .any(|p| p.name.as_deref() == Some("build:profile") && p.value.as_deref() == Some("release"))
        );
        assert!(
            props.iter().any(
                |p| p.name.as_deref() == Some("build:all-features") && p.value.as_deref() == Some("true")
            )
        );
        assert!(props.iter().any(|p| p.name.as_deref() == Some("build:target")
            && p.value.as_deref() == Some("x86_64-unknown-linux-musl")));
        assert!(props.iter().any(|p| p.name.as_deref() == Some("build:toolchain")
            && p.value.as_deref() == Some("stable")));
    }

    #[test]
    fn test_lockfile_affects_sbom() {
        let tc = CrateTestCase::stale_serde();

        let sbom_with_lock = generate_sbom_for_testcase(&tc, BuildOptions::default()).unwrap();
        let path_with_lock = tc.path().join("sbom_with_lock.json");
        std::fs::write(&path_with_lock, sbom_with_lock).unwrap();

        let lockfile = tc.path().join("Cargo.lock");
        std::fs::remove_file(&lockfile).unwrap();
        let sbom_without_lock = generate_sbom_for_testcase(
            &tc,
            BuildOptions {
                locked: false,
                ..Default::default()
            },
        )
        .unwrap();
        let path_without_lock = tc.path().join("sbom_without_lock.json");
        std::fs::write(&path_without_lock, sbom_without_lock).unwrap();

        assert_sboms_ne(&path_with_lock, &path_without_lock);
    }
}
