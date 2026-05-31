use crate::{Result, error};
use ignore::WalkBuilder;
use snafu::ResultExt;
use std::{fmt::Write, path::Path};

/// Format a byte slice as a lowercase hex string.
///
/// `sha2 0.11` returns digests as type that encapsulates a byte slice, which does not
/// implement [`std::fmt::LowerHex`]. Callers feed the return value of `hasher.finalize()`
/// directly to this function and get a lowercase hex repr back.
pub(crate) fn format_hex_lower(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Copy source files from src to dst, respecting .gitignore patterns.
///
/// Uses the `ignore` crate to walk the source tree while respecting gitignore rules,
/// then copies each file to the destination, preserving directory structure.
pub(crate) fn copy_source_tree(src: &Path, dst: &Path) -> Result<()> {
    let walker = WalkBuilder::new(src)
        .hidden(false) // Include hidden files (like .cargo)
        .git_ignore(true) // Respect .gitignore
        .git_exclude(true) // Respect .git/info/exclude
        .build();

    for result in walker {
        let entry = result
            .map_err(|e| Box::new(e) as _)
            .with_context(|_| error::CopySourceTreeSnafu {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
            })?;

        let src_path = entry.path();
        if src_path == src {
            continue; // Skip the root directory itself
        }

        let rel_path = src_path.strip_prefix(src).unwrap();
        let dst_path = dst.join(rel_path);

        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .map_err(|e| Box::new(e) as _)
                .with_context(|_| error::CopySourceTreeSnafu {
                    src: src.to_path_buf(),
                    dst: dst.to_path_buf(),
                })?;
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Box::new(e) as _)
                    .with_context(|_| error::CopySourceTreeSnafu {
                        src: src.to_path_buf(),
                        dst: dst.to_path_buf(),
                    })?;
            }
            std::fs::copy(src_path, &dst_path)
                .map_err(|e| Box::new(e) as _)
                .with_context(|_| error::CopySourceTreeSnafu {
                    src: src.to_path_buf(),
                    dst: dst.to_path_buf(),
                })?;
        }
    }

    Ok(())
}
