use std::{
    borrow::Cow,
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

    /// Platform tokens to try when matching release asset filenames.
    ///
    /// Release assets are not named consistently across projects. Some use the exact Rust target
    /// triple (`x86_64-unknown-linux-gnu`), while others use shorter `{os}-{arch}` or `{arch}-{os}`
    /// forms (`linux-x86_64`, `x86_64-linux`). This returns the full triple first because it is the
    /// most specific token, then appends those common short forms for Linux, Windows, and
    /// Darwin-like Apple targets. Other targets get only their exact triple, since each of the
    /// shorter variants were based on actual observed release asset naming conventions that may
    /// not generalize to other platforms.
    pub(crate) fn release_asset_platform_aliases(&self) -> Vec<Cow<'_, str>> {
        // Keep the original, full triple first. It is the most specific so, if present, it should
        // be used.
        let mut aliases = vec![self.as_cow()];

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

    #[test]
    fn serializes_and_deserializes_as_string() {
        let target = TargetTriple::from_static("x86_64-unknown-linux-gnu").unwrap();
        let json = serde_json::to_string(&target).unwrap();
        let round_trip: TargetTriple = serde_json::from_str(&json).unwrap();

        assert_eq!(json, r#""x86_64-unknown-linux-gnu""#);
        assert_eq!(round_trip, target);
    }
}
