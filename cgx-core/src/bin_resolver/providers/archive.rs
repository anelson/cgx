#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use snafu::ResultExt;
use xz2::read::XzDecoder;

use crate::{Result, error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::bin_resolver) enum ArchiveFormat {
    Tar,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Zip,
    /// A single executable compressed with gzip (no tar wrapper).
    Gz,
    /// A single executable compressed with xz (no tar wrapper).
    Xz,
    /// A single executable compressed with zstd (no tar wrapper).
    Zst,
    /// A single executable compressed with bzip2 (no tar wrapper).
    Bz2,

    /// The release artifact itself is just the naked binary, with no compression.
    NakedBinary,
}

impl ArchiveFormat {
    /// Canonical filename for writing downloads to disk.
    pub(in crate::bin_resolver) fn canonical_filename(&self) -> &'static str {
        match self {
            Self::Tar => "archive.tar",
            Self::TarGz => "archive.tar.gz",
            Self::TarXz => "archive.tar.xz",
            Self::TarZst => "archive.tar.zst",
            Self::TarBz2 => "archive.tar.bz2",
            Self::Zip => "archive.zip",
            Self::Gz => "archive.gz",
            Self::Xz => "archive.xz",
            Self::Zst => "archive.zst",
            Self::Bz2 => "archive.bz2",
            Self::NakedBinary => {
                if cfg!(windows) {
                    "archive.exe"
                } else {
                    "archive"
                }
            }
        }
    }

    /// All (format, suffix) pairs used for candidate filename generation.
    pub(in crate::bin_resolver) fn all_formats() -> &'static [(ArchiveFormat, &'static str)] {
        #[cfg(windows)]
        {
            &[
                (Self::Tar, ".tar"),
                (Self::TarGz, ".tar.gz"),
                (Self::TarGz, ".tgz"),
                (Self::TarXz, ".tar.xz"),
                (Self::TarZst, ".tar.zst"),
                (Self::TarBz2, ".tar.bz2"),
                (Self::Zip, ".zip"),
                (Self::Gz, ".gz"),
                (Self::Xz, ".xz"),
                (Self::Zst, ".zst"),
                (Self::Bz2, ".bz2"),
                (Self::NakedBinary, ".exe"),
            ]
        }
        #[cfg(not(windows))]
        {
            &[
                (Self::Tar, ".tar"),
                (Self::TarGz, ".tar.gz"),
                (Self::TarGz, ".tgz"),
                (Self::TarXz, ".tar.xz"),
                (Self::TarZst, ".tar.zst"),
                (Self::TarBz2, ".tar.bz2"),
                (Self::Zip, ".zip"),
                (Self::Gz, ".gz"),
                (Self::Xz, ".xz"),
                (Self::Zst, ".zst"),
                (Self::Bz2, ".bz2"),
                (Self::NakedBinary, ""),
            ]
        }
    }
}

/// Extract a binary from an artifact by searching for one of several expected executable names.
///
/// The caller specifies the [`ArchiveFormat`] explicitly; there is no detection or fallback.
/// Archive formats are unpacked into `dest_dir`, then searched in bounded, deterministic locations.
/// Naked binary and naked-compressed formats are written directly as the first candidate name.
pub(in crate::bin_resolver) fn extract_binary_by_candidate_names(
    artifact_path: &Path,
    format: ArchiveFormat,
    candidate_names: &[impl AsRef<str>],
    dest_dir: &Path,
) -> Result<PathBuf> {
    let lookup = BinaryLookup::by_candidate_names(candidate_names)?;
    extract_from_artifact(artifact_path, format, &lookup, dest_dir)
}

