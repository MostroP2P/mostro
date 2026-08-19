//! Filesystem permissions of the files mostrod owns or reads: the startup
//! check on the credential and secret files named by the settings, and the
//! primitives that create the settings directory, `settings.toml` and `.env`
//! owner-only.
//!
//! Kept in its own module — rather than folded into `config::util` — because
//! these are about the files themselves, not about loading the settings. Both
//! `config::util` (non-interactive template copy) and `config::wizard` (manual
//! template copy, guided wizard save, `.env` write) bring those files into
//! existence through the primitives here, so a single place decides how they
//! come into being.

use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

/// How many temporary sibling names `write_owner_only_atomic` tries before it
/// gives up. Each attempt only fails when the name is already taken, so a
/// handful is plenty; the bound is there so a directory seeded with every
/// candidate name is an error rather than a hang.
const TEMP_NAME_ATTEMPTS: u32 = 16;

/// Warn when a file holding a secret has any of its "other" permission bits
/// set, which puts it within reach of every account on the host.
///
/// The check is advisory: a node whose macaroon is `0644` still starts, it
/// just says so out loud once per boot. Refusing to start would turn a
/// hardening gap into an outage on the next upgrade for every operator who
/// already runs that way.
///
/// Empty paths and files that cannot be stat'ed are ignored: an unset or
/// unreadable path is not a permissions problem, and the real failure is
/// reported with far more context by whoever opens the file (for the macaroon,
/// `LndConnector::new`). `<settings_dir>/.env` is optional and usually absent,
/// so its being missing has to stay silent.
pub fn warn_if_other_accessible(path: &Path, label: &str) {
    if path.as_os_str().is_empty() {
        return;
    }

    if let Some(mode) = other_accessible_mode(path) {
        // `chmod o=` rather than `chmod 600`: the file may legitimately be
        // owned by another account and read by mostrod through a shared group
        // — the LND macaroon through the node's group is the documented case —
        // and following advice that drops the group bits would leave the
        // daemon unable to authenticate on its next restart.
        tracing::warn!(
            "{label} ({}) has permissions {mode:04o}: its \"other\" bits are set, so every \
             account on this host can reach it. Clear them with: chmod o= {}",
            path.display(),
            path.display()
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

    let mode = fs::metadata(path).ok()?.permissions().mode() & 0o777;
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
/// `0700` applies to the settings directory itself and to nothing else. Any
/// missing parents are created under the umask, the way `mkdir -p` would: with
/// `mostrod -d /srv/apps/mostro/conf` on a fresh tree, only `conf` is closed
/// off, while `/srv/apps` and `/srv/apps/mostro` stay reachable by whatever
/// else lives under them.
///
/// An existing settings directory is returned as it is, so an operator who
/// already set one up with deliberate group access keeps it.
///
/// The parents are created with a recursive `DirBuilder`, which resolves
/// symlinked components on the way. That is deliberate: symlinking a config
/// directory onto another volume is a legitimate setup, and planting a link on
/// the default `~/.mostro` path takes write access to `$HOME`, which already
/// owns the account. A settings directory that is itself a symlink to an
/// existing directory never reaches here — the caller finds it and uses it.
pub(crate) fn create_settings_dir(settings_dir: &Path) -> Result<(), MostroError> {
    if settings_dir.is_dir() {
        return Ok(());
    }

    if let Some(parent) = settings_dir.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            fs::create_dir_all(parent)
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
        }
    }

    // Non-recursive on purpose: the mode below must reach the settings
    // directory and no ancestor, and `create_dir_all` has no way to say that.
    let mut builder = fs::DirBuilder::new();
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
/// A write that fails partway takes the file with it. What is left behind
/// otherwise is a truncated `settings.toml` that the next boot parses and
/// rejects as malformed TOML instead of recreating — the file was created with
/// `O_EXCL` a moment earlier, so removing it cannot touch anything else.
pub(crate) fn create_owner_only(path: &Path, contents: &[u8]) -> Result<(), MostroError> {
    use std::io::Write;

    let mut file = open_owner_only_new(path).map_err(|e| {
        MostroInternalErr(ServiceError::IOError(format!(
            "Could not create {}: {}",
            path.display(),
            e
        )))
    })?;

    let written = file.write_all(contents);
    // Closed before the cleanup below, which Windows would refuse on an open
    // handle.
    drop(file);

    written.map_err(|e| {
        let _ = fs::remove_file(path);
        MostroInternalErr(ServiceError::IOError(format!(
            "Could not write {}: {}",
            path.display(),
            e
        )))
    })
}

/// Replace `path` with `contents`, owner-only (`0600` on Unix), atomically.
///
/// For the files mostrod rewrites rather than creates once — today
/// `<settings_dir>/.env`, which carries the same `nsec_privkey` as
/// `settings.toml`. [`create_owner_only`] cannot serve them: it refuses a path
/// that already exists, which is exactly what a rewrite has to do.
///
/// The contents go to a fresh temporary file in the same directory, created
/// with `O_CREAT | O_EXCL` and chmod'ed through its descriptor, and are then
/// moved onto `path` with `rename`. That buys two things at once:
///
/// - `rename` replaces whatever `path` names without ever opening it, so a
///   symlink another local account planted there is unlinked rather than
///   written through — its target keeps both its contents and its mode.
///   `create_owner_only` refuses in that situation; here refusing is not an
///   option, and replacing the link gives the target the same protection. A
///   deliberately symlinked `.env` is not a supported setup: it is a file the
///   daemon writes, in a directory it created `0700`.
/// - The file at `path` is never observed half-written. A `.env` truncated by
///   a full disk would otherwise leave the daemon with no `nsec_privkey` at
///   all on the next boot.
pub(crate) fn write_owner_only_atomic(path: &Path, contents: &[u8]) -> Result<(), MostroError> {
    use std::io::Write;

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        MostroInternalErr(ServiceError::IOError(format!(
            "{} does not name a file",
            path.display()
        )))
    })?;

    let (temp_path, mut temp_file) = create_temp_sibling(dir, file_name)?;

    // The temporary is only ever left behind on a failure, and never with the
    // secret still in it.
    let staged = temp_file
        .write_all(contents)
        .and_then(|()| temp_file.sync_all());
    drop(temp_file);

    if let Err(e) = staged.and_then(|()| fs::rename(&temp_path, path)) {
        let _ = fs::remove_file(&temp_path);
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "Could not write {}: {}",
            path.display(),
            e
        ))));
    }

    Ok(())
}

