use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cache::CacheError;

const HASH_BUFFER_SIZE: usize = 64 * 1024;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(super) fn hash_strings(hasher: &mut Sha256, values: &[String]) {
    hash_string(hasher, &values.len().to_string());
    for value in values {
        hash_string(hasher, value);
    }
}

pub(super) fn hash_string(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

/// Stream a file into the hasher so an oversized input never has to fit in
/// memory at once.
pub(super) fn hash_file(hasher: &mut Sha256, label: &str, path: &Path) -> Result<(), CacheError> {
    let mut file =
        fs::File::open(path).map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    let metadata = file
        .metadata()
        .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    hash_string(hasher, label);
    hasher.update(metadata.len().to_le_bytes());
    if let Some(mode) = file_mode(path)? {
        hasher.update(mode.to_le_bytes());
    }

    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

pub(super) fn file_digest(path: &Path) -> Result<String, CacheError> {
    let mut file =
        fs::File::open(path).map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn read_file(path: &Path) -> Result<Vec<u8>, CacheError> {
    fs::read(path).map_err(|source| CacheError::io(path.to_path_buf(), source))
}

pub(super) fn hash_bytes(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hash_string(hasher, label);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

pub(super) fn hex_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        hex.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
}

pub(super) fn file_mode(path: &Path) -> Result<Option<u32>, CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(
            fs::metadata(path)
                .map_err(|source| CacheError::io(path.to_path_buf(), source))?
                .permissions()
                .mode(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

pub(super) fn set_mode(path: &Path, mode: u32) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, permissions)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub(super) fn validate_cached_path(path: &str) -> Result<PathBuf, CacheError> {
    let relative = Path::new(path);
    if path.is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(CacheError::Invalid {
            message: format!("cache entry contains unsafe output path '{path}'"),
        });
    }
    Ok(relative.to_path_buf())
}

pub(super) fn ensure_inside(root: &Path, path: &Path) -> Result<(), CacheError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(CacheError::Invalid {
            message: format!("cache path escapes project root: {}", path.display()),
        })
    }
}

/// Reject cache output restoration through any symlink component from the
/// project root down to (and including) the destination path.
///
/// Uses `symlink_metadata` instead of `metadata` so a symlink is detected
/// rather than followed.  The path may not exist yet — that is only the
/// destination file, not an intermediate directory, but we stop scanning at
/// the first missing component since earlier components must exist.
pub(super) fn ensure_no_symlink_components(
    root: &Path,
    destination: &Path,
) -> Result<(), CacheError> {
    ensure_inside(root, destination)?;

    let relative = destination
        .strip_prefix(root)
        .map_err(|_| CacheError::Invalid {
            message: format!(
                "cache destination escapes project root: {}",
                destination.display()
            ),
        })?;

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => return Err(CacheError::io(current, source)),
        };
        if meta.file_type().is_symlink() {
            return Err(CacheError::Invalid {
                message: format!(
                    "cache output restoration refuses symlink component {}",
                    current.display()
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digest_uses_lowercase_two_digit_encoding() {
        assert_eq!(hex_digest(&[0x00, 0x01, 0xab, 0xff]), "0001abff");
    }

    #[test]
    fn cached_paths_accept_nested_relative_names_only() {
        assert!(validate_cached_path("dist/app.bin").is_ok());
        for unsafe_path in ["", "/tmp/app", "../app", "dist/../../app"] {
            assert!(validate_cached_path(unsafe_path).is_err(), "{unsafe_path}");
        }
    }

    #[test]
    fn hash_string_framing_distinguishes_different_value_boundaries() {
        let mut first = Sha256::new();
        hash_strings(&mut first, &["ab".to_owned(), "c".to_owned()]);
        let mut second = Sha256::new();
        hash_strings(&mut second, &["a".to_owned(), "bc".to_owned()]);

        assert_ne!(first.finalize(), second.finalize());
    }
}