/// Extract a binary whose location inside an archive is known exactly.
///
/// `archive_relative_path` must be a normal archive-relative path with no absolute root or parent
/// components. Naked binary and naked-compressed formats ignore the path because there is no
/// archive tree; they use `output_name` for the written binary filename.
pub(in crate::bin_resolver) fn extract_binary_at_archive_relative_path(
    artifact_path: &Path,
    format: ArchiveFormat,
    output_name: &str,
    archive_relative_path: &Path,
    dest_dir: &Path,
) -> Result<PathBuf> {
    let lookup = BinaryLookup::at_archive_relative_path(output_name, archive_relative_path)?;
    extract_from_artifact(artifact_path, format, &lookup, dest_dir)
}

///
/// Release artifacts come in two broad shapes. Most archives need a bounded search for one of the
/// expected binary names. Binstall metadata can instead name an exact archive-relative member path.
/// Naked binary and naked-compressed artifacts have no archive tree to inspect, so they use the
/// primary candidate name or explicit output name as the filename to write into `dest_dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BinaryLookup {
    CandidateNames { names: Vec<String> },
    ArchiveRelativePath { path: PathBuf, output_name: String },
}

impl BinaryLookup {
    fn by_candidate_names<I, S>(names: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let names = names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect::<Vec<_>>();

        if names.is_empty() || names.iter().any(|name| name.is_empty()) {
            return archive_extraction_failed(
                std::io::ErrorKind::InvalidInput,
                "binary candidate names must be non-empty",
            );
        }

        Ok(Self::CandidateNames { names })
    }

    fn at_archive_relative_path(output_name: impl AsRef<str>, path: impl AsRef<Path>) -> Result<Self> {
        let output_name = output_name.as_ref().to_string();
        if output_name.is_empty() {
            return archive_extraction_failed(
                std::io::ErrorKind::InvalidInput,
                "binary output name must be non-empty",
            );
        }

        let path = path.as_ref();
        validate_archive_member_path(path)?;

        Ok(Self::ArchiveRelativePath {
            path: path.to_path_buf(),
            output_name,
        })
    }

    fn output_name(&self) -> &str {
        match self {
            Self::CandidateNames { names } => names
                .first()
                .expect("candidate-name lookup is only constructed with at least one name"),
            Self::ArchiveRelativePath { output_name, .. } => output_name,
        }
    }
}

fn extract_from_artifact(
    artifact_path: &Path,
    format: ArchiveFormat,
    lookup: &BinaryLookup,
    dest_dir: &Path,
) -> Result<PathBuf> {
    match format {
        ArchiveFormat::Tar => extract_tar_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::TarGz => extract_tar_gz_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::TarXz => extract_tar_xz_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::TarZst => extract_tar_zst_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::TarBz2 => extract_tar_bz2_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::Zip => extract_zip_artifact(artifact_path, lookup, dest_dir),
        ArchiveFormat::Gz | ArchiveFormat::Xz | ArchiveFormat::Zst | ArchiveFormat::Bz2 => {
            extract_naked_compressed_artifact(artifact_path, format, lookup.output_name(), dest_dir)
        }
        ArchiveFormat::NakedBinary => {
            extract_naked_binary_artifact(artifact_path, lookup.output_name(), dest_dir)
        }
    }
}

fn extract_tar_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;
    unpack_tar_stream_and_resolve_binary(file, lookup, dest_dir)
}

fn extract_tar_gz_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;
    let decoder = GzDecoder::new(file);
    unpack_tar_stream_and_resolve_binary(decoder, lookup, dest_dir)
}

fn extract_tar_xz_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;
    let decoder = XzDecoder::new(file);
    unpack_tar_stream_and_resolve_binary(decoder, lookup, dest_dir)
}

fn extract_tar_zst_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;
    let decoder =
        zstd::stream::read::Decoder::new(file).map_err(|e| error::Error::ArchiveExtractionFailed {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        })?;
    unpack_tar_stream_and_resolve_binary(decoder, lookup, dest_dir)
}

fn extract_tar_bz2_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;
    let decoder = BzDecoder::new(file);
    unpack_tar_stream_and_resolve_binary(decoder, lookup, dest_dir)
}