/// Create an owner-only temporary file next to the target and return it with
/// its path.
///
/// `O_EXCL` again, so a stale temporary left behind by a killed run — or one
/// planted deliberately — is never written through; the suffix is bumped until
/// a free name is found. The temporary is a sibling rather than something
/// under `/tmp` because `rename` only works within a filesystem, and because
/// the settings directory is already `0700`.
fn create_temp_sibling(dir: &Path, file_name: &OsStr) -> Result<(PathBuf, fs::File), MostroError> {
    let mut last_error = None;

    for attempt in 0..TEMP_NAME_ATTEMPTS {
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".tmp-{}-{attempt}", std::process::id()));
        let candidate = dir.join(name);

        match open_owner_only_new(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) => last_error = Some(e),
        }
    }

    Err(MostroInternalErr(ServiceError::IOError(format!(
        "Could not create a temporary file in {} after {TEMP_NAME_ATTEMPTS} attempts: {}",
        dir.display(),
        last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))))
}

/// Open a brand-new file owner-only, failing if anything already occupies the
/// path.
///
/// `OpenOptionsExt::mode` is masked by the process umask, so the mode is set
/// again through the file descriptor — never through the path, which would
/// reintroduce the symlink `create_new` just refused to follow.
fn open_owner_only_new(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::test_support::temp_dir;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn macaroon_with_mode(tag: &str, mode: u32) -> PathBuf {
        let path = temp_dir("permissions", tag).join("admin.macaroon");
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
        let dir = temp_dir("permissions", "symlink");
        let target = dir.join("real.macaroon");
        std::fs::write(&target, b"macaroon").expect("write macaroon");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("set permissions");

        let link = dir.join("linked.macaroon");
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
        warn_if_other_accessible(Path::new(""), "LND admin macaroon");
        warn_if_other_accessible(
            Path::new("/definitely/not/here.macaroon"),
            "Mostro env file",
        );
    }
}

#[cfg(test)]
mod owner_only_tests {
    use super::*;
    use crate::config::test_support::{assert_mode, mode_of, set_mode, temp_dir};

    fn temp_root(tag: &str) -> PathBuf {
        temp_dir("owner-only", tag)
    }

    #[test]
    fn settings_dir_is_created_owner_only() {
        let root = temp_root("dir");
        let settings_dir = root.join(".mostro");
        create_settings_dir(&settings_dir).expect("create settings dir");
        assert_mode(&settings_dir, 0o700);
    }

    #[test]
    fn settings_dir_creation_closes_off_the_leaf_but_not_its_ancestors() {
        let root = temp_root("dir-nested");
        let settings_dir = root.join("apps").join("mostro").join("conf");
        create_settings_dir(&settings_dir).expect("create nested settings dir");
        assert_mode(&settings_dir, 0o700);

        // The parents `mkdir -p` had to invent keep whatever the umask gives
        // them, so `mostrod -d /srv/apps/mostro/conf` does not close off
        // `/srv/apps` for everything else that lives under it. Compared
        // against a reference tree rather than a literal mode, because the
        // umask is the environment's to choose.
        let reference = root.join("reference").join("inner");
        std::fs::create_dir_all(&reference).expect("create reference tree");
        assert_eq!(
            mode_of(&root.join("apps")),
            mode_of(&root.join("reference"))
        );
        assert_eq!(
            mode_of(&root.join("apps").join("mostro")),
            mode_of(&reference)
        );
    }

