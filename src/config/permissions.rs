//! Filesystem permissions of the files mostrod owns or reads: the startup
//! check on the credential files named by the settings, and the primitives
//! that create the settings directory and `settings.toml` owner-only.
//!
//! Kept in its own module — rather than folded into `config::util` — because
//! these are about the files themselves, not about loading the settings. Both
//! `config::util` (non-interactive template copy) and `config::wizard` (manual
//! template copy, guided wizard save) create `settings.toml` through the same
//! primitive here, so a single place decides how that file comes into being.

use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use std::fs;
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

/// Create the settings directory owner-only (`0700` on Unix).
///
/// The directory holds `settings.toml` with a plaintext `nsec_privkey`, the
/// `mostro.db` database and, in the Docker flows, the LND credentials under
/// `lnd/`. `create_dir_all` would apply the process umask instead, which on a
/// typical host leaves the directory at `0755`.
///
/// Only directories this call creates are affected: an operator who already set
/// up the directory with deliberate group access keeps it.
pub(crate) fn create_settings_dir(settings_dir: &Path) -> Result<(), MostroError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(settings_dir)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))
}

/// Create `path` and write `contents` to it with owner-only permissions
/// (`0600` on Unix). Fails if anything already exists at `path`.
///
/// Every path that brings an initial `settings.toml` into existence goes
/// through here, so the file that later carries `nsec_privkey` is never left
/// at the process umask and never reached through a symlink.
///
/// `create_new` maps to `O_CREAT | O_EXCL`, which POSIX requires to fail with
/// `EEXIST` when the path names a symbolic link, whatever it points at. Callers
/// only reach this after finding no settings file, but that check and this call
/// cannot be one operation: on a settings directory another local account can
/// write to, a symlink planted in between would otherwise have its target
/// truncated and its mode reset to `0600` by the two steps below.
///
/// `OpenOptionsExt::mode` is also masked by the process umask, so the mode is
/// set again through the file descriptor — never through the path, which would
/// reintroduce the symlink it just refused to follow.
pub(crate) fn create_owner_only(path: &Path, contents: &[u8]) -> Result<(), MostroError> {
    use std::io::Write;

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    let mut file = file.map_err(|e| {
        MostroInternalErr(ServiceError::IOError(format!(
            "Could not create {}: {}",
            path.display(),
            e
        )))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    }

    file.write_all(contents)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))
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

#[cfg(all(test, unix))]
mod owner_only_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-owner-only-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn settings_dir_is_created_owner_only() {
        let root = temp_root("dir");
        let settings_dir = root.join(".mostro");
        create_settings_dir(&settings_dir).expect("create settings dir");
        assert_eq!(mode_of(&settings_dir), 0o700);
    }

    #[test]
    fn settings_dir_creation_is_recursive_and_owner_only() {
        let root = temp_root("dir-nested");
        let settings_dir = root.join("nested").join(".mostro");
        create_settings_dir(&settings_dir).expect("create nested settings dir");
        assert_eq!(mode_of(&settings_dir), 0o700);
        assert_eq!(mode_of(&root.join("nested")), 0o700);
    }

    #[test]
    fn settings_dir_creation_leaves_an_existing_directory_alone() {
        let root = temp_root("dir-existing");
        let settings_dir = root.join(".mostro");
        std::fs::create_dir(&settings_dir).expect("create settings dir");
        std::fs::set_permissions(&settings_dir, std::fs::Permissions::from_mode(0o750))
            .expect("loosen permissions");
        // `recursive(true)` makes this a no-op rather than an error, and a
        // deliberate group-readable directory must survive it.
        create_settings_dir(&settings_dir).expect("existing directory is not an error");
        assert_eq!(mode_of(&settings_dir), 0o750);
    }

    #[test]
    fn template_is_written_owner_only() {
        let root = temp_root("file");
        let config_file = root.join("settings.toml");
        create_owner_only(&config_file, b"nsec_privkey = 'nsec1...'\n").expect("write template");
        assert_eq!(mode_of(&config_file), 0o600);
        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read back"),
            "nsec_privkey = 'nsec1...'\n"
        );
    }

    #[test]
    fn a_preexisting_file_is_refused_instead_of_truncated() {
        let root = temp_root("file-existing");
        let config_file = root.join("settings.toml");
        std::fs::write(&config_file, "operator contents").expect("seed file");
        std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o644))
            .expect("loosen permissions");
        assert!(create_owner_only(&config_file, b"fresh\n").is_err());
        // Neither the contents nor the mode of what was already there change.
        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read back"),
            "operator contents"
        );
        assert_eq!(mode_of(&config_file), 0o644);
    }

    #[test]
    fn a_symlink_in_the_settings_path_is_refused_and_its_target_untouched() {
        let root = temp_root("file-symlink");
        let victim = root.join("victim");
        std::fs::write(&victim, "victim contents").expect("seed victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644))
            .expect("set victim mode");

        // What another local account could plant in a settings directory it can
        // write to, in the window between the caller's existence check and this
        // call. Following it would truncate the victim and reset it to 0600.
        let config_file = root.join("settings.toml");
        std::os::unix::fs::symlink(&victim, &config_file).expect("plant symlink");

        assert!(create_owner_only(&config_file, b"template\n").is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read back"),
            "victim contents"
        );
        assert_eq!(mode_of(&victim), 0o644);
        // The symlink itself is left in place; nothing was written through it.
        assert!(std::fs::symlink_metadata(&config_file)
            .expect("symlink metadata")
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_dangling_symlink_is_refused_rather_than_created_through() {
        let root = temp_root("file-dangling");
        let config_file = root.join("settings.toml");
        // `Path::exists` follows symlinks, so the caller's check reports false
        // here and this call is what has to refuse.
        std::os::unix::fs::symlink(root.join("does-not-exist"), &config_file)
            .expect("plant dangling symlink");
        assert!(!config_file.exists());
        assert!(create_owner_only(&config_file, b"template\n").is_err());
        assert!(!root.join("does-not-exist").exists());
    }

    #[test]
    fn writing_to_an_unwritable_path_is_an_error() {
        let root = temp_root("file-error");
        // A directory cannot be opened for writing, so this exercises the
        // error branch instead of silently succeeding.
        let config_file = root.join("settings.toml");
        std::fs::create_dir(&config_file).expect("create dir in the file's place");
        assert!(create_owner_only(&config_file, b"x").is_err());
    }
}
