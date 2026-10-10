//! install.sh: local-tarball install, checksum mismatch and failed attestation are fatal.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const TARGET: &str = "aarch64-apple-darwin"; // any name works: WALKIE_TARGET overrides detection

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// A directory holding a fake release: a tarball with an executable `walkie` and its .sha256.
fn fake_release(dir: &Path) {
    let build = dir.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let bin = build.join("walkie");
    std::fs::write(&bin, "#!/bin/sh\necho fake walkie\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tgz = dir.join(format!("walkie-{TARGET}.tar.gz"));
    assert!(Command::new("tar")
        .args(["-czf"])
        .arg(&tgz)
        .args(["-C"])
        .arg(&build)
        .arg("walkie")
        .status()
        .unwrap()
        .success());
    std::fs::write(
        dir.join(format!("walkie-{TARGET}.tar.gz.sha256")),
        format!("{}  walkie-{TARGET}.tar.gz\n", sha256(&tgz)),
    )
    .unwrap();
}

/// PATH with only the basics, optionally plus a fake `gh` that fails or succeeds.
fn run_install(work: &Path, gh_exit: Option<i32>) -> Output {
    let fakebin = work.join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    // minimal PATH: symlink only the tools install.sh needs, so a real `gh` is never found
    for tool in [
        "sh",
        "cut",
        "shasum",
        "sha256sum",
        "tar",
        "install",
        "mkdir",
        "mktemp",
        "cp",
        "mv",
        "rm",
        "uname",
        "curl",
        "cat",
        "gzip",
    ] {
        if let Ok(p) = which(tool) {
            let _ = std::os::unix::fs::symlink(p, fakebin.join(tool));
        }
    }
    if let Some(code) = gh_exit {
        let gh = fakebin.join("gh");
        std::fs::write(&gh, format!("#!/bin/sh\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Command::new("/bin/sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .current_dir(work)
        .env_clear()
        .env("PATH", &fakebin)
        .env("HOME", work)
        .env("TMPDIR", work)
        .env("WALKIE_TARGET", TARGET)
        .env("WALKIE_INSTALL_DIR", work.join("out"))
        .output()
        .unwrap()
}

fn which(tool: &str) -> Result<std::path::PathBuf, ()> {
    for d in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = Path::new(d).join(tool);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(())
}

#[test]
fn installs_from_a_local_release_download_when_attestation_verifies() {
    let t = tempfile::tempdir().unwrap();
    fake_release(t.path());
    let o = run_install(t.path(), Some(0));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let bin = t.path().join("out/walkie");
    assert!(bin.exists());
    let run = Command::new(bin).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "fake walkie");
}

#[test]
fn checksum_mismatch_is_fatal() {
    let t = tempfile::tempdir().unwrap();
    fake_release(t.path());
    std::fs::write(
        t.path().join(format!("walkie-{TARGET}.tar.gz.sha256")),
        format!("{}  x\n", "0".repeat(64)),
    )
    .unwrap();
    let o = run_install(t.path(), Some(0));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("checksum mismatch"));
    assert!(!t.path().join("out/walkie").exists());
}

#[test]
fn failed_attestation_is_fatal_and_installs_nothing() {
    let t = tempfile::tempdir().unwrap();
    fake_release(t.path());
    let o = run_install(t.path(), Some(1));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("attestation verification FAILED"));
    assert!(!t.path().join("out/walkie").exists());
}

#[test]
fn without_gh_it_warns_and_installs_after_the_checksum() {
    let t = tempfile::tempdir().unwrap();
    fake_release(t.path());
    let o = run_install(t.path(), None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("attestation not verified"));
    assert!(t.path().join("out/walkie").exists());
}