    #[test]
    fn settings_dir_creation_leaves_an_existing_directory_alone() {
        let root = temp_root("dir-existing");
        let settings_dir = root.join(".mostro");
        std::fs::create_dir(&settings_dir).expect("create settings dir");
        set_mode(&settings_dir, 0o750);
        // A deliberate group-readable directory must survive this rather than
        // be tightened or reported as an error.
        create_settings_dir(&settings_dir).expect("existing directory is not an error");
        assert_mode(&settings_dir, 0o750);
    }

    #[test]
    fn template_is_written_owner_only() {
        let root = temp_root("file");
        let config_file = root.join("settings.toml");
        create_owner_only(&config_file, b"nsec_privkey = 'nsec1...'\n").expect("write template");
        assert_mode(&config_file, 0o600);
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
        set_mode(&config_file, 0o644);
        assert!(create_owner_only(&config_file, b"fresh\n").is_err());
        // Neither the contents nor the mode of what was already there change.
        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read back"),
            "operator contents"
        );
        assert_mode(&config_file, 0o644);
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

    #[test]
    fn atomic_write_creates_a_missing_file_owner_only() {
        let root = temp_root("atomic-new");
        let env_file = root.join(".env");
        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n")
            .expect("write env file");
        assert_eq!(
            std::fs::read_to_string(&env_file).expect("read back"),
            "MOSTRO_NSEC_PRIVKEY=nsec1...\n"
        );
        assert_mode(&env_file, 0o600);
    }

    #[test]
    fn atomic_write_replaces_an_existing_file_and_tightens_its_mode() {
        let root = temp_root("atomic-existing");
        let env_file = root.join(".env");
        std::fs::write(&env_file, "OLD=stale\n").expect("seed env file");
        set_mode(&env_file, 0o644);

        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1replaced\n")
            .expect("rewrite env file");

        assert_eq!(
            std::fs::read_to_string(&env_file).expect("read back"),
            "MOSTRO_NSEC_PRIVKEY=nsec1replaced\n"
        );
        assert_mode(&env_file, 0o600);
    }

    #[test]
    fn atomic_write_leaves_no_temporary_behind() {
        let root = temp_root("atomic-clean");
        let env_file = root.join(".env");
        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n").expect("write");

        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .expect("read dir")
            .map(|entry| entry.expect("dir entry").file_name())
            .filter(|name| name != ".env")
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temporary must be renamed away, found {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_to_an_unwritable_path_is_an_error() {
        let root = temp_root("atomic-error");
        let env_file = root.join(".env");
        std::fs::create_dir(&env_file).expect("create dir in the file's place");
        // `rename` cannot replace a non-empty directory, so this must not
        // report success.
        std::fs::write(env_file.join("occupied"), b"x").expect("occupy the directory");
        assert!(write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n").is_err());
    }
}

#[cfg(all(test, unix))]
mod symlink_tests {
    use super::*;
    use crate::config::test_support::{assert_mode, set_mode, temp_dir};

    fn victim_in(root: &Path) -> PathBuf {
        let victim = root.join("victim");
        std::fs::write(&victim, "victim contents").expect("seed victim");
        set_mode(&victim, 0o644);
        victim
    }

    #[test]
    fn a_symlink_in_the_settings_path_is_refused_and_its_target_untouched() {
        let root = temp_dir("owner-only", "file-symlink");
        let victim = victim_in(&root);

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
        assert_mode(&victim, 0o644);
        // The symlink itself is left in place; nothing was written through it.
        assert!(std::fs::symlink_metadata(&config_file)
            .expect("symlink metadata")
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_dangling_symlink_is_refused_rather_than_created_through() {
        let root = temp_dir("owner-only", "file-dangling");
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
    fn atomic_write_replaces_a_planted_symlink_instead_of_following_it() {
        let root = temp_dir("owner-only", "atomic-symlink");
        let victim = victim_in(&root);

        let env_file = root.join(".env");
        std::os::unix::fs::symlink(&victim, &env_file).expect("plant symlink");

        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n")
            .expect("write env file");

        // The nsec landed in a regular file that replaced the link, and the
        // target kept both its contents and its mode.
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read victim"),
            "victim contents"
        );
        assert_mode(&victim, 0o644);
        assert!(std::fs::symlink_metadata(&env_file)
            .expect("symlink metadata")
            .file_type()
            .is_file());
        assert_mode(&env_file, 0o600);
    }

    #[test]
    fn atomic_write_steps_over_a_stale_temporary() {
        let root = temp_dir("owner-only", "atomic-stale");
        let env_file = root.join(".env");
        // What a run killed mid-write leaves behind. Writing through it would
        // be harmless here, but the same name could just as well be a symlink
        // another account planted, so the first free suffix is used instead.
        let stale = root.join(format!(".env.tmp-{}-0", std::process::id()));
        std::fs::write(&stale, "stale").expect("seed stale temporary");

        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n").expect("write");

        assert_eq!(
            std::fs::read_to_string(&env_file).expect("read back"),
            "MOSTRO_NSEC_PRIVKEY=nsec1...\n"
        );
        assert_eq!(
            std::fs::read_to_string(&stale).expect("read stale"),
            "stale",
            "the stale temporary must not be written through"
        );
    }
}
