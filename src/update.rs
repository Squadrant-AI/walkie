//! Update check (at most once per 24 h, cached) and `walkie update`.
//! Network goes through the system `curl`; every failure in the check is silent.
use crate::exit::{fail, Exit};
use crate::paths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const RELEASES: &str = "https://github.com/Squadrant-AI/walkie/releases";
const DOWNLOAD: &str = "https://github.com/Squadrant-AI/walkie/releases/latest/download";
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Cache {
    checked_at: u64,
    latest: Option<String>,
}

/// `X.Y.Z` (optionally `v`-prefixed; a `-pre`/`+build` suffix is ignored) as a comparable triple.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let v = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(v)
}

fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

fn cache_fresh(checked_at: u64, now: u64) -> bool {
    now.saturating_sub(checked_at) < CHECK_EVERY_SECS && checked_at <= now
}

/// Whether the check runs at all. `skip_cmd`: daemon, wait, update.
fn should_check(skip_cmd: bool, no_check: Option<&str>, ci: Option<&str>) -> bool {
    !skip_cmd && no_check != Some("1") && ci.is_none_or(str::is_empty)
}

#[derive(Debug, PartialEq)]
enum Notice {
    Prompt,
    OneLine,
}

fn notice_mode(stdin_tty: bool, stderr_tty: bool) -> Notice {
    if stdin_tty && stderr_tty {
        Notice::Prompt
    } else {
        Notice::OneLine
    }
}

