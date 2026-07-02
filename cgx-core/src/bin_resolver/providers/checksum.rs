use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

use sha2::{Digest, Sha256};
use snafu::ResultExt;

use crate::{
    Result, error,
    messages::{MessageReporter, PrebuiltBinaryMessage},
};

const HASH_BUFFER_BYTES: usize = 64 * 1024;
const CHECKSUM_UNPARSABLE_PREVIEW_BYTES: usize = 16 * 1024;

/// Verify that the SHA256 hash of the file at `asset_path` matches the SHA256 checksum for the
/// file `asset_filename` stored in `checksum_path`.
///
/// `asset_filename` is used to select the relevant checksum line when the checksum file contains
/// multiple hashes. The asset itself is hashed by streaming the file from disk.
pub(super) fn verify_sha256_checksum(
    asset_path: &Path,
    checksum_path: &Path,
    asset_filename: &str,
    reporter: &MessageReporter,
) -> Result<()> {
    let expected_hash = parse_sha256_checksum_file(checksum_path, asset_filename)?;

    reporter.report(|| PrebuiltBinaryMessage::verifying_checksum(&expected_hash));

    let actual_hash = sha256_file(asset_path)?;

    if expected_hash != actual_hash {
        return error::ChecksumMismatchSnafu {
            expected: expected_hash,
            actual: actual_hash,
        }
        .fail();
    }

    reporter.report(PrebuiltBinaryMessage::checksum_verified);

    Ok(())
}

