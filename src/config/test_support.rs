//! Shared helpers for the tests in this module that touch the filesystem.
//!
//! Collected here rather than repeated per test module so that how a test
//! temporary directory comes into being is decided in one place. These tests
//! are about permissions, and on a shared CI host `/tmp` is world-writable, so
//! the answer matters: a predictable name is fine, running inside a directory
//! this process did not create is not.

use std::path::{Path, PathBuf};

/// A fresh, owner-only temporary directory named after the calling module and
/// a per-test tag.
///
/// The name stays deterministic (module, tag and the pid) so repeated runs
/// reuse the same path instead of piling up under `/tmp`. Creation is a plain
/// non-recursive `mkdir`, which fails if anything already occupies the path —
/// including a symlink another local account planted between the cleanup below
/// and this call. Failing the test is the point: the alternative is a
/// permissions test that quietly runs inside someone else's directory.
pub(crate) fn temp_dir(module: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mostro-{module}-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir).unwrap_or_else(|e| {
        panic!(
            "could not create the test directory {}: {e}. It is never reused when something \
             already occupies the path — remove whatever is there and rerun.",
            dir.display()
        )
    });
    dir
}

/// The path's permission bits, or `None` on platforms that have none.
///
/// `Option` rather than `u32` so assertions compile everywhere and the tests
/// that only care about content stay portable; the mode assertions read
/// `Some(0o600)`.
#[cfg(unix)]
pub(crate) fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    let metadata =
        std::fs::metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    Some(metadata.permissions().mode() & 0o777)
}

/// Non-Unix platforms have no POSIX permission bits to report.
#[cfg(not(unix))]
pub(crate) fn mode_of(_path: &Path) -> Option<u32> {
    None
}

/// Assert a path's permission bits, where the platform has any.
///
/// A no-op elsewhere, so the tests around it — that a file is created, refused
/// or replaced — stay portable instead of being compiled only on Unix.
#[cfg(unix)]
pub(crate) fn assert_mode(path: &Path, expected: u32) {
    assert_eq!(
        mode_of(path),
        Some(expected),
        "unexpected mode on {}",
        path.display()
    );
}

/// Non-Unix platforms have no POSIX permission bits to assert on.
#[cfg(not(unix))]
pub(crate) fn assert_mode(_path: &Path, _expected: u32) {}

/// Loosen (or otherwise set) a path's permission bits, where the platform has
/// any. A no-op elsewhere.
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("set mode {mode:04o} on {}: {e}", path.display()));
}

/// Non-Unix platforms have no POSIX permission bits to set.
#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) {}
