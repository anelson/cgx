use crate::{Result, error};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use snafu::ResultExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use xz2::read::XzDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::bin_resolver) enum ArchiveFormat {
    Tar,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Zip,
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
                (Self::NakedBinary, ""),
            ]
        }
    }
}

/// Extract a binary from an archive or naked binary file.
///
/// The caller specifies the [`ArchiveFormat`] explicitly; there is no detection or fallback.
pub(in crate::bin_resolver) fn extract_binary(
    archive_path: &Path,
    format: ArchiveFormat,
    expected_binary_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf> {
    match format {
        ArchiveFormat::Tar => extract_tar(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::TarGz => extract_tar_gz(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::TarXz => extract_tar_xz(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::TarZst => extract_tar_zst(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::TarBz2 => extract_tar_bz2(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::Zip => extract_zip(archive_path, expected_binary_name, dest_dir),
        ArchiveFormat::NakedBinary => extract_naked_binary(archive_path, expected_binary_name, dest_dir),
    }
}

fn extract_tar(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
    })?;
    extract_tar_archive(file, binary_name, dest_dir)
}

fn extract_tar_gz(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
    })?;
    let decoder = GzDecoder::new(file);
    extract_tar_archive(decoder, binary_name, dest_dir)
}

fn extract_tar_xz(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
    })?;
    let decoder = XzDecoder::new(file);
    extract_tar_archive(decoder, binary_name, dest_dir)
}

fn extract_tar_zst(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
    })?;
    let decoder =
        zstd::stream::read::Decoder::new(file).map_err(|e| error::Error::ArchiveExtractionFailed {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        })?;
    extract_tar_archive(decoder, binary_name, dest_dir)
}

fn extract_tar_bz2(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
    })?;
    let decoder = BzDecoder::new(file);
    extract_tar_archive(decoder, binary_name, dest_dir)
}

fn extract_tar_archive<R: std::io::Read>(reader: R, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let mut archive = tar::Archive::new(reader);

    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    archive
        .unpack(dest_dir)
        .map_err(|e| error::Error::ArchiveExtractionFailed {
            source: Box::new(e) as Box<dyn std::error::Error + Send + Sync>,
        })?;

    find_binary_in_dir(dest_dir, binary_name)
}

fn extract_zip(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive_path).with_context(|_| error::IoSnafu {
        path: archive_path.to_path_buf(),
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

    find_binary_in_dir(dest_dir, binary_name)
}

fn extract_naked_binary(archive_path: &Path, binary_name: &str, dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir).with_context(|_| error::IoSnafu {
        path: dest_dir.to_path_buf(),
    })?;

    let dest_path = dest_dir.join(format!("{}{}", binary_name, std::env::consts::EXE_SUFFIX));

    std::fs::copy(archive_path, &dest_path).with_context(|_| error::IoSnafu {
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

/// Find a binary executable in a directory, searching common locations.
///
/// Looks for:
/// - `binary_name` or `binary_name.exe` in the root
/// - `binary_name` or `binary_name.exe` in `bin/`
/// - `binary_name` or `binary_name.exe` in `target/release/`
fn find_binary_in_dir(dir: &Path, binary_name: &str) -> Result<PathBuf> {
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    let candidates = [
        dir.join(format!("{}{}", binary_name, exe_suffix)),
        dir.join(binary_name),
        dir.join("bin").join(format!("{}{}", binary_name, exe_suffix)),
        dir.join("bin").join(binary_name),
        dir.join("target")
            .join("release")
            .join(format!("{}{}", binary_name, exe_suffix)),
        dir.join("target").join("release").join(binary_name),
    ];

    for candidate in &candidates {
        if candidate.exists() && is_executable(candidate) {
            return Ok(candidate.clone());
        }
    }

    let err = std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("binary '{}' not found in extracted archive", binary_name),
    );
    Err(error::Error::ArchiveExtractionFailed {
        source: Box::new(err) as Box<dyn std::error::Error + Send + Sync>,
    })
}

/// Check if a file is executable.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use flate2::{Compression, write::GzEncoder};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        io::{Cursor, Write},
    };
    use xz2::write::XzEncoder;
    use zip::write::SimpleFileOptions;

    impl ArchiveFormat {
        fn suffix(&self) -> &'static str {
            match self {
                Self::Tar => ".tar",
                Self::TarGz => ".tar.gz",
                Self::TarXz => ".tar.xz",
                Self::TarZst => ".tar.zst",
                Self::TarBz2 => ".tar.bz2",
                Self::Zip => ".zip",
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
    }

    impl BinaryLocation {
        fn relative_path(&self, binary_name: &str) -> PathBuf {
            match self {
                Self::Root => PathBuf::from(binary_name),
                Self::BinDir => PathBuf::from("bin").join(binary_name),
                Self::TargetRelease => PathBuf::from("target").join("release").join(binary_name),
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            "testbin",
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            "testbin",
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::TarGz,
            "testbin",
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
    fn test_extract_tar_xz() {
        let archive_data = create_test_tar_xz("testbin", BinaryLocation::Root);

        let temp_archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_archive.path(), &archive_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::TarXz,
            "testbin",
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::TarZst,
            "testbin",
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::Zip,
            "testbin",
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
        let result = extract_binary(
            temp_archive.path(),
            ArchiveFormat::Zip,
            "testbin",
            dest_dir.path(),
        );

        let binary_path = result.unwrap();
        assert!(binary_path.exists());
    }

    #[test]
    fn test_extract_naked_binary() {
        let temp_binary = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp_binary.path(), b"#!/bin/sh\necho test").unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary(
            temp_binary.path(),
            ArchiveFormat::NakedBinary,
            "testbin",
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
        let result = extract_binary(&archive_path, ArchiveFormat::TarGz, "testbin", dest_dir.path());

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_tar_gz() {
        let corrupt_data = b"This is not a valid gzip file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.gz");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary(&archive_path, ArchiveFormat::TarGz, "testbin", dest_dir.path());

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_tar_xz() {
        let corrupt_data = b"This is not a valid xz file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.tar.xz");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary(&archive_path, ArchiveFormat::TarXz, "testbin", dest_dir.path());

        assert_matches::assert_matches!(result, Err(Error::ArchiveExtractionFailed { .. }));
    }

    #[test]
    fn test_corrupt_zip() {
        let corrupt_data = b"This is not a valid zip file";

        let temp_dir = tempfile::tempdir().unwrap();
        let archive_path = temp_dir.path().join("test.zip");
        fs::write(&archive_path, corrupt_data).unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let result = extract_binary(&archive_path, ArchiveFormat::Zip, "testbin", dest_dir.path());

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
        let result = extract_binary(&archive_path, ArchiveFormat::TarGz, "testbin", dest_dir.path());

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
        let result = extract_binary(&archive_path, ArchiveFormat::TarGz, "testbin", dest_dir.path());

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
        let result = extract_binary(&archive_path, ArchiveFormat::TarGz, "testbin", dest_dir.path());

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

        let result = find_binary_in_dir(temp_dir.path(), binary_name);
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

        let result = find_binary_in_dir(temp_dir.path(), binary_name);
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

        let result = find_binary_in_dir(temp_dir.path(), binary_name);
        assert!(result.is_ok(), "Should find binary in target/release");
        assert_eq!(result.unwrap(), binary_path);
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
