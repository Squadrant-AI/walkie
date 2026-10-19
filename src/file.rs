//! Native file transfer: sender-side checks, name sanitizing, and the receiving side that
//! streams chunks into a quarantine inbox. A peer never chooses where bytes land: it supplies a
//! name, we keep only a sanitized basename and create the file exclusively (never overwriting)
//! inside `<home>/rooms/<room>/inbox/`.

use crate::exit::{fail, Exit};
use crate::frame::sanitize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Per-file limit, separate from (and much larger than) the 16 KiB text limit.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Raw bytes per wire chunk; base64 makes it 64 KiB, well under `proto::MAX_FRAME`.
pub const CHUNK_BYTES: usize = 48 * 1024;
const NAME_MAX_BYTES: usize = 200;
const PART_PREFIX: &str = ".part-";

/// Reduces a peer-supplied (or local) file name to a safe basename: no path separators, no
/// control / bidi / zero-width characters, never empty, `.` or `..`, never hidden.
pub fn sanitize_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = sanitize(base).chars().filter(|c| !c.is_control()).collect();
    let mut name = cleaned.trim().trim_start_matches('.').to_string();
    while name.len() > NAME_MAX_BYTES {
        name.pop();
    }
    if name.is_empty() {
        "file".to_string()
    } else {
        name
    }
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn file_sha(p: &Path) -> Option<String> {
    let mut data = Vec::new();
    std::fs::File::open(p)
        .ok()?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    Some(sha256_hex(&data))
}

pub struct Outgoing {
    pub name: String,
    pub data: Vec<u8>,
    pub sha256: String,
}

/// Cheap pre-flight (no content read): regular file within the size limit. Returns the size.
pub fn check_sendable(path: &Path) -> anyhow::Result<u64> {
    let md = std::fs::metadata(path)
        .map_err(|e| fail(Exit::Error, format!("cannot read {}: {e}", path.display())))?;
    if !md.is_file() {
        return Err(fail(
            Exit::Error,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if md.len() > MAX_FILE_BYTES {
        return Err(fail(
            Exit::Error,
            format!(
                "{} is too large ({} bytes, max {} MiB)",
                path.display(),
                md.len(),
                MAX_FILE_BYTES >> 20
            ),
        ));
    }
    Ok(md.len())
}

pub fn load_outgoing(path: &Path) -> anyhow::Result<Outgoing> {
    check_sendable(path)?;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut data))
        .map_err(|e| fail(Exit::Error, format!("cannot read {}: {e}", path.display())))?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(fail(
            Exit::Error,
            format!("{} grew past the size limit while reading", path.display()),
        ));
    }
    let name = sanitize_name(&path.file_name().unwrap_or_default().to_string_lossy());
    let sha256 = sha256_hex(&data);
    Ok(Outgoing { name, data, sha256 })
}

/// One wire chunk as the receiver sees it.
pub struct Chunk<'a> {
    pub xfer: u64,
    pub idx: u32,
    pub last: bool,
    pub name: &'a str,
    pub size: u64,
    pub sha256: &'a str,
    pub data: &'a [u8],
}

/// Outcome of a finished transfer. `path` is `None` when nothing was saved; then `problem` says why.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub name: String,
    pub size: u64,
    /// Computed by the receiver over the bytes it wrote (`-` when nothing was written).
    pub sha256: String,
    pub path: Option<PathBuf>,
    pub problem: Option<String>,
}

struct Active {
    xfer: u64,
    next_idx: u32,
    name: String,
    size: u64,
    declared_sha: String,
    written: u64,
    hasher: Sha256,
    tmp: Option<(std::fs::File, PathBuf)>,
    failed: Option<String>,
}

pub struct Receiver {
    dir: PathBuf,
    ready: bool,
    active: Option<Active>,
}

impl Receiver {
    pub fn new(dir: PathBuf) -> Receiver {
        Receiver {
            dir,
            ready: false,
            active: None,
        }
    }

    /// Creates `<room>/inbox` (each level private) and clears partials a dead daemon left behind.
    fn ensure_dir(&mut self) -> anyhow::Result<()> {
        if self.ready {
            return Ok(());
        }
        if let Some(parent) = self.dir.parent() {
            crate::paths::ensure_private_dir(parent)?;
        }
        crate::paths::ensure_private_dir(&self.dir)?;
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            if e.file_name().to_string_lossy().starts_with(PART_PREFIX) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.ready = true;
        Ok(())
    }

