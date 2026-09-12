//! The one module in `xtask` that knows which operating system it is running
//! on, mirroring the published crate's `platform` module.
//!
//! `xtask` only differs by platform in one place: Unix piped installers need
//! the executable bit before they are packed into a `.tar.gz`, while the
//! Windows `.zip` archive has no such concept.

use std::path::Path;

use crate::Error;

/// Mark `path` executable, on the platforms that have an executable bit.
#[allow(unused_variables)]
pub(crate) fn set_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}