/// Compute the SHA256 hash of a file and return the hex string representation.
///
/// The lowercase hex representation of the hash is used in the checksum files, so the output of
/// this function is also lowercase hex.
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|_| error::IoSnafu {
        path: path.to_path_buf(),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];

    loop {
        let read = file.read(&mut buffer).with_context(|_| error::IoSnafu {
            path: path.to_path_buf(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(crate::helpers::format_hex_lower(hasher.finalize()))
}

pub(super) fn asset_filename_from_url(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(path)
}

fn parse_sha256_checksum_file(path: &Path, asset_filename: &str) -> Result<String> {
    let file = File::open(path).with_context(|_| error::IoSnafu {
        path: path.to_path_buf(),
    })?;
    let reader = BufReader::new(file);
    let mut first_token = None;
    let mut preview = String::new();

    for line in reader.lines() {
        let line = line.with_context(|_| error::IoSnafu {
            path: path.to_path_buf(),
        })?;
        append_unparsable_preview(&mut preview, &line);
        if let Some(token) = search_sha256_line(&mut first_token, &line, asset_filename) {
            return Ok(token);
        }
    }

    finish_sha256_search(first_token, preview)
}

fn search_sha256_line(first_token: &mut Option<String>, line: &str, asset_filename: &str) -> Option<String> {
    let token = first_sha256_token(line)?;
    if line.contains(asset_filename) {
        return Some(token);
    }
    if first_token.is_none() {
        *first_token = Some(token);
    }
    None
}

fn finish_sha256_search(first_token: Option<String>, contents: String) -> Result<String> {
    if let Some(token) = first_token {
        return Ok(token);
    }
    error::ChecksumUnparsableSnafu { contents }.fail()
}

fn append_unparsable_preview(preview: &mut String, line: &str) {
    if preview.len() >= CHECKSUM_UNPARSABLE_PREVIEW_BYTES {
        return;
    }

    let remaining = CHECKSUM_UNPARSABLE_PREVIEW_BYTES - preview.len();
    preview.extend(line.chars().take(remaining));
    if preview.len() < CHECKSUM_UNPARSABLE_PREVIEW_BYTES {
        preview.push('\n');
    }
}

fn first_sha256_token(line: &str) -> Option<String> {
    let mut run_start = None;
    let mut run_len = 0;

    let bytes = line.as_bytes();
    for (idx, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_hexdigit() {
            if run_start.is_none() {
                run_start = Some(idx);
            }
            run_len += 1;
            continue;
        }

        if run_len == 64 {
            let start = run_start.expect("a 64-character hex run has a start index");
            return Some(lower_ascii_hex(&bytes[start..idx]));
        }

        run_start = None;
        run_len = 0;
    }

    if run_len == 64 {
        let start = run_start.expect("a 64-character hex run has a start index");
        return Some(lower_ascii_hex(&bytes[start..]));
    }

    let compact: String = line.chars().filter(|ch| !ch.is_ascii_whitespace()).collect();
    if compact.len() == 64 && compact.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Some(compact.to_ascii_lowercase());
    }

    None
}

fn lower_ascii_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| char::from(*byte).to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;
    use crate::error::Error;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_HASH: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn parse_sha256_checksum(contents: &str, asset_filename: &str) -> Result<String> {
        let mut first_token = None;
        for line in contents.lines() {
            if let Some(token) = search_sha256_line(&mut first_token, line, asset_filename) {
                return Ok(token);
            }
        }
        finish_sha256_search(first_token, contents.to_string())
    }

    #[test]
    fn parses_gnu_sha256sum() {
        let contents = format!("{HASH}  archive.tar.gz\n");
        assert_eq!(parse_sha256_checksum(&contents, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn parses_bare_hash() {
        assert_eq!(parse_sha256_checksum(HASH, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn parses_bsd_style() {
        let contents = format!("SHA256 (archive.tar.gz) = {HASH}\n");
        assert_eq!(parse_sha256_checksum(&contents, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn parses_powershell_style() {
        let contents = format!(
            "Algorithm       Hash       Path\r\nSHA256          {HASH}       C:\\temp\\archive.zip\r\n"
        );
        assert_eq!(parse_sha256_checksum(&contents, "archive.zip").unwrap(), HASH);
    }

    #[test]
    fn parses_uppercase_hash_as_lowercase() {
        let contents = format!("{}  archive.tar.gz\n", HASH.to_ascii_uppercase());
        assert_eq!(parse_sha256_checksum(&contents, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn parses_certutil_crlf_output() {
        let spaced_hash = HASH
            .as_bytes()
            .chunks(2)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        let contents = format!(
            "SHA256 hash of archive.zip:\r\n{spaced_hash}\r\nCertUtil: -hashfile command completed \
             successfully.\r\n"
        );
        assert_eq!(parse_sha256_checksum(&contents, "archive.zip").unwrap(), HASH);
    }

    #[test]
    fn prefers_hash_on_line_matching_asset_filename() {
        let contents = format!("{OTHER_HASH}  other.tar.gz\n{HASH}  archive.tar.gz\n");
        assert_eq!(parse_sha256_checksum(&contents, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn returns_first_hash_when_no_line_matches_filename() {
        let contents = format!("{HASH}  other.tar.gz\n{OTHER_HASH}  another.tar.gz\n");
        assert_eq!(parse_sha256_checksum(&contents, "archive.tar.gz").unwrap(), HASH);
    }

    #[test]
    fn unparsable_content_returns_error() {
        assert_matches!(
            parse_sha256_checksum("not a checksum", "archive.tar.gz"),
            Err(Error::ChecksumUnparsable { .. })
        );
    }

    #[test]
    fn verifies_matching_checksum() {
        let data = b"hello";
        let asset = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(asset.path(), data).unwrap();
        let checksum = tempfile::NamedTempFile::new().unwrap();
        let digest = {
            let mut hasher = Sha256::new();
            hasher.update(data);
            crate::helpers::format_hex_lower(hasher.finalize())
        };
        std::fs::write(checksum.path(), format!("{digest}  archive.tar.gz")).unwrap();

        verify_sha256_checksum(
            asset.path(),
            checksum.path(),
            "archive.tar.gz",
            &MessageReporter::null(),
        )
        .unwrap();
    }

    #[test]
    fn mismatched_checksum_returns_error() {
        let asset = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(asset.path(), b"hello").unwrap();
        let checksum = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(checksum.path(), format!("{OTHER_HASH}  archive.tar.gz")).unwrap();

        let result = verify_sha256_checksum(
            asset.path(),
            checksum.path(),
            "archive.tar.gz",
            &MessageReporter::null(),
        );

        assert_matches!(result, Err(Error::ChecksumMismatch { .. }));
    }
}
