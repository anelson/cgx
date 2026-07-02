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
use target_lexicon::{Architecture, Environment, OperatingSystem, Triple, Vendor};

use crate::{Result, error};

/// Strongly typed representation of a Rust target triple string, with parsed components and
/// convenience methods.
///
/// This spares us the ugly and error-prone stringly-typed handling of targets, and gives us a
/// single place to implement logic around target-specific behavior in a form that we can more
/// easily unit test.
#[derive(Clone, Debug)]
pub(crate) struct TargetTriple {
    raw: Cow<'static, str>,
    triple: Triple,
}

impl TargetTriple {
    pub(crate) fn from_static(target: &'static str) -> Result<Self> {
        Self::parse(Cow::Borrowed(target))
    }

    pub(crate) fn from_owned(target: String) -> Result<Self> {
        Self::parse(Cow::Owned(target))
    }

    pub(crate) fn host() -> &'static Self {
        static HOST: OnceLock<TargetTriple> = OnceLock::new();
        HOST.get_or_init(|| {
            Self::from_static(build_context::TARGET)
                .expect("build_context::TARGET must be a valid target triple")
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.raw
    }

    pub(crate) fn as_cow(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.as_str())
    }

    pub(crate) fn triple(&self) -> &Triple {
        &self.triple
    }

    pub(crate) fn architecture(&self) -> Architecture {
        self.triple().architecture
    }

    pub(crate) fn operating_system(&self) -> OperatingSystem {
        self.triple().operating_system
    }

    pub(crate) fn environment(&self) -> Environment {
        self.triple().environment
    }

    pub(crate) fn vendor(&self) -> &Vendor {
        &self.triple().vendor
    }

    pub(crate) fn is_windows(&self) -> bool {
        self.operating_system() == OperatingSystem::Windows
    }

    /// Returns true if the target is macOS or another Apple platform (e.g., iOS, tvOS, watchOS).
    ///
    /// There are many different target OS values for the various Apple platforms, but for our
    /// purposes they are all "macos-like".
    pub(crate) fn is_macos_like(&self) -> bool {
        self.operating_system().is_like_darwin()
    }

