use crate::exit::{fail, Exit};
use anyhow::Result;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `sun_path` is 104 bytes on macOS (108 on Linux); stay under the smaller one.
const SUN_PATH_MAX: usize = 104;

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn walkie_home() -> PathBuf {
    if let Ok(h) = std::env::var("WALKIE_HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".walkie")
}

pub fn ensure_private_dir(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p).is_err() {
        std::fs::create_dir_all(p).map_err(|e| {
            fail(
                Exit::Environment,
                format!("cannot create {}: {e}", p.display()),
            )
        })?;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    let md = std::fs::symlink_metadata(p)?;
    if md.file_type().is_symlink() {
        return Err(fail(
            Exit::Environment,
            format!("{} is a symlink; refusing", p.display()),
        ));
    }
    if md.uid() != uid() {
        return Err(fail(
            Exit::Environment,
            format!("{} is owned by another user; refusing", p.display()),
        ));
    }
    if md.mode() & 0o077 != 0 {
        return Err(fail(
            Exit::Environment,
            format!("{} must be mode 0700", p.display()),
        ));
    }
    Ok(())
}

pub fn rooms_dir() -> Result<PathBuf> {
    let home = walkie_home();
    ensure_private_dir(&home)?;
    let d = home.join("rooms");
    ensure_private_dir(&d)?;
    Ok(d)
}

/// Where sockets live: `<base>/walkie/`, where base is `$XDG_RUNTIME_DIR`, or on macOS
/// `$TMPDIR`, or `/tmp/walkie-<uid>` (preflight D11). Returns `(base, dir)`; `base` is
/// only safety-checked for the shared-`/tmp` fallback, where it is the trust anchor.
fn resolve_runtime_dir(
    env: impl Fn(&str) -> Option<String>,
    macos: bool,
    uid: u32,
) -> (Option<PathBuf>, PathBuf) {
    if let Some(d) = env("WALKIE_RUNTIME_DIR") {
        // explicit override (tests): used as the final directory
        return (None, PathBuf::from(d));
    }
    if let Some(x) = env("XDG_RUNTIME_DIR") {
        return (None, PathBuf::from(x).join("walkie"));
    }
    if macos {
        if let Some(t) = env("TMPDIR") {
            return (None, PathBuf::from(t).join("walkie"));
        }
    }
    let base = PathBuf::from(format!("/tmp/walkie-{uid}"));
    (Some(base.clone()), base.join("walkie"))
}

pub fn runtime_dir() -> Result<PathBuf> {
    let (base, d) =
        resolve_runtime_dir(|k| std::env::var(k).ok(), cfg!(target_os = "macos"), uid());
    if let Some(b) = base {
        ensure_private_dir(&b)?;
    }
    ensure_private_dir(&d)?;
    Ok(d)
}

/// Rooms that currently have a socket file (a live daemon, or a stale socket).
pub fn socket_rooms() -> Vec<String> {
    let Ok(dir) = runtime_dir() else {
        return vec![];
    };
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".sock")
                .map(str::to_string)
        })
        .collect();
    v.sort();
    v
}

pub fn socket_path(room: &str) -> Result<PathBuf> {
    let p = runtime_dir()?.join(format!("{room}.sock"));
    if p.as_os_str().len() >= SUN_PATH_MAX {
        return Err(fail(
            Exit::Environment,
            format!(
                "socket path too long ({} bytes, max {}): {}; set a shorter $TMPDIR",
                p.as_os_str().len(),
                SUN_PATH_MAX - 1,
                p.display()
            ),
        ));
    }
    Ok(p)
}
pub fn ticket_path(room: &str) -> Result<PathBuf> {
    Ok(rooms_dir()?.join(format!("{room}.ticket")))
}
/// Quarantine for files the peer sends: `<home>/rooms/<room>/inbox` (created on first file).
pub fn inbox_dir(room: &str) -> Result<PathBuf> {
    Ok(rooms_dir()?.join(room).join("inbox"))
}
pub fn log_path(room: &str) -> Result<PathBuf> {
    Ok(rooms_dir()?.join(format!("{room}.log")))
}
pub fn pid_path(room: &str) -> Result<PathBuf> {
    Ok(rooms_dir()?.join(format!("{room}.pid")))
}
pub fn tombstone_path(room: &str) -> Result<PathBuf> {
    Ok(rooms_dir()?.join(format!("{room}.ended")))
}