/// `…/releases/tag/v0.1.2` → `0.1.2`
fn version_from_redirect(url: &str) -> Option<String> {
    let v = url.trim().rsplit_once("/tag/")?.1.trim_start_matches('v');
    parse_version(v).map(|_| v.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn fetch_latest() -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "-m", "2", "-o", "/dev/null", "-w", "%{redirect_url}"])
        .arg(format!("{RELEASES}/latest"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    version_from_redirect(&String::from_utf8_lossy(&out.stdout))
}

fn cache_path() -> std::path::PathBuf {
    paths::walkie_home().join("update-check.json")
}

fn write_cache(c: &Cache) -> Result<()> {
    paths::ensure_private_dir(&paths::walkie_home())?;
    let p = cache_path();
    std::fs::write(&p, serde_json::to_vec(c)?)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Latest known version, hitting the network only when the cache is older than 24 h.
/// A failed lookup keeps the previous answer and still counts as a check (backs off).
fn latest_known() -> Option<String> {
    let cached: Option<Cache> = std::fs::read(cache_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(c) = &cached {
        if cache_fresh(c.checked_at, now()) {
            return c.latest.clone();
        }
    }
    let latest = fetch_latest().or_else(|| cached.and_then(|c| c.latest));
    let _ = write_cache(&Cache {
        checked_at: now(),
        latest: latest.clone(),
    });
    latest
}

/// Called after a command finishes. Never fails, never touches stdout.
pub fn maybe_notify(skip_cmd: bool) {
    let env = |k| std::env::var(k).ok();
    if !should_check(
        skip_cmd,
        env("WALKIE_NO_UPDATE_CHECK").as_deref(),
        env("CI").as_deref(),
    ) {
        return;
    }
    let Some(latest) = latest_known().filter(|l| is_newer(l, CURRENT)) else {
        return;
    };
    let (si, se) = (
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    );
    match notice_mode(si, se) {
        Notice::OneLine => eprintln!("walkie: {latest} is available — run `walkie update`"),
        Notice::Prompt => {
            eprint!("walkie {latest} is available (you have {CURRENT}). Update now? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_ok()
                && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
            {
                if let Err(e) = run() {
                    eprintln!("walkie: update failed: {e:#}");
                }
            }
        }
    }
}

fn dir_writable(dir: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(dir.as_os_str().as_bytes())
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// `walkie update`: fetch the release's install.sh and run it, replacing this executable in place.
/// `WALKIE_UPDATE_BASE` overrides the download base (tests use a local `file://` release dir).
pub fn run() -> Result<Exit> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe
        .parent()
        .ok_or_else(|| fail(Exit::Environment, "cannot locate the walkie executable"))?;
    if !dir_writable(dir) {
        return Err(fail(
            Exit::Environment,
            format!(
                "{} is not writable, so walkie cannot update itself there; re-run the installer \
                 with WALKIE_INSTALL_DIR set, or use sudo",
                dir.display()
            ),
        ));
    }
    let base = std::env::var("WALKIE_UPDATE_BASE").unwrap_or_else(|_| DOWNLOAD.to_string());
    let tmp = tempfile::tempdir_in(dir)
        .map_err(|e| fail(Exit::Environment, format!("cannot create a temp dir: {e}")))?;
    let script = tmp.path().join("install.sh");
    let ok = Command::new("curl")
        .args(["-fsSL", "-m", "30", "-o"])
        .arg(&script)
        .arg(format!("{base}/install.sh"))
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return Err(fail(
            Exit::Environment,
            "could not download install.sh (no network?)",
        ));
    }
    let status = Command::new("sh")
        .arg(&script)
        .current_dir(tmp.path())
        .env("WALKIE_INSTALL_DIR", dir)
        .env("WALKIE_BASE_URL", &base)
        .status()?;
    if !status.success() {
        return Err(fail(
            Exit::Environment,
            "the installer failed; walkie was not updated",
        ));
    }
    let new = Command::new(&exe)
        .arg("--version")
        .env("WALKIE_NO_UPDATE_CHECK", "1")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .last()
                .map(str::to_string)
        })
        .filter(|v| parse_version(v).is_some());
    match new {
        Some(n) => println!("walkie updated {CURRENT} → {n}"),
        None => println!("walkie updated (was {CURRENT})"),
    }
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.10.0", "0.9.0"), "numeric, not lexical");
        assert!(!is_newer("garbage", "0.1.1"));
        assert!(!is_newer("0.1", "0.1.1"));
        assert_eq!(parse_version("0.1.2-rc1"), Some((0, 1, 2)));
    }

    #[test]
    fn redirect_parsing() {
        assert_eq!(
            version_from_redirect("https://github.com/Squadrant-AI/walkie/releases/tag/v0.1.2\n"),
            Some("0.1.2".into())
        );
        assert_eq!(version_from_redirect(""), None);
        assert_eq!(version_from_redirect("https://github.com/x/releases"), None);
    }

    #[test]
    fn cache_is_fresh_for_24h() {
        let t = 1_000_000;
        assert!(cache_fresh(t, t));
        assert!(cache_fresh(t, t + CHECK_EVERY_SECS - 1));
        assert!(!cache_fresh(t, t + CHECK_EVERY_SECS));
        assert!(
            !cache_fresh(t + 10, t),
            "a timestamp from the future is stale"
        );
    }

    #[test]
    fn tty_decision() {
        assert_eq!(notice_mode(true, true), Notice::Prompt);
        assert_eq!(notice_mode(false, true), Notice::OneLine);
        assert_eq!(notice_mode(true, false), Notice::OneLine);
        assert_eq!(notice_mode(false, false), Notice::OneLine);
    }

    #[test]
    fn skip_rules() {
        assert!(should_check(false, None, None));
        assert!(!should_check(true, None, None), "daemon/wait/update");
        assert!(!should_check(false, Some("1"), None));
        assert!(!should_check(false, None, Some("true")));
        assert!(should_check(false, None, Some("")), "empty CI is unset");
        assert!(should_check(false, Some("0"), None));
    }

    #[test]
    fn cache_roundtrip_is_0600() {
        let env = crate::paths::test_support::IsolatedEnv::new();
        write_cache(&Cache {
            checked_at: 5,
            latest: Some("1.2.3".into()),
        })
        .unwrap();
        let p = env.dir.path().join("home/update-check.json");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let c: Cache = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            c,
            Cache {
                checked_at: 5,
                latest: Some("1.2.3".into())
            }
        );
    }
}
