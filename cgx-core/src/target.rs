use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    str::FromStr,
    sync::OnceLock,
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};
use target_lexicon::{Aarch64Architecture, Architecture, Environment, OperatingSystem, Triple, Vendor};

/// Strongly typed representation of a Rust target triple string, with parsed components and
/// convenience methods.
///
/// This spares us the ugly and error-prone stringly-typed handling of targets, and gives us a
/// single place to implement logic around target-specific behavior in a form that we can more
/// easily unit test.
///
/// Having said all of that, there is sadly still a bit of stringly-typedness left, because not all
/// Rust targets are parsable as LLVM target triples.  For example, `arm64ec-pc-windows-msvc` is a
/// valid Rust target triple but not a valid LLVM triple. We also have to represent pseudo-targets
/// like Mac's `universal-apple-darwin` which isn't any kind of target triple at all. So this also
/// carries around the raw target string, and must fall back to some stringly heuristics in cases
/// where a parsed `Triple` isn't available.
#[derive(Clone, Debug)]
pub(crate) struct TargetTriple {
    raw: Cow<'static, str>,
    /// The parsed components, or `None` when `raw` is not parseable as an LLVM triple.
    triple: Option<Triple>,
}

impl TargetTriple {
    pub(crate) fn from_static(target: &'static str) -> Self {
        Self::new(Cow::Borrowed(target))
    }

    pub(crate) fn from_owned(target: String) -> Self {
        Self::new(Cow::Owned(target))
    }

    pub(crate) fn host() -> &'static Self {
        static HOST: OnceLock<TargetTriple> = OnceLock::new();
        HOST.get_or_init(|| {
            let host = Self::from_static(build_context::TARGET);
            if host.triple.is_none() {
                tracing::warn!(
                    "Host target triple '{}' is not recognized; pre-built binary discovery will only match \
                     assets that name this exact triple",
                    build_context::TARGET
                );
            }
            host
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.raw
    }

    pub(crate) fn as_cow(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.as_str())
    }

    pub(crate) fn triple(&self) -> Option<&Triple> {
        self.triple.as_ref()
    }

    pub(crate) fn architecture(&self) -> Option<Architecture> {
        self.triple().map(|triple| triple.architecture)
    }

    pub(crate) fn operating_system(&self) -> Option<OperatingSystem> {
        self.triple().map(|triple| triple.operating_system)
    }

    pub(crate) fn environment(&self) -> Option<Environment> {
        self.triple().map(|triple| triple.environment)
    }

    pub(crate) fn vendor(&self) -> Option<&Vendor> {
        self.triple().map(|triple| &triple.vendor)
    }

    /// Whether this target is a Windows target.
    ///
    /// For an opaque target that could not be parsed into a `Target`, this falls back to a
    /// substring heuristic on the raw string.
    pub(crate) fn is_windows(&self) -> bool {
        match &self.triple {
            Some(triple) => triple.operating_system == OperatingSystem::Windows,
            None => self.raw.contains("-windows"),
        }
    }

    /// Returns true if the target is macOS or another Apple platform (e.g., iOS, tvOS, watchOS).
    ///
    /// There are many different target OS values for the various Apple platforms, but for our
    /// purposes they are all "macos-like". For an opaque target this falls back to a substring
    /// heuristic, mirroring [`Self::is_windows`].
    pub(crate) fn is_macos_like(&self) -> bool {
        match &self.triple {
            Some(triple) => triple.operating_system.is_like_darwin(),
            None => self.raw.contains("-darwin"),
        }
    }

