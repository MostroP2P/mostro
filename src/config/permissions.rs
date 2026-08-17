//! Startup checks on the filesystem permissions of the credential files
//! mostrod reads.
//!
//! Kept in its own module — rather than folded into `config::util` — because
//! these checks are about the files named by the settings, not about loading
//! the settings themselves.

use std::path::Path;

/// Warn when a credential file has any of its "other" permission bits set,
/// which puts it within reach of every account on the host.
///
/// The check is advisory: a node whose macaroon is `0644` still starts, it
/// just says so out loud once per boot. Refusing to start would turn a
/// hardening gap into an outage on the next upgrade for every operator who
/// already runs that way.
///
/// Empty paths and files that cannot be stat'ed are ignored: an unset or
/// unreadable path is not a permissions problem, and the real failure is
/// reported with far more context by whoever opens the file (for the macaroon,
/// `LndConnector::new`).
pub fn warn_if_other_accessible(path: &str, label: &str) {
    if path.is_empty() {
        return;
    }

    if let Some(mode) = other_accessible_mode(Path::new(path)) {
        // `chmod o=` rather than `chmod 600`: the file may legitimately be
        // owned by the LND account and read by mostrod through the node's
        // group, and following advice that drops the group bits would leave
        // the daemon unable to authenticate on its next restart.
        tracing::warn!(
            "{label} ({path}) has permissions {mode:04o}: its \"other\" bits are set, so every \
             account on this host can reach it. Clear them with: chmod o= {path}"
        );
    }
}

/// The file's permission bits when the "other" class has any of them set,
/// `None` otherwise.
///
/// Group access is deliberately tolerated: LND itself creates
/// `admin.macaroon` with mode `0640`, and granting a service account access
/// through the node's group is a legitimate deployment, not a finding.
///
/// Only the mode bits are read. A POSIX ACL can widen access without touching
/// them, so a quiet startup means "the mode bits are sane", not "no other
/// account can read this file" — the check is a cheap guard against the
/// documented copy-it-into-place flows, not an audit.
///
/// `metadata` follows symlinks on purpose — pointing `lnd_macaroon_file` at a
/// link is common, and what matters is the mode of the file that is actually
/// read.
#[cfg(unix)]
fn other_accessible_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o007 != 0).then_some(mode)
}

/// Non-Unix platforms have no POSIX permission bits to inspect.
#[cfg(not(unix))]
fn other_accessible_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-permissions-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn macaroon_with_mode(tag: &str, mode: u32) -> PathBuf {
        let path = temp_dir(tag).join("admin.macaroon");
        std::fs::write(&path, b"macaroon").expect("write macaroon");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("set permissions");
        path
    }

    #[test]
    fn owner_only_is_accepted() {
        let path = macaroon_with_mode("owner-only", 0o600);
        assert_eq!(other_accessible_mode(&path), None);
    }

    #[test]
    fn group_readable_is_accepted() {
        // 0640 is the mode LND writes admin.macaroon with, and reaching it
        // through the node's group is a supported setup.
        let path = macaroon_with_mode("group-read", 0o640);
        assert_eq!(other_accessible_mode(&path), None);
    }

    #[test]
    fn world_readable_is_reported() {
        let path = macaroon_with_mode("world-read", 0o644);
        assert_eq!(other_accessible_mode(&path), Some(0o644));
    }

    #[test]
    fn other_read_without_group_read_is_reported() {
        let path = macaroon_with_mode("other-read", 0o604);
        assert_eq!(other_accessible_mode(&path), Some(0o604));
    }

    #[test]
    fn world_writable_is_reported() {
        // Not a disclosure by itself, but a local account that can replace the
        // credential mostrod authenticates with is the same class of problem.
        let path = macaroon_with_mode("world-write", 0o602);
        assert_eq!(other_accessible_mode(&path), Some(0o602));
    }

    #[test]
    fn symlink_reports_the_target_mode() {
        let dir = temp_dir("symlink");
        let target = dir.join("real.macaroon");
        std::fs::write(&target, b"macaroon").expect("write macaroon");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("set permissions");

        let link = dir.join("linked.macaroon");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        assert_eq!(other_accessible_mode(&link), Some(0o644));
    }

    #[test]
    fn missing_file_is_ignored() {
        assert_eq!(
            other_accessible_mode(Path::new("/definitely/not/here.macaroon")),
            None
        );
    }

    #[test]
    fn empty_and_missing_paths_do_not_panic() {
        warn_if_other_accessible("", "LND admin macaroon");
        warn_if_other_accessible("/definitely/not/here.macaroon", "LND admin macaroon");
    }
}