    pub(crate) fn binary_ext(&self) -> &'static str {
        if self.is_windows() { ".exe" } else { "" }
    }

    /// Platform strings to try when matching release asset filenames.
    ///
    /// Release assets are not named consistently across projects. Some use the exact Rust target
    /// triple (eg, `x86_64-unknown-linux-gnu`), while others use shorter `{os}-{arch}` or
    /// `{arch}-{os}` forms (`linux-x86_64`, `x86_64-linux`). This method returns the full
    /// triple first because it is the most specific token, then appends those common short
    /// forms for whatever target this is. Targets outside of the known windows/mac/linux set
    /// get only their exact triple, since each of the shorter variants were based on actual
    /// observed release asset naming conventions that may not generalize to more exotic
    /// targets.
    pub(crate) fn release_asset_platform_aliases(&self) -> Vec<Cow<'static, str>> {
        // Keep the original, full triple first. It is the most specific so, if present, it should
        // be used.
        let mut aliases = vec![self.raw.clone()];

        // Convert the OS into a colloquial OS name most commonly used when naming release assets.
        let os = if self.is_windows() {
            "windows"
        } else if self.is_macos_like() {
            "darwin"
        } else if self.operating_system() == OperatingSystem::Linux {
            "linux"
        } else {
            // We don't know this OS so we can't generate any aliases for it. Just return the full triple.
            return aliases;
        };

        let arch = self.architecture().into_str();

        // Short aliases can collide with the full triple for unusual targets, so preserve priority
        // order while dropping duplicates.
        for alias in [format!("{}-{}", os, arch), format!("{}-{}", arch, os)] {
            if !aliases.iter().any(|existing| existing.as_ref() == alias) {
                aliases.push(Cow::Owned(alias));
            }
        }

        aliases
    }

    /// The ABI-compatible fallback targets for this host: every real target *other than* `self`
    /// whose binaries this host can also execute, most preferred first.
    ///
    /// This deliberately excludes `self`; callers that want the complete "targets to try" list with
    /// the exact host first should use [`Self::compatible_targets`]. Returns empty when the host
    /// has no compatible siblings (eg, a musl Linux host, an Apple Silicon host without Rosetta, or
    /// an OS we have no fallback rules for).
    pub(crate) fn compatible_fallback_targets(&self) -> Vec<TargetTriple> {
        self.compatible_fallback_targets_with(HostCapabilities::detect())
    }

    /// Host-independent core of [`Self::compatible_fallback_targets`].
    ///
    /// `caps` is the runtime-probed host state, which is used on certain platforms where the
    /// targets available for fallback also depend on whether or not the host is configured to
    /// emulate other architedtures.
    fn compatible_fallback_targets_with(&self, caps: HostCapabilities) -> Vec<TargetTriple> {
        let mut fallbacks = Vec::new();

        if self.is_macos_like() {
            // An Apple Silicon host can also run an `x86_64-apple-darwin` binary, but only through
            // Rosetta 2 — so it is a fallback only when the probe found Rosetta.
            if matches!(self.architecture(), Architecture::Aarch64(_)) && caps.x86_64_macos_runnable {
                fallbacks.push(self.with_architecture(Architecture::X86_64));
            }
        } else if self.operating_system() == OperatingSystem::Windows {
            // Every Windows ABI is interchangeable at runtime for the same architecture (see
            // `windows_fallback_environments`), so each sibling ABI is a valid fallback.
            //
            // TODO: Windows on ARM64 can supposedly run x86_64 binaries through emulation.  If
            // Windows on ARM64 becomes a relevant target, and I can get my hands on a Windows
            // ARM64 machine to test on, add some logic here to detect that and add the x86_64
            // sibling as a fallback for ARM64 hosts.
            for &environment in Self::windows_fallback_environments(self.environment()) {
                fallbacks.push(self.with_environment(environment));
            }
        } else if self.operating_system() == OperatingSystem::Linux && self.environment() == Environment::Gnu
        {
            // A glibc host can also run a musl binary (musl release binaries are statically linked).
            // The reverse does NOT hold — a musl-only host generally cannot run a glibc-linked binary
            // — so a musl host is intentionally left with no fallback.
            fallbacks.push(self.with_environment(Environment::Musl));
        }

        fallbacks
    }

    /// Every real target whose binaries this host can execute, `self` first (the exact host always
    /// wins), then [`Self::compatible_fallback_targets`]. This is the ordered list the binary
    /// providers should iterate when probing for a downloadable asset.
    pub(crate) fn compatible_targets(&self) -> Vec<TargetTriple> {
        std::iter::once(self.clone())
            .chain(self.compatible_fallback_targets())
            .collect()
    }

    /// Release-asset platform strings across the host and all its compatible fallbacks, most
    /// preferred first. This is what the GitHub and GitLab providers match candidate asset
    /// filenames against.
    pub(crate) fn compatible_asset_platform_aliases(&self) -> Vec<Cow<'static, str>> {
        self.compatible_asset_platform_aliases_with(HostCapabilities::detect())
    }

    /// Host-independent core of [`Self::compatible_asset_platform_aliases`]; see
    /// [`Self::compatible_fallback_targets_with`] for why `caps` is injected.
    fn compatible_asset_platform_aliases_with(&self, caps: HostCapabilities) -> Vec<Cow<'static, str>> {
        // Build the token list in strict priority order, then drop duplicates. Priority is:
        //   1. the exact host's asset tokens (full triple + `{os}-{arch}` / `{arch}-{os}` forms),
        //   2. the host's native fat-binary pseudo-targets (`universal*`) — these run natively, so they
        //      must outrank any cross-architecture fallback yet never the exact host, then (Mac-only)
        //   3. each ABI-compatible fallback target's asset tokens, in fallback-preference order.
        let mut aliases: Vec<Cow<'static, str>> = Vec::new();
        aliases.extend(self.release_asset_platform_aliases());
        aliases.extend(
            self.pseudo_target_asset_names()
                .iter()
                .copied()
                .map(Cow::Borrowed),
        );
        for fallback in self.compatible_fallback_targets_with(caps) {
            aliases.extend(fallback.release_asset_platform_aliases());
        }

        // Same-arch siblings share their short forms — a gnu host and its musl fallback both emit
        // `linux-x86_64` — so the concatenation contains duplicates. Keep the first occurrence of
        // each token, which preserves the priority order established above.
        let mut seen = HashSet::new();
        aliases.retain(|alias| seen.insert(alias.clone()));
        aliases
    }

    /// Asset-name platform strings for this OS's native fat-binary pseudo-targets, or an empty
    /// slice when it has none. On macOS a `universal`/`universal2` archive carries a slice for
    /// every architecture and so always runs natively; these tokens let providers match such an
    /// asset. They are asset names only, never real target triples (they do not parse as
    /// [`Triple`]s — their "arch" is an Apple `lipo` fat-binary naming convention, not a real
    /// architecture).
    fn pseudo_target_asset_names(&self) -> &'static [&'static str] {
        if self.is_macos_like() {
            &["universal-apple-darwin", "universal2-apple-darwin"]
        } else {
            &[]
        }
    }

    /// The Windows ABI environments to fall back to after the host's own, in preference order.
    ///
    /// Every Windows target compiles to a standalone PE linked against the system UCRT, so a binary
    /// built for a sibling ABI runs on the host; we just prefer the closest ABI first. The host's
    /// own ABI is omitted because the sole caller wants only the *additional* targets — the host is
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
            triple,
        }
    }

    /// The sibling target that differs from this one only in its environment (ABI), for example
    /// turning a `-gnu` host into its `-musl` sibling.
    fn with_environment(&self, environment: Environment) -> TargetTriple {
        let mut triple = self.triple.clone();
        triple.environment = environment;
        Self::from_triple(triple)
    }

    /// The sibling target that differs from this one only in its architecture, for example turning
    /// an `aarch64-apple-darwin` host into `x86_64-apple-darwin`.
    fn with_architecture(&self, architecture: Architecture) -> TargetTriple {
        let mut triple = self.triple.clone();
        triple.architecture = architecture;
        Self::from_triple(triple)
    }

    fn parse(raw: Cow<'static, str>) -> Result<Self> {
        let triple = Triple::from_str(raw.as_ref()).map_err(|source| {
            error::InvalidTargetTripleSnafu {
                target: raw.to_string(),
                message: source.to_string(),
            }
            .build()
        })?;
        Ok(Self { raw, triple })
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
                && matches!(host.architecture(), Architecture::Aarch64(_))
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
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TargetTriple {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TargetTripleVisitor;

        impl Visitor<'_> for TargetTripleVisitor {
            type Value = TargetTriple;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Rust target triple string")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                TargetTriple::from_owned(value.to_string()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                TargetTriple::from_owned(value).map_err(E::custom)
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
    use crate::error::Error;

    #[test]
    fn parses_valid_target() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(target.as_str(), "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn invalid_parse_reports_invalid_target_triple() {
        assert_matches!(
            TargetTriple::from_static("not-a-real-target"),
            Err(Error::InvalidTargetTriple { .. })
        );
    }

    #[test]
    fn host_target_parses() {
        assert_eq!(TargetTriple::host().as_str(), build_context::TARGET);
    }

    #[test]
    fn exposes_original_string_forms() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(target.to_string(), "x86_64-unknown-linux-gnu");
        assert_eq!(target.as_str(), "x86_64-unknown-linux-gnu");
        assert_matches!(target.as_cow(), Cow::Borrowed("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn exposes_typed_components() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(target.triple().architecture, Architecture::X86_64);
        assert_eq!(target.architecture(), Architecture::X86_64);
        assert_eq!(target.operating_system(), OperatingSystem::Linux);
        assert_eq!(target.environment(), Environment::Gnu);
        assert_eq!(target.vendor(), &Vendor::Unknown);
    }

    #[test]
    fn windows_binary_extension() {
        let windows = TargetTriple::from_static("x86_64-pc-windows-msvc").unwrap();
        let linux = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(windows.binary_ext(), ".exe");
        assert_eq!(linux.binary_ext(), "");
    }

    #[test]
    fn linux_release_asset_aliases() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(
            target.release_asset_platform_aliases(),
            vec![
                Cow::Borrowed("x86_64-unknown-linux-gnu"),
                Cow::Owned("linux-x86_64".to_string()),
                Cow::Owned("x86_64-linux".to_string()),
            ]
        );
    }

    #[test]
    fn macos_release_asset_aliases_use_darwin() {
        let target = TargetTriple::from_static("aarch64-apple-darwin").unwrap();

        assert_eq!(
            target.release_asset_platform_aliases(),
            vec![
                Cow::Borrowed("aarch64-apple-darwin"),
                Cow::Owned("darwin-aarch64".to_string()),
                Cow::Owned("aarch64-darwin".to_string()),
            ]
        );
    }

    #[test]
    fn windows_release_asset_aliases() {
        let target = TargetTriple::from_static("x86_64-pc-windows-msvc").unwrap();

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
        items
            .iter()
            .map(|item| TargetTriple::from_static(item).unwrap())
            .collect()
    }

    /// The full "targets to try" list puts the exact host first, then its fallbacks. Uses a Linux
    /// host so the result does not depend on the machine running the test — only the
    /// `aarch64-apple-darwin` arm consults runtime-probed capabilities.
    #[test]
    fn compatible_targets_list_host_first_then_fallbacks() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(
            target.compatible_targets(),
            targets(&["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"])
        );
    }

    #[test]
    fn linux_gnu_falls_back_to_musl() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-unknown-linux-musl"])
        );
    }

    #[test]
    fn linux_aarch64_gnu_falls_back_to_musl() {
        let target = TargetTriple::from_static("aarch64-unknown-linux-gnu").unwrap();

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["aarch64-unknown-linux-musl"])
        );
    }

    /// A musl host must NOT be offered a gnu fallback: a musl-only host generally cannot run a
    /// glibc-linked binary. This asymmetry is load-bearing.
    #[test]
    fn linux_musl_has_no_fallback() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-musl").unwrap();

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
    }

    #[test]
    fn windows_msvc_falls_back_to_gnu_then_gnullvm() {
        let target = TargetTriple::from_static("x86_64-pc-windows-msvc").unwrap();

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-pc-windows-gnu", "x86_64-pc-windows-gnullvm"])
        );
    }

    #[test]
    fn windows_gnu_falls_back_to_gnullvm_then_msvc() {
        let target = TargetTriple::from_static("x86_64-pc-windows-gnu").unwrap();

        assert_eq!(
            target.compatible_fallback_targets_with(caps(false)),
            targets(&["x86_64-pc-windows-gnullvm", "x86_64-pc-windows-msvc"])
        );
    }

    #[test]
    fn apple_silicon_without_rosetta_has_no_fallback() {
        let target = TargetTriple::from_static("aarch64-apple-darwin").unwrap();

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
    }

    #[test]
    fn apple_silicon_with_rosetta_falls_back_to_x86_64() {
        let target = TargetTriple::from_static("aarch64-apple-darwin").unwrap();

        assert_eq!(
            target.compatible_fallback_targets_with(caps(true)),
            targets(&["x86_64-apple-darwin"])
        );
    }

    /// An Intel Mac host is already `x86_64-apple-darwin`, so the capability bit is irrelevant and
    /// no cross-architecture sibling is added regardless of its value.
    #[test]
    fn intel_mac_has_no_fallback_regardless_of_capability_bit() {
        let target = TargetTriple::from_static("x86_64-apple-darwin").unwrap();

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
        assert_eq!(target.compatible_fallback_targets_with(caps(true)), targets(&[]));
    }

    /// An OS with no known ABI-compatible siblings gets no fallbacks — only its exact host triple,
    /// which `compatible_fallback_targets` excludes.
    #[test]
    fn unhandled_os_has_no_fallback() {
        let target = TargetTriple::from_static("x86_64-unknown-freebsd").unwrap();

        assert_eq!(target.compatible_fallback_targets_with(caps(false)), targets(&[]));
    }

    /// The musl sibling's short `{os}-{arch}` forms coincide with the gnu host's, so they dedup
    /// away; only the musl full triple is added.
    #[test]
    fn linux_gnu_asset_aliases_add_only_musl_full_triple() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(
            target.compatible_asset_platform_aliases_with(caps(false)),
            cows(&[
                "x86_64-unknown-linux-gnu",
                "linux-x86_64",
                "x86_64-linux",
                "x86_64-unknown-linux-musl",
            ])
        );
    }

    /// The Rosetta cross-architecture sibling contributes its own short forms (`darwin-x86_64` /
    /// `x86_64-darwin`), while the universal pseudo-targets contribute only themselves.
    #[test]
    fn apple_silicon_with_rosetta_asset_aliases_include_x86_64_short_forms() {
        let target = TargetTriple::from_static("aarch64-apple-darwin").unwrap();

        assert_eq!(
            target.compatible_asset_platform_aliases_with(caps(true)),
            cows(&[
                "aarch64-apple-darwin",
                "darwin-aarch64",
                "aarch64-darwin",
                "universal-apple-darwin",
                "universal2-apple-darwin",
                "x86_64-apple-darwin",
                "darwin-x86_64",
                "x86_64-darwin",
            ])
        );
    }

    #[test]
    fn apple_silicon_without_rosetta_asset_aliases_omit_x86_64_short_forms() {
        let target = TargetTriple::from_static("aarch64-apple-darwin").unwrap();

        assert_eq!(
            target.compatible_asset_platform_aliases_with(caps(false)),
            cows(&[
                "aarch64-apple-darwin",
                "darwin-aarch64",
                "aarch64-darwin",
                "universal-apple-darwin",
                "universal2-apple-darwin",
            ])
        );
    }

    /// The universal pseudo-targets are asset-name tokens, not real triples, so they must not parse
    /// back into a [`TargetTriple`].
    #[test]
    fn universal_pseudo_targets_do_not_parse() {
        assert_matches!(
            TargetTriple::from_owned("universal-apple-darwin".to_string()),
            Err(Error::InvalidTargetTriple { .. })
        );
        assert_matches!(
            TargetTriple::from_owned("universal2-apple-darwin".to_string()),
            Err(Error::InvalidTargetTriple { .. })
        );
    }

    #[test]
    fn serializes_and_deserializes_as_string() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();
        let json = serde_json::to_string(&target).unwrap();
        let round_trip: TargetTriple = serde_json::from_str(&json).unwrap();

        assert_eq!(json, r#""x86_64-unknown-linux-gnu""#);
        assert_eq!(round_trip, target);
    }
}