    pub(crate) fn binary_ext(&self) -> &'static str {
        if self.is_windows() { ".exe" } else { "" }
    }

    /// Platform strings to try when matching release asset filenames.
    ///
    /// Release assets are not named consistently across projects. Some use the exact Rust target
    /// triple (eg, `x86_64-unknown-linux-gnu`), while others use shorter `{os}-{arch}` or
    /// `{arch}-{os}` forms (`linux-x86_64`, `x86_64-linux`). This method returns the full
    /// triple first because it is the most specific token, then any alternate full
    /// triples, then those common short forms for whatever target this is. Targets outside of the
    /// known windows/mac/linux set get only their exact triple, since without a successfully
    /// parsed `Target` we don't know enough about the components of the target to generate any
    /// other permutations/short forms of it.
    pub(crate) fn release_asset_platform_aliases(&self) -> Vec<Cow<'static, str>> {
        // Keep the original, full triple first. It is the most specific so, if present, it should
        // be used.
        let mut aliases = vec![self.raw.clone()];

        // An opaque target has no known components to build short forms from, so its exact string
        // is the only usable token.
        let Some(triple) = &self.triple else {
            return aliases;
        };

        let push_unique = |aliases: &mut Vec<Cow<'static, str>>, alias: String| {
            if !aliases.iter().any(|existing| existing.as_ref() == alias) {
                aliases.push(Cow::Owned(alias));
            }
        };

        // `arm64` is a far more common release-asset spelling than the canonical `aarch64`, so
        // aarch64 targets match under both strings..
        let arch = triple.architecture.into_str();
        let mut arch_versions = vec![arch.clone()];
        if triple.architecture == Architecture::Aarch64(Aarch64Architecture::Aarch64) {
            arch_versions.push(Cow::Borrowed("arm64"));

            // Alternate-form full triples (eg `arm64-apple-darwin`) directly after the raw triple:
            // still fully specific, just a different arch version. The canonical version reproduces
            // the raw triple and is dropped as a duplicate.
            let arm64_triple = self.raw.replace("aarch64-", "arm64-");
            push_unique(&mut aliases, arm64_triple);
        }

        // Convert the OS into a colloquial OS name most commonly used when naming release assets.
        let os = if triple.operating_system == OperatingSystem::Windows {
            "windows"
        } else if triple.operating_system.is_like_darwin() {
            "darwin"
        } else if triple.operating_system == OperatingSystem::Linux {
            "linux"
        } else {
            // We don't know this OS so we can't generate any short aliases for it.
            return aliases;
        };

        // For a known target triple, add common short forms that encode just the OS and the
        // architecture name.
        for arch_version in &arch_versions {
            for alias in [
                format!("{}-{}", os, arch_version),
                format!("{}-{}", arch_version, os),
            ] {
                push_unique(&mut aliases, alias);
            }
        }

        aliases
    }

    /// The ABI-compatible fallback targets for this host: every target *other than* `self` whose
    /// binaries this host can also execute, most preferred first. On macOS this includes the
    /// `universal`/`universal2` fat-binary pseudo-targets.
    ///
    /// This deliberately excludes `self`; callers that want the complete list with the exact host
    /// first should use [`Self::compatible_targets`]. Returns empty when the host has no
    /// compatible siblings (eg, a musl Linux host, an OS we have no fallback rules for, or an
    /// opaque target whose components are unknown).
    ///
    /// `caps` is the runtime-probed host state, which is used on certain platforms where the
    /// targets available for fallback also depend on whether or not the host is configured to
    /// emulate other architedtures.
    fn compatible_fallback_targets_with(&self, caps: HostCapabilities) -> Vec<TargetTriple> {
        let mut fallbacks = Vec::new();

        // An opaque target has unknown components, so no ABI-compatibility rule can apply.
        let Some(triple) = &self.triple else {
            return fallbacks;
        };

        if triple.operating_system.is_like_darwin() {
            // A universal (fat) binary carries a slice for every macOS architecture, so it runs
            // natively on any Mac - Intel or Apple Silicon. These are Apple `lipo` asset naming
            // conventions, not real triples, so they are carried as opaque targets.
            fallbacks.push(Self::from_static("universal-apple-darwin"));
            fallbacks.push(Self::from_static("universal2-apple-darwin"));

            // An Apple Silicon host can also run an `x86_64-apple-darwin` binary, but only through
            // Rosetta 2 - so it is a fallback only when the probe found Rosetta.
            if matches!(triple.architecture, Architecture::Aarch64(_)) && caps.x86_64_macos_runnable {
                fallbacks.push(Self::sibling_with_architecture(triple, Architecture::X86_64));
            }
        } else if triple.operating_system == OperatingSystem::Windows {
            // Every Windows ABI is interchangeable at runtime for the same architecture (see
            // `windows_fallback_environments`), so each sibling ABI is a valid fallback.
            //
            // TODO: Windows on ARM64 can supposedly run x86_64 binaries through emulation.  If
            // Windows on ARM64 becomes a relevant target, and I can get my hands on a Windows
            // ARM64 machine to test on, add some logic here to detect that and add the x86_64
            // sibling as a fallback for ARM64 hosts.
            for &environment in Self::windows_fallback_environments(triple.environment) {
                fallbacks.push(Self::sibling_with_environment(triple, environment));
            }
        } else if triple.operating_system == OperatingSystem::Linux && triple.environment == Environment::Gnu
        {
            // A glibc host can also run a musl binary (musl release binaries are statically linked).
            // The reverse does NOT hold - a musl-only host generally cannot run a glibc-linked binary
            // - so a musl host is intentionally left with no fallback.
            fallbacks.push(Self::sibling_with_environment(triple, Environment::Musl));
        }

        fallbacks
    }

    /// List every target whose binaries this host can execute, `self` first (the exact host always
    /// wins), then its ABI-compatible fallbacks in preference order.
    ///
    /// This is the ordered list the binary providers should iterate when probing for a
    /// downloadable asset.
    pub(crate) fn compatible_targets(&self) -> Vec<TargetTriple> {
        self.compatible_targets_with(HostCapabilities::detect())
    }

    /// Host-independent core of [`Self::compatible_targets`]; see
    /// [`Self::compatible_fallback_targets_with`] for why `caps` is injected.
    fn compatible_targets_with(&self, caps: HostCapabilities) -> Vec<TargetTriple> {
        std::iter::once(self.clone())
            .chain(self.compatible_fallback_targets_with(caps))
            .collect()
    }

    /// Release-asset platform strings grouped per compatible target, in target-preference order:
    /// the exact host's group first, then each ABI-compatible fallback's group.
    pub(crate) fn compatible_asset_platform_alias_groups(
        &self,
    ) -> Vec<(TargetTriple, Vec<Cow<'static, str>>)> {
        self.compatible_asset_platform_alias_groups_with(HostCapabilities::detect())
    }

    /// Host-independent core of [`Self::compatible_asset_platform_alias_groups`]; see
    /// [`Self::compatible_fallback_targets_with`] for why `caps` is injected.
    fn compatible_asset_platform_alias_groups_with(
        &self,
        caps: HostCapabilities,
    ) -> Vec<(TargetTriple, Vec<Cow<'static, str>>)> {
        // Within a group the strings are that target's [`Self::release_asset_platform_aliases`].
        // Across groups a string is kept only in the first (most preferred) group that produces it,
        // so an ambiguous short form shared by multiple targets (eg, a gnu host and its musl fallback both
        // emit `linux-x86_64`) is attributed to the primary target.
        //
        // Basically, take all compatible targets, and for each target make the various platform
        // alias strings for that target, and return a deduped list of all of those platform alias
        // strings.

        let mut seen = HashSet::new();
        let mut groups = Vec::new();
        for candidate in self.compatible_targets_with(caps) {
            let tokens: Vec<Cow<'static, str>> = candidate
                .release_asset_platform_aliases()
                .into_iter()
                .filter(|token| seen.insert(token.clone()))
                .collect();
            if !tokens.is_empty() {
                groups.push((candidate, tokens));
            }
        }
        groups
    }

    /// The Windows ABI environments to fall back to after the host's own, in preference order.
    ///
    /// Every Windows target compiles to a standalone PE linked against the system UCRT, so a binary
    /// built for a sibling ABI runs on the host; we just prefer the closest ABI first. The host's
    /// own ABI is omitted because the sole caller wants only the *additional* targets - the host is
    /// already covered by `self`.
    fn windows_fallback_environments(host: Environment) -> &'static [Environment] {
        match host {
            Environment::Msvc => &[Environment::Gnu, Environment::GnuLlvm],
            Environment::Gnu => &[Environment::GnuLlvm, Environment::Msvc],
            Environment::GnuLlvm => &[Environment::Gnu, Environment::Msvc],
            _ => &[],
        }
    }

    /// Build a [`TargetTriple`] from an already-parsed [`Triple`], re-deriving its canonical
    /// string.
    fn from_triple(triple: Triple) -> Self {
        Self {
            raw: Cow::Owned(triple.to_string()),
            triple: Some(triple),
        }
    }

    /// The sibling target that differs from `triple` only in its environment (ABI), for example
    /// turning a `-gnu` host into its `-musl` sibling.
    fn sibling_with_environment(triple: &Triple, environment: Environment) -> TargetTriple {
        let mut sibling = triple.clone();
        sibling.environment = environment;
        Self::from_triple(sibling)
    }

    /// The sibling target that differs from `triple` only in its architecture, for example turning
    /// an `aarch64-apple-darwin` host into `x86_64-apple-darwin`.
    fn sibling_with_architecture(triple: &Triple, architecture: Architecture) -> TargetTriple {
        let mut sibling = triple.clone();
        sibling.architecture = architecture;
        Self::from_triple(sibling)
    }

    fn new(raw: Cow<'static, str>) -> Self {
        let triple = Triple::from_str(raw.as_ref()).ok();
        Self { raw, triple }
    }
}