/// Tests that touch process-global env (WALKIE_HOME, WALKIE_RUNTIME_DIR) take this lock and
/// point everything at a tempdir, so they neither race nor reach the real `~/.walkie`
/// (preflight D2).
#[cfg(test)]
pub mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct IsolatedEnv {
        _guard: MutexGuard<'static, ()>,
        pub dir: tempfile::TempDir,
    }

    impl IsolatedEnv {
        pub fn new() -> IsolatedEnv {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("WALKIE_HOME", dir.path().join("home"));
            std::env::set_var("WALKIE_RUNTIME_DIR", dir.path().join("rt"));
            IsolatedEnv { _guard: guard, dir }
        }
    }

    impl Drop for IsolatedEnv {
        fn drop(&mut self) {
            std::env::remove_var("WALKIE_HOME");
            std::env::remove_var("WALKIE_RUNTIME_DIR");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::IsolatedEnv;
    use super::*;
    use crate::exit::{exit_of, Exit};
    use std::collections::HashMap;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn creates_private_dir_0700() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("rt");
        ensure_private_dir(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn refuses_symlinked_or_open_runtime_dir() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = t.path().join("link");
        symlink(&real, &link).unwrap();
        assert_eq!(
            exit_of(&ensure_private_dir(&link).unwrap_err()),
            Exit::Environment
        );

        let open = t.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            exit_of(&ensure_private_dir(&open).unwrap_err()),
            Exit::Environment
        );
    }

    #[test]
    fn runtime_dir_resolution_always_ends_in_walkie() {
        // preflight D11: <runtime-dir>/walkie/<room>.sock in every case
        let (b, d) = resolve_runtime_dir(env_of(&[("XDG_RUNTIME_DIR", "/run/user/1")]), false, 1);
        assert_eq!((b, d), (None, PathBuf::from("/run/user/1/walkie")));
        let (b, d) = resolve_runtime_dir(env_of(&[("TMPDIR", "/var/T/")]), true, 1);
        assert_eq!((b, d), (None, PathBuf::from("/var/T/walkie")));
        // TMPDIR is ignored off macOS
        let (b, d) = resolve_runtime_dir(env_of(&[("TMPDIR", "/var/T/")]), false, 501);
        assert_eq!(b, Some(PathBuf::from("/tmp/walkie-501")));
        assert_eq!(d, PathBuf::from("/tmp/walkie-501/walkie"));
    }

    #[test]
    fn real_macos_tmpdir_socket_path_fits_sun_path() {
        // the TMPDIR length measured on the dev machine (Task 0 spike): 49 bytes
        let tmpdir = "/var/folders/rv/y35yc8ns24g7js2zg0mtzs3r0000gn/T/";
        let (_, d) = resolve_runtime_dir(env_of(&[("TMPDIR", tmpdir)]), true, 501);
        let p = d.join("abcdef.sock");
        assert!(p.as_os_str().len() < SUN_PATH_MAX, "{}", p.display());
    }

    #[test]
    fn overlong_socket_path_is_refused_with_exit_8() {
        let env = IsolatedEnv::new();
        let long = env.dir.path().join("x".repeat(100));
        std::env::set_var("WALKIE_RUNTIME_DIR", long);
        assert_eq!(
            exit_of(&socket_path("abcdef").unwrap_err()),
            Exit::Environment
        );
    }

    #[test]
    fn socket_path_is_created_private_under_the_runtime_dir() {
        let env = IsolatedEnv::new();
        let p = socket_path("abcdef").unwrap();
        assert_eq!(p, env.dir.path().join("rt").join("abcdef.sock"));
        assert_eq!(
            std::fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