    fn discard(&mut self) {
        if let Some(a) = self.active.take() {
            if let Some((_, p)) = a.tmp {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn start(&mut self, c: &Chunk) -> Active {
        let mut a = Active {
            xfer: c.xfer,
            next_idx: 0,
            name: sanitize_name(c.name),
            size: c.size,
            declared_sha: c.sha256.to_ascii_lowercase(),
            written: 0,
            hasher: Sha256::new(),
            tmp: None,
            failed: None,
        };
        if c.size > MAX_FILE_BYTES {
            a.failed = Some(format!(
                "file exceeds the {} MiB limit ({} bytes declared)",
                MAX_FILE_BYTES >> 20,
                c.size
            ));
        } else if !is_sha256_hex(c.sha256) {
            a.failed = Some("sender gave no valid sha256".into());
        } else {
            let tmp = self.dir.join(format!(
                "{PART_PREFIX}{}-{:016x}",
                c.xfer,
                rand::random::<u64>()
            ));
            let opened = self.ensure_dir().and_then(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)
                    .map_err(Into::into)
            });
            match opened {
                Ok(f) => a.tmp = Some((f, tmp)),
                Err(e) => a.failed = Some(format!("cannot write to the inbox: {e:#}")),
            }
        }
        a
    }

    /// Feeds one in-order chunk. Returns the result when the transfer's last chunk arrives.
    pub fn on_chunk(&mut self, c: &Chunk) -> Option<Received> {
        if c.idx == 0 {
            self.discard();
            self.active = Some(self.start(c));
        }
        let usable = self
            .active
            .as_ref()
            .is_some_and(|a| a.xfer == c.xfer && a.next_idx == c.idx);
        if !usable {
            // continuation of a transfer we never saw the start of (receiver restarted)
            self.discard();
            return c.last.then(|| Received {
                name: sanitize_name(c.name),
                size: c.size,
                sha256: "-".into(),
                path: None,
                problem: Some(
                    "transfer interrupted before it started here; ask the peer to resend".into(),
                ),
            });
        }
        let a = self.active.as_mut().expect("checked above");
        a.next_idx += 1;
        if a.failed.is_none() {
            a.written += c.data.len() as u64;
            if a.written > a.size {
                a.failed = Some("peer sent more data than it declared".into());
            } else {
                a.hasher.update(c.data);
                if let Some((f, _)) = a.tmp.as_mut() {
                    if let Err(e) = f.write_all(c.data) {
                        a.failed = Some(format!("cannot write to the inbox: {e}"));
                    }
                }
            }
        }
        if !c.last {
            return None;
        }
        let a = self.active.take().expect("checked above");
        Some(self.finish(a))
    }

    fn finish(&mut self, a: Active) -> Received {
        let computed = data_encoding::HEXLOWER.encode(&a.hasher.finalize());
        let tmp = a.tmp;
        let remove_tmp = |tmp: &Option<(std::fs::File, PathBuf)>| {
            if let Some((_, p)) = tmp {
                let _ = std::fs::remove_file(p);
            }
        };
        let mut rec = Received {
            name: a.name,
            size: a.written,
            sha256: computed.clone(),
            path: None,
            problem: a.failed,
        };
        if rec.problem.is_none() {
            if a.written != a.size {
                rec.problem = Some(format!(
                    "size mismatch: sender declared {} bytes, received {}",
                    a.size, a.written
                ));
            } else if computed != a.declared_sha {
                rec.problem = Some(format!(
                    "sha256 mismatch: sender declared {}, received bytes hash to {computed}",
                    a.declared_sha
                ));
            }
        }
        if rec.problem.is_some() {
            remove_tmp(&tmp);
            return rec;
        }
        let Some((file, tmp_path)) = tmp.as_ref() else {
            rec.problem = Some("no staging file".into());
            return rec;
        };
        match file
            .sync_all()
            .map_err(Into::into)
            .and_then(|_| self.place(tmp_path, &rec.name, &computed))
        {
            Ok(p) => rec.path = Some(p),
            Err(e) => rec.problem = Some(format!("cannot save to the inbox: {e:#}")),
        }
        remove_tmp(&tmp);
        rec
    }

    /// Hard-links the staged file to a free name; `link` fails if the target exists, so an
    /// existing file is never overwritten. A same-named file with identical content is reused:
    /// a transfer redelivered after a daemon restart must not leave a duplicate behind.
    fn place(&self, tmp: &Path, name: &str, sha: &str) -> anyhow::Result<PathBuf> {
        let (stem, ext) = match Path::new(name).extension() {
            Some(e) => (
                Path::new(name)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                format!(".{}", e.to_string_lossy()),
            ),
            None => (name.to_string(), String::new()),
        };
        for n in 0..10_000u32 {
            let cand = if n == 0 {
                name.to_string()
            } else {
                format!("{stem}-{n}{ext}")
            };
            let target = self.dir.join(cand);
            match std::fs::hard_link(tmp, &target) {
                Ok(()) => return Ok(target),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if file_sha(&target).as_deref() == Some(sha) {
                        return Ok(target);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("no free file name for {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn names_are_reduced_to_a_safe_basename() {
        for (raw, want) in [
            ("report.txt", "report.txt"),
            ("../../etc/passwd", "passwd"),
            ("/etc/shadow", "shadow"),
            ("a/b/c.md", "c.md"),
            ("..\\..\\win.ini", "win.ini"),
            ("..", "file"),
            (".", "file"),
            ("", "file"),
            ("dir/", "file"),
            (".bashrc", "bashrc"),
            ("..hidden", "hidden"),
            ("a\x00b\nc\x1b[31m.txt", "abc.txt"),
            ("x\u{202E}txt.exe", "xtxt.exe"),
            ("  spaced.txt  ", "spaced.txt"),
        ] {
            assert_eq!(sanitize_name(raw), want, "raw={raw:?}");
        }
        let long = sanitize_name(&"é".repeat(500));
        assert!(long.len() <= NAME_MAX_BYTES && !long.is_empty());
        assert!(!sanitize_name("../x").contains('/'));
    }

    fn rx() -> (tempfile::TempDir, Receiver) {
        let t = tempfile::tempdir().unwrap();
        let r = Receiver::new(t.path().join("room").join("inbox"));
        (t, r)
    }

    fn feed(r: &mut Receiver, xfer: u64, name: &str, data: &[u8], sha: &str) -> Received {
        let parts: Vec<&[u8]> = if data.is_empty() {
            vec![data]
        } else {
            data.chunks(7).collect()
        };
        let mut out = None;
        for (i, p) in parts.iter().enumerate() {
            out = r.on_chunk(&Chunk {
                xfer,
                idx: i as u32,
                last: i + 1 == parts.len(),
                name,
                size: data.len() as u64,
                sha256: sha,
                data: p,
            });
        }
        out.expect("last chunk yields a result")
    }

    #[test]
    fn saves_verbatim_with_receiver_computed_sha_and_private_modes() {
        let (t, mut r) = rx();
        let data = b"line\n\x00\xff\xfebinary\r\n\n";
        let got = feed(&mut r, 1, "a.bin", data, &sha256_hex(data));
        assert_eq!(got.problem, None);
        assert_eq!(got.sha256, sha256_hex(data));
        let p = got.path.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), data);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let inbox = t.path().join("room/inbox");
        assert_eq!(
            std::fs::metadata(&inbox).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(p.parent().unwrap(), inbox);
        // no staging leftovers
        assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 1);
    }

    #[test]
    fn hostile_name_stays_inside_the_inbox() {
        let (t, mut r) = rx();
        let got = feed(&mut r, 1, "../../../evil", b"x", &sha256_hex(b"x"));
        let p = got.path.unwrap();
        assert_eq!(p, t.path().join("room/inbox/evil"));
        assert!(!t.path().join("evil").exists());
    }

    #[test]
    fn collisions_get_a_suffix_and_never_overwrite() {
        let (_t, mut r) = rx();
        let mut paths = vec![];
        for (i, body) in [b"one".as_ref(), b"two", b"three"].into_iter().enumerate() {
            let got = feed(&mut r, i as u64, "n.txt", body, &sha256_hex(body));
            paths.push((got.path.unwrap(), body));
        }
        let names: Vec<_> = paths
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["n.txt", "n-1.txt", "n-2.txt"]);
        for (p, body) in paths {
            assert_eq!(std::fs::read(p).unwrap(), body);
        }
    }

    #[test]
    fn redelivered_identical_file_is_not_duplicated() {
        let (t, mut r) = rx();
        let a = feed(&mut r, 1, "n.txt", b"same", &sha256_hex(b"same"));
        let b = feed(&mut r, 2, "n.txt", b"same", &sha256_hex(b"same"));
        assert_eq!(a.path, b.path);
        assert_eq!(
            std::fs::read_dir(t.path().join("room/inbox"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn sha_mismatch_is_reported_and_nothing_is_saved() {
        let (t, mut r) = rx();
        let got = feed(&mut r, 1, "a.txt", b"hello", &sha256_hex(b"other"));
        assert!(got.path.is_none());
        assert!(got.problem.unwrap().contains("sha256 mismatch"));
        assert_eq!(got.sha256, sha256_hex(b"hello"));
        assert_eq!(
            std::fs::read_dir(t.path().join("room/inbox"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn declared_size_over_limit_is_rejected_and_drained() {
        let (_t, mut r) = rx();
        let big = MAX_FILE_BYTES + 1;
        let sha = sha256_hex(b"");
        assert!(r
            .on_chunk(&Chunk {
                xfer: 1,
                idx: 0,
                last: false,
                name: "big",
                size: big,
                sha256: &sha,
                data: b"abc"
            })
            .is_none());
        let got = r
            .on_chunk(&Chunk {
                xfer: 1,
                idx: 1,
                last: true,
                name: "big",
                size: big,
                sha256: &sha,
                data: b"def",
            })
            .unwrap();
        assert!(got.path.is_none());
        assert!(got.problem.unwrap().contains("limit"));
    }

    #[test]
    fn more_data_than_declared_is_rejected() {
        let (_t, mut r) = rx();
        let sha = sha256_hex(b"ab");
        let got = r
            .on_chunk(&Chunk {
                xfer: 1,
                idx: 0,
                last: true,
                name: "x",
                size: 2,
                sha256: &sha,
                data: b"abcdef",
            })
            .unwrap();
        assert!(got.path.is_none() && got.problem.unwrap().contains("more data"));
    }

    #[test]
    fn empty_file_and_bad_sha_declaration() {
        let (_t, mut r) = rx();
        let got = feed(&mut r, 1, "empty", b"", &sha256_hex(b""));
        assert_eq!(std::fs::read(got.path.unwrap()).unwrap(), b"");
        let got = r
            .on_chunk(&Chunk {
                xfer: 2,
                idx: 0,
                last: true,
                name: "y",
                size: 1,
                sha256: "zz",
                data: b"y",
            })
            .unwrap();
        assert!(got.path.is_none() && got.problem.unwrap().contains("sha256"));
    }

    #[test]
    fn orphan_continuation_is_reported_not_saved() {
        let (_t, mut r) = rx();
        let sha = sha256_hex(b"x");
        assert!(r
            .on_chunk(&Chunk {
                xfer: 9,
                idx: 3,
                last: false,
                name: "x",
                size: 1,
                sha256: &sha,
                data: b"x"
            })
            .is_none());
        let got = r
            .on_chunk(&Chunk {
                xfer: 9,
                idx: 4,
                last: true,
                name: "x",
                size: 1,
                sha256: &sha,
                data: b"x",
            })
            .unwrap();
        assert!(got.path.is_none() && got.problem.unwrap().contains("interrupted"));
    }

    #[test]
    fn sender_checks_reject_non_regular_and_oversize() {
        let t = tempfile::tempdir().unwrap();
        let e = check_sendable(t.path()).unwrap_err();
        assert!(e.to_string().contains("not a regular file"));
        assert!(check_sendable(&t.path().join("missing")).is_err());
        let big = t.path().join("big");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(check_sendable(&big)
            .unwrap_err()
            .to_string()
            .contains("too large"));
        let ok = t.path().join("ok.txt");
        std::fs::write(&ok, b"hi\n").unwrap();
        let o = load_outgoing(&ok).unwrap();
        assert_eq!(
            (o.name.as_str(), o.data.as_slice()),
            ("ok.txt", b"hi\n".as_ref())
        );
        assert_eq!(o.sha256, sha256_hex(b"hi\n"));
    }
}