/// Runtime-probed facts about the host that the ABI-compatibility logic needs but cannot derive
/// from the target triple alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostCapabilities {
    /// Whether an `x86_64-apple-darwin` binary runs on this host. This is native on Intel Macs and
    /// available on Apple Silicon only when Rosetta 2 is installed. It is consulted only when the
    /// host is `aarch64-apple-darwin`.
    pub(crate) x86_64_macos_runnable: bool,
}

impl HostCapabilities {
    /// The real capabilities of the current host, probed once and memoized for the process
    /// lifetime.
    fn detect() -> Self {
        static CAPABILITIES: OnceLock<HostCapabilities> = OnceLock::new();
        *CAPABILITIES.get_or_init(|| {
            let host = TargetTriple::host();
            let x86_64_macos_runnable = host.is_macos_like()
                && matches!(host.architecture(), Some(Architecture::Aarch64(_)))
                && Self::probe_x86_64_macos_runnable();
            HostCapabilities {
                x86_64_macos_runnable,
            }
        })
    }

    /// Probe whether an `x86_64` macOS binary can execute on this host by running a universal
    /// system binary forced through its `x86_64` slice. Success means the `x86_64` slice ran,
    /// which on Apple Silicon requires Rosetta 2.
    #[cfg(target_os = "macos")]
    fn probe_x86_64_macos_runnable() -> bool {
        std::process::Command::new("arch")
            .args(["-arch", "x86_64", "/usr/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// On non-macOS hosts an `x86_64-apple-darwin` binary can never run, and the field is never
    /// consulted anyway.
    #[cfg(not(target_os = "macos"))]
    fn probe_x86_64_macos_runnable() -> bool {
        false
    }
}

impl AsRef<str> for TargetTriple {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for TargetTriple {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl Eq for TargetTriple {}

impl Hash for TargetTriple {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

// serde for the target triple is literally just preserving the string representation that it was
// created from.

impl Serialize for TargetTriple {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TargetTriple {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TargetTripleVisitor;

        impl Visitor<'_> for TargetTripleVisitor {
            type Value = TargetTriple;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Rust target triple string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(TargetTriple::from_owned(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(TargetTriple::from_owned(value))
            }
        }

        deserializer.deserialize_string(TargetTripleVisitor)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use target_lexicon::{Architecture, Environment, OperatingSystem, Vendor};

    use super::*;

    #[test]
    fn parses_valid_target() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(target.as_str(), "x86_64-unknown-linux-gnu");
        assert!(target.triple().is_some());
    }

    /// A string target-lexicon cannot parse is carried opaquely: the raw string is preserved
    /// verbatim, component accessors report nothing, and the target participates in discovery with
    /// only its exact token and no ABI-compatible fallbacks.
    #[test]
    fn unparsable_target_is_carried_opaquely() {
        // A real rustc target that target-lexicon cannot parse (unknown architecture).
        let target = TargetTriple::from_static("arm64ec-pc-windows-msvc");

        assert_eq!(target.as_str(), "arm64ec-pc-windows-msvc");
        assert!(target.triple().is_none());
        assert_eq!(target.architecture(), None);
        assert_eq!(target.operating_system(), None);
        assert_eq!(target.environment(), None);
        assert_eq!(target.vendor(), None);
        assert_eq!(
            target.release_asset_platform_aliases(),
            vec![Cow::Borrowed("arm64ec-pc-windows-msvc")]
        );
        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
        // The `-windows` heuristic still classifies it for cosmetic purposes.
        assert!(target.is_windows());
        assert_eq!(target.binary_ext(), ".exe");
    }

    #[test]
    fn host_target_parses() {
        assert_eq!(TargetTriple::host().as_str(), build_context::TARGET);
    }

    #[test]
    fn exposes_original_string_forms() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(target.to_string(), "x86_64-unknown-linux-gnu");
        assert_eq!(target.as_str(), "x86_64-unknown-linux-gnu");
        assert_matches!(target.as_cow(), Cow::Borrowed("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn exposes_typed_components() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(target.triple().unwrap().architecture, Architecture::X86_64);
        assert_eq!(target.architecture(), Some(Architecture::X86_64));
        assert_eq!(target.operating_system(), Some(OperatingSystem::Linux));
        assert_eq!(target.environment(), Some(Environment::Gnu));
        assert_eq!(target.vendor(), Some(&Vendor::Unknown));
    }

    #[test]
    fn windows_binary_extension() {
        let windows = TargetTriple::from_static("x86_64-pc-windows-msvc");
        let linux = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(windows.binary_ext(), ".exe");
        assert_eq!(linux.binary_ext(), "");
    }

    #[test]
    fn linux_release_asset_aliases() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(
            target.release_asset_platform_aliases(),
            vec![
                Cow::Borrowed("x86_64-unknown-linux-gnu"),
                Cow::Owned("linux-x86_64".to_string()),
                Cow::Owned("x86_64-linux".to_string()),
            ]
        );
    }

    /// macOS triples use `apple`, but most asset names use `darwin`; aarch64 targets additionally
    /// match under the more common `arm64` spelling, full-triple and short forms alike.
    #[test]
    fn macos_release_asset_aliases_use_darwin_and_arm64() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");

        assert_eq!(
            target.release_asset_platform_aliases(),
            cows(&[
                "aarch64-apple-darwin",
                "arm64-apple-darwin",
                "darwin-aarch64",
                "aarch64-darwin",
                "darwin-arm64",
                "arm64-darwin",
            ])
        );
    }

    #[test]
    fn windows_release_asset_aliases() {
        let target = TargetTriple::from_static("x86_64-pc-windows-msvc");

        assert_eq!(
            target.release_asset_platform_aliases(),
            vec![
                Cow::Borrowed("x86_64-pc-windows-msvc"),
                Cow::Owned("windows-x86_64".to_string()),
                Cow::Owned("x86_64-windows".to_string()),
            ]
        );
    }

    fn caps(x86_64_macos_runnable: bool) -> HostCapabilities {
        HostCapabilities {
            x86_64_macos_runnable,
        }
    }

    fn cows(items: &[&'static str]) -> Vec<Cow<'static, str>> {
        items.iter().copied().map(Cow::Borrowed).collect()
    }

    fn targets(items: &[&'static str]) -> Vec<TargetTriple> {
        items.iter().map(|item| TargetTriple::from_static(item)).collect()
    }

    /// Flatten the grouped alias tokens into one preference-ordered list.
    fn flat_aliases(target: &TargetTriple, caps: HostCapabilities) -> Vec<Cow<'static, str>> {
        target
            .compatible_asset_platform_alias_groups_with(caps)
            .into_iter()
            .flat_map(|(_, tokens)| tokens)
            .collect()
    }

    /// The full "targets to try" list puts the exact host first, then its fallbacks. Uses a Linux
    /// host so the result does not depend on the machine running the test - only the
    /// `aarch64-apple-darwin` arm consults runtime-probed capabilities.
    #[test]
    fn compatible_targets_list_host_first_then_fallbacks() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(
            target.compatible_targets(),
            targets(&["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"])
        );
    }

    #[test]
    fn linux_gnu_falls_back_to_musl() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-unknown-linux-musl"])
        );
    }

    #[test]
    fn linux_aarch64_gnu_falls_back_to_musl() {
        let target = TargetTriple::from_static("aarch64-unknown-linux-gnu");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["aarch64-unknown-linux-musl"])
        );
    }

    /// A musl host must NOT be offered a gnu fallback: a musl-only host generally cannot run a
    /// glibc-linked binary. This asymmetry is load-bearing.
    #[test]
    fn linux_musl_has_no_fallback() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-musl");

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
    }

    #[test]
    fn windows_msvc_falls_back_to_gnu_then_gnullvm() {
        let target = TargetTriple::from_static("x86_64-pc-windows-msvc");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-pc-windows-gnu", "x86_64-pc-windows-gnullvm"])
        );
    }

