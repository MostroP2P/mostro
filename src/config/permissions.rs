//! Filesystem permissions of the files mostrod owns or reads, and the
//! primitives that create them owner-only.
//!
//! Kept in its own module — rather than folded into `config::util` — because
//! these are about the files themselves, not about loading the settings.

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
///
/// The contents are `fsync`ed before the rename and the directory after it, so
/// the replacement survives a crash and not only a process exit. Without the
/// second one the rename is atomic but not durable: a power loss right after
/// it can leave the directory entry still pointing at the old file, which for
/// the wizard's `.env` means an `nsec_privkey` the operator was told was
/// saved.
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

    sync_dir(dir);

    Ok(())
}

/// Flush the directory entry the `rename` above just replaced.
///
/// `fsync` on a directory descriptor is the POSIX way to make a rename
/// durable; the data blocks are already on disk from the `sync_all` on the
/// temporary. Windows has no equivalent, and its rename does not need one.
///
/// Failures are logged rather than propagated. The new contents are in place
/// and visible either way — only the durability of the directory entry is in
/// question — so returning an error here would fail a write that succeeded and
/// send the caller into a rollback path with nothing to roll back.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    match fs::File::open(dir).and_then(|dir_file| dir_file.sync_all()) {
        Ok(()) => {}
        Err(e) => tracing::warn!(
            "Wrote the file but could not flush {}: {e}. The contents are in place; only a \
             crash before the filesystem catches up on its own could still lose them.",
            dir.display()
        ),
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

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
        let candidate = dir.join(temp_sibling_name(file_name, attempt));

        match open_owner_only_new(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            // Only a taken name is worth another attempt. An unwritable or
            // missing directory, or a full disk, fails the same way sixteen
            // times over and would be reported as a name collision.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => {
                return Err(MostroInternalErr(ServiceError::IOError(format!(
                    "Could not create a temporary file in {}: {e}",
                    dir.display()
                ))))
            }
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

/// The name `create_temp_sibling` tries for a given attempt: a dotfile next to
/// the target, so a temporary left behind by a killed run is not mistaken for
/// a settings file.
///
/// Shared with the tests, which seed one of these names to exercise the retry
/// and would otherwise silently stop matching if the format changed here.
fn temp_sibling_name(file_name: &OsStr, attempt: u32) -> OsString {
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".tmp-{}-{attempt}", std::process::id()));
    name
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

#[cfg(test)]
mod owner_only_tests {
    use super::*;
    use crate::config::test_support::{assert_mode, set_mode, temp_dir};

    fn temp_root(tag: &str) -> PathBuf {
        temp_dir("owner-only", tag)
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
        std::fs::write(env_file.join("occupied"), b"x").expect("occupy the directory");
        assert!(write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n").is_err());
    }

    #[test]
    fn a_failure_that_is_not_a_name_collision_is_reported_as_itself() {
        let root = temp_root("atomic-missing-dir");
        let env_file = root.join("absent").join(".env");
        let err = write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n")
            .expect_err("a missing directory must not be written to");
        let message = format!("{err:?}");
        assert!(
            !message.contains("attempts"),
            "expected the underlying error, got {message}"
        );
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
    fn atomic_write_replaces_a_planted_symlink_instead_of_following_it() {
        let root = temp_dir("owner-only", "atomic-symlink");
        let victim = victim_in(&root);

        let env_file = root.join(".env");
        std::os::unix::fs::symlink(&victim, &env_file).expect("plant symlink");

        write_owner_only_atomic(&env_file, b"MOSTRO_NSEC_PRIVKEY=nsec1...\n")
            .expect("write env file");

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
        let stale = root.join(temp_sibling_name(OsStr::new(".env"), 0));
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