fn unpack_tar_stream_and_resolve_binary<R: std::io::Read>(
    reader: R,
    lookup: &BinaryLookup,
    dest_dir: &Path,
) -> Result<PathBuf> {
    let mut archive = tar::Archive::new(reader);

    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    archive
        .unpack(dest_dir)
        .map_err(|e| error::Error::ArchiveExtractionFailed {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        })?;

    resolve_extracted_binary(dest_dir, lookup)
}

fn extract_zip_artifact(artifact_path: &Path, lookup: &BinaryLookup, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;

    let mut archive = zip::ZipArchive::new(file).map_err(|e| error::Error::ArchiveExtractionFailed {
        source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
    })?;

    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    archive
        .extract(dest_dir)
        .map_err(|e| error::Error::ArchiveExtractionFailed {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        })?;

    resolve_extracted_binary(dest_dir, lookup)
}

/// Extract a single executable compressed without a tar wrapper.
///
/// Unlike `tar.*` and `.zip` formats, the decompressed stream is the binary. There is no archive
/// tree to search, so the bytes are written directly to `dest_dir/{binary_name}{EXE_SUFFIX}` and
/// marked executable.
fn extract_naked_compressed_artifact(
    artifact_path: &Path,
    format: ArchiveFormat,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf> {
    let file = std::fs::File::open(artifact_path).with_context(|_| error::IoSnafu {
        path: artifact_path.to_path_buf(),
    })?;

    let mut reader: Box<dyn std::io::Read> = match format {
        ArchiveFormat::Gz => Box::new(GzDecoder::new(file)),
        ArchiveFormat::Xz => Box::new(XzDecoder::new(file)),
        ArchiveFormat::Bz2 => Box::new(BzDecoder::new(file)),
        ArchiveFormat::Zst => Box::new(zstd::stream::read::Decoder::new(file).map_err(|e| {
            error::Error::ArchiveExtractionFailed {
                source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
            }
        })?),
        ArchiveFormat::Tar
        | ArchiveFormat::TarGz
        | ArchiveFormat::TarXz
        | ArchiveFormat::TarZst
        | ArchiveFormat::TarBz2
        | ArchiveFormat::Zip
        | ArchiveFormat::NakedBinary => {
            unreachable!("BUG: extract_naked_compressed_artifact only handles naked compressed formats")
        }
    };

    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    let dest_path = dest_dir.join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));
    let mut dest_file = std::fs::File::create(&dest_path).with_context(|_| error::IoSnafu {
        path: dest_path.clone(),
    })?;

    std::io::copy(&mut reader, &mut dest_file).map_err(|e| error::Error::ArchiveExtractionFailed {
        source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
    })?;

    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&dest_path)
            .with_context(|_| error::IoSnafu {
                path: dest_path.clone(),
            })?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_path, perms).with_context(|_| error::IoSnafu {
            path: dest_path.clone(),
        })?;
    }

    Ok(dest_path)
}

fn extract_naked_binary_artifact(
    artifact_path: &Path,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    let dest_path = dest_dir.join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));

    std::fs::copy(artifact_path, &dest_path).with_context(|_| error::IoSnafu {
        path: dest_path.clone(),
    })?;

    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&dest_path)
            .with_context(|_| error::IoSnafu {
                path: dest_path.clone(),
            })?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_path, perms).with_context(|_| error::IoSnafu {
            path: dest_path.clone(),
        })?;
    }

    Ok(dest_path)
}

fn resolve_extracted_binary(dir: &Path, lookup: &BinaryLookup) -> Result<PathBuf> {
    match lookup {
        BinaryLookup::CandidateNames { names } => find_binary_by_candidate_names(dir, names),
        BinaryLookup::ArchiveRelativePath { path, .. } => find_binary_at_archive_relative_path(dir, path),
    }
}