    #[test]
    fn windows_gnu_falls_back_to_gnullvm_then_msvc() {
        let target = TargetTriple::from_static("x86_64-pc-windows-gnu");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-pc-windows-gnullvm", "x86_64-pc-windows-msvc"])
        );
    }

    /// Without Rosetta an Apple Silicon host cannot run x86_64 binaries, but universal fat
    /// binaries always run natively, so they remain the only fallbacks.
    #[test]
    fn apple_silicon_without_rosetta_falls_back_to_universal_only() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["universal-apple-darwin", "universal2-apple-darwin"])
        );
    }

    #[test]
    fn apple_silicon_with_rosetta_falls_back_to_universal_then_x86_64() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");

        assert_eq!(
            target.compatible_fallback_targets_with(caps(true)),
            targets(&[
                "universal-apple-darwin",
                "universal2-apple-darwin",
                "x86_64-apple-darwin"
            ])
        );
    }

    /// An Intel Mac host is already `x86_64-apple-darwin`, so the capability bit is irrelevant and
    /// no cross-architecture sibling is added regardless of its value; only the universal fat
    /// binaries are compatible fallbacks.
    #[test]
    fn intel_mac_falls_back_to_universal_regardless_of_capability_bit() {
        let target = TargetTriple::from_static("x86_64-apple-darwin");

        let expected = targets(&["universal-apple-darwin", "universal2-apple-darwin"]);
        assert_eq!(target.compatible_fallback_targets_with(caps(false)), expected);
        assert_eq!(target.compatible_fallback_targets_with(caps(true)), expected);
    }

    /// An OS with no known ABI-compatible siblings gets no fallbacks - only its exact host triple,
    /// which `compatible_fallback_targets` excludes.
    #[test]
    fn unhandled_os_has_no_fallback() {
        let target = TargetTriple::from_static("x86_64-unknown-freebsd");

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
    }

    /// The musl sibling's short `{os}-{arch}` forms coincide with the gnu host's, so they are
    /// attributed to the host group; the musl group keeps only its full triple.
    #[test]
    fn linux_gnu_alias_groups_attribute_shared_tokens_to_host() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");

        let groups = target.compatible_asset_platform_alias_groups_with(caps(false));

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, target);
        assert_eq!(
            groups[0].1,
            cows(&["x86_64-unknown-linux-gnu", "linux-x86_64", "x86_64-linux"])
        );
        assert_eq!(
            groups[1].0,
            TargetTriple::from_static("x86_64-unknown-linux-musl")
        );
        assert_eq!(groups[1].1, cows(&["x86_64-unknown-linux-musl"]));
    }

    /// The Rosetta cross-architecture sibling contributes its own short forms (`darwin-x86_64` /
    /// `x86_64-darwin`), while the universal pseudo-targets contribute only themselves.
    #[test]
    fn apple_silicon_with_rosetta_asset_aliases_include_x86_64_short_forms() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");

        assert_eq!(
            flat_aliases(&target, caps(true)),
            cows(&[
                "aarch64-apple-darwin",
                "arm64-apple-darwin",
                "darwin-aarch64",
                "aarch64-darwin",
                "darwin-arm64",
                "arm64-darwin",
                "universal-apple-darwin",
                "universal2-apple-darwin",
                "x86_64-apple-darwin",
                "darwin-x86_64",
                "x86_64-darwin",
            ])
        );
    }

    /// On Apple Silicon with Rosetta, the common `arm64` asset spelling must be generated, and
    /// every native (aarch64/arm64) token must precede every emulated x86_64 token - otherwise a
    /// release shipping `foo-arm64-darwin.tar.gz` plus `foo-x86_64-darwin.tar.gz` matches only the
    /// Intel asset and cgx silently runs it under Rosetta instead of natively.
    #[test]
    fn apple_silicon_arm64_spellings_present_and_precede_x86_64() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");
        let aliases = flat_aliases(&target, caps(true));

        for expected in ["darwin-arm64", "arm64-darwin", "arm64-apple-darwin"] {
            assert!(
                aliases.iter().any(|alias| alias.as_ref() == expected),
                "expected {expected:?} among aliases {aliases:?}"
            );
        }

        let last_native = aliases
            .iter()
            .rposition(|alias| alias.contains("aarch64") || alias.contains("arm64"))
            .expect("expected native-architecture aliases");
        let first_x86_64 = aliases
            .iter()
            .position(|alias| alias.contains("x86_64"))
            .expect("expected x86_64 fallback aliases under Rosetta");
        assert!(
            last_native < first_x86_64,
            "all native tokens must precede all x86_64 tokens: {aliases:?}"
        );
    }

    #[test]
    fn apple_silicon_without_rosetta_asset_aliases_omit_x86_64_short_forms() {
        let target = TargetTriple::from_static("aarch64-apple-darwin");

        assert_eq!(
            flat_aliases(&target, caps(false)),
            cows(&[
                "aarch64-apple-darwin",
                "arm64-apple-darwin",
                "darwin-aarch64",
                "aarch64-darwin",
                "darwin-arm64",
                "arm64-darwin",
                "universal-apple-darwin",
                "universal2-apple-darwin",
            ])
        );
    }

    /// The universal pseudo-targets are asset-name tokens, not real triples: they are carried as
    /// opaque [`TargetTriple`]s whose only asset token is their exact name, and they never gain
    /// fallbacks of their own even though the `-darwin` heuristic classifies them as macOS-like.
    #[test]
    fn universal_pseudo_targets_are_opaque() {
        for pseudo in ["universal-apple-darwin", "universal2-apple-darwin"] {
            let target = TargetTriple::from_owned(pseudo.to_string());

            assert!(target.triple().is_none(), "{pseudo} must not parse as a triple");
            assert!(target.is_macos_like());
            assert_eq!(target.release_asset_platform_aliases(), cows(&[pseudo]));
            assert_eq!(target.compatible_fallback_targets_with(caps(true)), targets(&[]));
        }
    }

    #[test]
    fn serializes_and_deserializes_as_string() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu");
        let json = serde_json::to_string(&target).unwrap();
        let round_trip: TargetTriple = serde_json::from_str(&json).unwrap();

        assert_eq!(json, r#""x86_64-unknown-linux-gnu""#);
        assert_eq!(round_trip, target);
    }
}
