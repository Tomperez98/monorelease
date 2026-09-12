//! The one module in `mono` that knows which operating system it is running on.
//!
//! Every platform difference the crate has is funnelled through here so that
//! no other module needs `#[cfg(target_os)]`. A function in this module either
//! delegates to a library that already abstracts the platform (process trees
//! via [`process_wrap`]) or implements one platform's semantics in exactly one
//! place (POSIX permission bits). Where the platforms genuinely differ, the
//! difference is named here rather than smeared across call sites.

#[cfg(unix)]
use std::fs;
use std::io;
use std::path::Path;

use process_wrap::std::CommandWrap;
#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;

/// The mode a freshly created regular file receives when there is no existing
/// destination to inherit from. Chosen to match the conventional `umask 022`.
#[cfg(unix)]
const DEFAULT_FILE_MODE: u32 = 0o644;

/// Put `command` in a process tree that can be terminated as a unit.
///
/// [`process_wrap`] deliberately exposes the Unix and Windows mechanisms as
/// distinct wrappers — a Unix process group and a Windows Job Object — so this
/// selection is the smallest possible platform branch. It lives here so the
/// rest of the crate spawns processes the same way on every target.
pub(crate) fn configure_process_tree(command: &mut CommandWrap) {
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());

    #[cfg(windows)]
    command.wrap(JobObject);
}

/// Whether a failed process-tree termination should be read as "the tree had
/// already exited" rather than as an error.
///
/// `killpg` can lose a race with a short-lived child on macOS and report
/// `EPERM` for a process group that no longer exists. Callers already treat an
/// error as benign when the child has been reaped; this covers the narrower
/// macOS window where it has not. On other platforms a termination failure is
/// always a real failure.
pub(crate) fn termination_failure_means_already_exited(error: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    {
        error.kind() == io::ErrorKind::PermissionDenied
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = error;
        false
    }
}

/// The permission bits `path` carries, when the platform stores them.
///
/// Windows has no POSIX mode, so callers receive `None` and leave the bits out
/// of any fingerprint built from them.
pub(crate) fn file_mode(path: &Path) -> io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(fs::metadata(path)?.permissions().mode()))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Restore permission bits onto `path`, when the platform stores them.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Give a temporary file the permissions a file published at `destination`
/// should end up with.
///
/// `tempfile` creates its files private (`0600`) so that a partially written
/// temporary is never world-readable. Publishing renames that file into place,
/// so without this step every `mono`-written manifest and changelog would
/// become owner-only. An existing destination keeps its mode; a new file gets
/// [`DEFAULT_FILE_MODE`].
pub(crate) fn prepare_published_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mode = match fs::metadata(destination) {
            Ok(metadata) => {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => DEFAULT_FILE_MODE,
            Err(error) => return Err(error),
        };
        set_mode(temporary, mode)
    }

    #[cfg(not(unix))]
    {
        let _ = (temporary, destination);
        Ok(())
    }
}