/// Find a binary executable in a directory, searching common locations.
///
/// Looks for each binary name with the current platform's executable suffix in the archive root,
/// `bin/`, and `target/release/`, then repeats the same bounded checks under each immediate
/// top-level extracted directory.
fn find_binary_by_candidate_names(dir: &Path, binary_names: &[impl AsRef<str>]) -> Result<PathBuf> {
    let search_roots = bounded_search_roots(dir)?;
    let binary_names = binary_names.iter().map(AsRef::as_ref).collect::<Vec<_>>();

    for binary_name in &binary_names {
        for root in &search_roots {
            for candidate in candidate_binary_paths(root, binary_name) {
                if is_executable(&candidate) {
                    return Ok(candidate);
                }
            }
        }
    }

    archive_extraction_failed(
        std::io::ErrorKind::NotFound,
        format!(
            "binary [{}] not found in extracted archive",
            binary_names.join(", ")
        ),
    )
}

fn bounded_search_roots(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![dir.to_path_buf()];
    let mut top_level_dirs = Vec::new();

    for entry in std::fs::read_dir(dir).map_err(archive_extraction_error)? {
        let entry = entry.map_err(archive_extraction_error)?;
        let file_type = entry.file_type().map_err(archive_extraction_error)?;
        if file_type.is_dir() && !file_type.is_symlink() {
            top_level_dirs.push(entry.path());
        }
    }

    top_level_dirs.sort();
    roots.extend(top_level_dirs);

    Ok(roots)
}

fn candidate_binary_paths(root: &Path, binary_name: &str) -> [PathBuf; 6] {
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    [
        root.join(format!("{}{}", binary_name, exe_suffix)),
        root.join(binary_name),
        root.join("bin").join(format!("{}{}", binary_name, exe_suffix)),
        root.join("bin").join(binary_name),
        root.join("target")
            .join("release")
            .join(format!("{}{}", binary_name, exe_suffix)),
        root.join("target").join("release").join(binary_name),
    ]
}

fn find_binary_at_archive_relative_path(dir: &Path, archive_relative_path: &Path) -> Result<PathBuf> {
    let candidate = dir.join(archive_relative_path);
    if is_executable(&candidate) {
        return Ok(candidate);
    }

    archive_extraction_failed(
        std::io::ErrorKind::NotFound,
        format!(
            "binary '{}' not found in extracted archive",
            archive_relative_path.display()
        ),
    )
}

fn validate_archive_member_path(path: &Path) -> Result<()> {
    let valid = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    if valid {
        return Ok(());
    }

    archive_extraction_failed(
        std::io::ErrorKind::InvalidInput,
        format!("archive-relative binary path '{}' is invalid", path.display()),
    )
}

fn archive_extraction_failed<T>(kind: std::io::ErrorKind, message: impl Into<String>) -> Result<T> {
    let err = std::io::Error::new(kind, message.into());
    Err(error::Error::ArchiveExtractionFailed {
        source: Box::new(err) as Box<dyn std::error::Error + Send + Sync>,
    })
}

fn archive_extraction_error(source: std::io::Error) -> error::Error {
    error::Error::ArchiveExtractionFailed {
        source: Box::new(source) as Box<dyn std::error::Error + Send + Sync>,
    }
}

/// Check if a file is executable.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::{
        fs,
        io::{Cursor, Write},
    };

    use flate2::{Compression, write::GzEncoder};
    use xz2::write::XzEncoder;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::error::Error;

    impl ArchiveFormat {
        fn suffix(&self) -> &'static str {
            match self {
                Self::Tar => ".tar",
                Self::TarGz => ".tar.gz",
                Self::TarXz => ".tar.xz",
                Self::TarZst => ".tar.zst",
                Self::TarBz2 => ".tar.bz2",
                Self::Zip => ".zip",
                Self::Gz => ".gz",
                Self::Xz => ".xz",
                Self::Zst => ".zst",
                Self::Bz2 => ".bz2",
                Self::NakedBinary => {
                    if cfg!(windows) {
                        ".exe"
                    } else {
                        ""
                    }
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum BinaryLocation {
        Root,
        BinDir,
        TargetRelease,
        TopLevelRoot,
        TopLevelBinDir,
        TopLevelTargetRelease,
        TopLevelShareMan,
    }

    impl BinaryLocation {
        fn relative_path(&self, binary_name: &str) -> PathBuf {
            match self {
                Self::Root => PathBuf::from(binary_name),
                Self::BinDir => PathBuf::from("bin").join(binary_name),
                Self::TargetRelease => PathBuf::from("target").join("release").join(binary_name),
                Self::TopLevelRoot => PathBuf::from("release").join(binary_name),
                Self::TopLevelBinDir => PathBuf::from("release").join("bin").join(binary_name),
                Self::TopLevelTargetRelease => PathBuf::from("release")
                    .join("target")
                    .join("release")
                    .join(binary_name),
                Self::TopLevelShareMan => PathBuf::from("release")
                    .join("share")
                    .join("man")
                    .join(binary_name),
            }
        }
    }

    fn create_test_binary(temp_dir: &Path, binary_name: &str, location: BinaryLocation) -> PathBuf {
        let binary_path = temp_dir.join(location.relative_path(binary_name));
        fs::create_dir_all(binary_path.parent().unwrap()).unwrap();

        let mut file = fs::File::create(&binary_path).unwrap();
        file.write_all(b"#!/bin/sh\necho test").unwrap();

        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&binary_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).unwrap();
        }

        binary_path
    }

    fn create_test_tar_gz(binary_name: &str, location: BinaryLocation) -> Vec<u8> {
        let temp_dir = tempfile::tempdir().unwrap();
        create_test_binary(temp_dir.path(), binary_name, location);

        let mut archive_data = Vec::new();
        {
            let encoder = GzEncoder::new(&mut archive_data, Compression::default());
            let mut tar = tar::Builder::new(encoder);
            tar.append_dir_all(".", temp_dir.path()).unwrap();
            tar.finish().unwrap();
        }

        archive_data
    }

    fn create_test_tar_xz(binary_name: &str, location: BinaryLocation) -> Vec<u8> {
        let temp_dir = tempfile::tempdir().unwrap();
        create_test_binary(temp_dir.path(), binary_name, location);

        let mut archive_data = Vec::new();
        {
            let encoder = XzEncoder::new(&mut archive_data, 6);
            let mut tar = tar::Builder::new(encoder);
            tar.append_dir_all(".", temp_dir.path()).unwrap();
            tar.finish().unwrap();
        }

        archive_data
    }

    fn create_test_tar_zst(binary_name: &str, location: BinaryLocation) -> Vec<u8> {
        let temp_dir = tempfile::tempdir().unwrap();
        create_test_binary(temp_dir.path(), binary_name, location);

        let mut archive_data = Vec::new();
        {
            let encoder = zstd::stream::write::Encoder::new(&mut archive_data, 3).unwrap();
            let mut tar = tar::Builder::new(encoder);
            tar.append_dir_all(".", temp_dir.path()).unwrap();
            let encoder = tar.into_inner().unwrap();
            encoder.finish().unwrap();
        }

        archive_data
    }

    fn create_test_zip(binary_name: &str, location: BinaryLocation) -> Vec<u8> {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary_path = create_test_binary(temp_dir.path(), binary_name, location);
        let relative_path = location.relative_path(binary_name);

        let mut archive_data = Vec::new();
        {
            let cursor = Cursor::new(&mut archive_data);
            let mut zip = zip::ZipWriter::new(cursor);

            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);

            zip.start_file(relative_path.to_string_lossy(), options).unwrap();
            let content = fs::read(&binary_path).unwrap();
            zip.write_all(&content).unwrap();

            zip.finish().unwrap();
        }

        archive_data
    }

    #[test]
    fn test_extract_tar_gz_from_root() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::Root);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
        assert!(binary_path.to_string_lossy().contains("testbin"));
    }

    #[test]
    fn test_extract_tar_gz_from_bin_dir() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::BinDir);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
        assert!(binary_path.to_string_lossy().contains("bin"));
    }

    #[test]
    fn test_extract_tar_gz_from_target_release() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::TargetRelease);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
        assert!(
            binary_path.starts_with(dest_dir.path()),
            "Binary should be in dest_dir"
        );
    }

    #[test]
    fn test_extract_tar_gz_from_top_level_root() {
        let archive_data = create_test_tar_gz("rg", BinaryLocation::TopLevelRoot);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["rg"],
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.exists());
        assert!(
            binary_path.ends_with(Path::new("release").join("rg")),
            "expected to find rg under the top-level release directory"
        );
    }

    #[test]
    fn test_extract_tar_gz_from_top_level_bin_dir() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::TopLevelBinDir);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.ends_with(Path::new("release").join("bin").join("testbin")));
    }

    #[test]
    fn test_extract_tar_gz_from_top_level_target_release() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::TopLevelTargetRelease);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        )
        .unwrap();

        assert!(
            binary_path.ends_with(
                Path::new("release")
                    .join("target")
                    .join("release")
                    .join("testbin")
            )
        );
    }

    #[test]
    fn test_extract_tar_xz() {
        let archive_data = create_test_tar_xz("testbin", BinaryLocation::Root);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarXz,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
    }

    #[test]
    fn test_extract_tar_zst() {
        let archive_data = create_test_tar_zst("testbin", BinaryLocation::Root);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::TarZst,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
    }

    #[test]
    fn test_extract_zip_from_root() {
        let archive_data = create_test_zip("testbin", BinaryLocation::Root);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::Zip,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
    }

    #[test]
    fn test_extract_zip_from_bin_dir() {
        let archive_data = create_test_zip("testbin", BinaryLocation::BinDir);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::Zip,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
    }

    #[test]
    fn test_extract_zip_from_top_level_root_with_exe_fallback_name() {
        let archive_data = create_test_zip("rg.exe", BinaryLocation::TopLevelRoot);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::Zip,
            &["rg", "rg.exe"],
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.exists());
        assert_eq!(binary_path.file_name().unwrap().to_string_lossy(), "rg.exe");
    }

    #[test]
    fn test_extract_tar_gz_at_archive_relative_path() {
        let archive_data = create_test_tar_gz("testbin", BinaryLocation::TopLevelBinDir);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_at_archive_relative_path(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            "testbin",
            Path::new("release").join("bin").join("testbin").as_path(),
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.ends_with(Path::new("release").join("bin").join("testbin")));
    }

    #[test]
    fn test_extract_zip_at_archive_relative_path() {
        let archive_data = create_test_zip("testbin", BinaryLocation::TopLevelBinDir);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_at_archive_relative_path(
            temp_archive.path(),
            ArchiveFormat::Zip,
            "testbin",
            Path::new("release").join("bin").join("testbin").as_path(),
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.ends_with(Path::new("release").join("bin").join("testbin")));
    }

    #[test]
    fn test_extract_naked_binary() {
        let temp_binary = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_binary.path(), b"#!/bin/sh\necho test").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            temp_binary.path(),
            ArchiveFormat::NakedBinary,
            &["testbin"],
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
        assert!(binary_path.to_string_lossy().contains("testbin"));

        #[cfg(unix)]
        {
            let perms = fs::metadata(&binary_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o755, "Binary should have 755 permissions");
        }
    }

    /// A bare `.gz` (a single binary gzipped without a tar wrapper, as published by e.g. taplo)
    /// decompresses directly to the named binary, executable, with no archive tree to search.
    #[test]
    fn test_extract_naked_gz() {
        let payload = b"#!/bin/sh\necho test";

        let mut gz_data = Vec::new();
        {
            let mut encoder = GzEncoder::new(&mut gz_data, Compression::default());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap();
        }

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &gz_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_by_candidate_names(
            temp_archive.path(),
            ArchiveFormat::Gz,
            &["taplo"],
            dest_dir.path(),
        )
        .unwrap();

        assert!(binary_path.exists());
        assert_eq!(
            binary_path.file_name().unwrap().to_string_lossy(),
            format!("taplo{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(
            fs::read(&binary_path).unwrap(),
            payload,
            "decompressed bytes are the binary"
        );

        #[cfg(unix)]
        {
            let perms = fs::metadata(&binary_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o755, "Binary should have 755 permissions");
        }
    }

    #[test]
    fn test_extract_naked_gz_at_archive_relative_path_uses_output_name() {
        let payload = b"#!/bin/sh\necho test";

        let mut gz_data = Vec::new();
        {
            let mut encoder = GzEncoder::new(&mut gz_data, Compression::default());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap();
        }

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &gz_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let binary_path = extract_binary_at_archive_relative_path(
            temp_archive.path(),
            ArchiveFormat::Gz,
            "taplo",
            Path::new("ignored"),
            dest_dir.path(),
        )
        .unwrap();

        assert_eq!(
            binary_path.file_name().unwrap().to_string_lossy(),
            format!("taplo{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(fs::read(&binary_path).unwrap(), payload);
    }

    #[test]
    fn test_extract_rejects_empty_candidate_names() {
        let temp_binary = tempfile::NamedTempFile::new().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let result = extract_binary_by_candidate_names(
            temp_binary.path(),
            ArchiveFormat::NakedBinary,
            &[""],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_extract_rejects_invalid_archive_relative_path() {
        let temp_binary = tempfile::NamedTempFile::new().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let result = extract_binary_at_archive_relative_path(
            temp_binary.path(),
            ArchiveFormat::NakedBinary,
            "testbin",
            Path::new("../testbin"),
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_binary_not_found_in_archive() {
        let temp_src = tempfile::tempdir().unwrap();
        let binary_path = temp_src.path().join("otherbinary");
        fs::write(&binary_path, b"#!/bin/sh\necho test").unwrap();

        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&binary_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).unwrap();
        }

        let mut archive_data = Vec::new();
        {
            let encoder = GzEncoder::new(&mut archive_data, Compression::default());
            let mut tar = tar::Builder::new(encoder);
            tar.append_path_with_name(&binary_path, "otherbinary").unwrap();
            tar.finish().unwrap();
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_tar_gz() {
        let corrupt_data = b"This is not a valid gzip file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_tar_xz() {
        let corrupt_data = b"This is not a valid xz file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.xz");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarXz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_zip() {
        let corrupt_data = b"This is not a valid zip file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.zip");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::Zip,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_truncated_tar_gz() {
        let full_archive = create_test_tar_gz("testbin", BinaryLocation::Root);
        let truncated = &full_archive[..full_archive.len() / 2];

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, truncated).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_empty_tar_gz() {
        let mut archive_data = Vec::new();
        {
            let encoder = GzEncoder::new(&mut archive_data, Compression::default());
            let mut tar = tar::Builder::new(encoder);
            tar.finish().unwrap();
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn test_non_executable_file_in_archive() {
        let temp_src = tempfile::tempdir().unwrap();
        let binary_path = temp_src.path().join("testbin");

        let mut file = fs::File::create(&binary_path).unwrap();
        file.write_all(b"#!/bin/sh\necho test").unwrap();
        drop(file);

        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&binary_path, perms).unwrap();

        let mut archive_data = Vec::new();
        {
            let encoder = GzEncoder::new(&mut archive_data, Compression::default());
            let mut tar = tar::Builder::new(encoder);
            tar.append_path_with_name(&binary_path, "testbin").unwrap();
            tar.finish().unwrap();
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary_by_candidate_names(
            &archive_path,
            ArchiveFormat::TarGz,
            &["testbin"],
            dest_dir.path(),
        );

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_find_binary_with_exe_suffix() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary_name = "testbin";
        let binary_path = temp_dir
            .path()
            .join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));

        let mut file = fs::File::create(&binary_path).unwrap();
        file.write_all(b"test").unwrap();

        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&binary_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).unwrap();
        }

        let result = find_binary_by_candidate_names(temp_dir.path(), &[binary_name]);
        assert!(result.is_ok(), "Should find binary with EXE_SUFFIX");
        assert_eq!(result.unwrap(), binary_path);
    }

    #[test]
    #[cfg(unix)]
    fn test_find_binary_without_exe_suffix() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary_name = "testbin";
        let binary_path = temp_dir.path().join(binary_name);

        let mut file = fs::File::create(&binary_path).unwrap();
        file.write_all(b"test").unwrap();

        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();

        let result = find_binary_by_candidate_names(temp_dir.path(), &[binary_name]);
        assert!(result.is_ok(), "Should find binary without .exe suffix on Unix");
        assert_eq!(result.unwrap(), binary_path);
    }

    #[test]
    fn test_find_binary_in_nested_locations() {
        let temp_dir = tempfile::tempdir().unwrap();
        let binary_name = "testbin";

        let nested_path = temp_dir.path().join("target").join("release");
        fs::create_dir_all(&nested_path).unwrap();
        let binary_path = nested_path.join(binary_name);

        let mut file = fs::File::create(&binary_path).unwrap();
        file.write_all(b"test").unwrap();

        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&binary_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).unwrap();
        }

        let result = find_binary_by_candidate_names(temp_dir.path(), &[binary_name]);
        assert!(result.is_ok(), "Should find binary in target/release");
        assert_eq!(result.unwrap(), binary_path);
    }

    #[test]
    fn test_find_binary_does_not_scan_deep_unrelated_paths() {
        let temp_dir = tempfile::tempdir().unwrap();
        create_test_binary(temp_dir.path(), "testbin", BinaryLocation::TopLevelShareMan);

        let result = find_binary_by_candidate_names(temp_dir.path(), &["testbin"]);

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_find_binary_prioritizes_binary_name_before_fallback_name() {
        let temp_dir = tempfile::tempdir().unwrap();
        let primary = create_test_binary(temp_dir.path(), "rg", BinaryLocation::TopLevelRoot);
        create_test_binary(temp_dir.path(), "ripgrep", BinaryLocation::Root);

        let result = find_binary_by_candidate_names(temp_dir.path(), &["rg", "ripgrep"]).unwrap();

        assert_eq!(result, primary);
    }

    #[test]
    #[cfg(unix)]
    fn test_find_binary_skips_symlinked_top_level_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let real_temp_dir = tempfile::tempdir().unwrap();
        let real_dir = real_temp_dir.path().join("real-release");
        fs::create_dir(&real_dir).unwrap();
        let binary_path = real_dir.join("testbin");
        fs::write(&binary_path, b"test").unwrap();

        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();

        symlink(&real_dir, temp_dir.path().join("release")).unwrap();

        let result = find_binary_by_candidate_names(temp_dir.path(), &["testbin"]);

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn archive_format_suffix_consistency() {
        assert_eq!(ArchiveFormat::Tar.suffix(), ".tar");
        assert_eq!(ArchiveFormat::TarGz.suffix(), ".tar.gz");
        assert_eq!(ArchiveFormat::TarXz.suffix(), ".tar.xz");
        assert_eq!(ArchiveFormat::TarZst.suffix(), ".tar.zst");
        assert_eq!(ArchiveFormat::TarBz2.suffix(), ".tar.bz2");
        assert_eq!(ArchiveFormat::Zip.suffix(), ".zip");
    }
}
