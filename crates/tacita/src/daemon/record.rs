//! Step 0 corpus recorder (test 5).
//!
//! THE ONE DELIBERATE EXCEPTION to "Tacita never writes a credential value
//! to disk": it writes raw request and reply bodies, which can hold secrets.
//! It is opt-in (`step0.record_dir`), refused inside a git repo, and records
//! only what the client sent and got, never the credentials Tacita injects.
//!
//! The capture is a side copy taken while bytes pass; files are written when
//! the request's `MetaHandle` is dropped, off the async workers.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use bytes::Bytes;
use serde_json::json;

use crate::paths;

/// Capture stops after this many bytes per side.
pub const MAX_CAPTURE: usize = 64 * 1024 * 1024;

/// Header values kept as received; every other value is `<redacted>`.
const KEEP: [&str; 8] = [
    "anthropic-version",
    "anthropic-beta",
    "content-type",
    "accept",
    "user-agent",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
];

pub struct Recorder {
    dir: PathBuf,
}

pub struct RecordParts {
    pub headers: BTreeMap<String, String>,
    pub status: u16,
    pub request: Bytes,
    pub forwarded: Bytes,
    /// Chunks as they passed (refcounted, not copied); joined in `save`.
    pub upstream: Vec<Bytes>,
    pub client: Vec<Bytes>,
    pub truncated: bool,
}

impl Recorder {
    /// Creates `dir` (mode 0700) and records into it.
    pub fn new(dir: &Path) -> io::Result<Recorder> {
        paths::ensure_private_dir(dir)?;
        Ok(Recorder {
            dir: dir.to_path_buf(),
        })
    }

    /// A recorder for this daemon run: a fresh `<base>/run-<Unix ms>`, so
    /// runs (which number requests from 1) never overwrite each other.
    pub fn for_run(base: &Path) -> io::Result<Recorder> {
        paths::ensure_private_dir(base)?;
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        for ms in ms..ms + 1000 {
            match std::fs::create_dir(base.join(format!("run-{ms}"))) {
                Ok(()) => return Recorder::new(&base.join(format!("run-{ms}"))),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free run-<ms> directory",
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the five files, `meta` last. Never overwrites. Errors go to
    /// stderr only.
    pub fn save(&self, n: u64, parts: RecordParts) {
        let meta = json!({
            "n": n,
            "status": parts.status,
            "truncated": parts.truncated,
            "headers": parts.headers,
        })
        .to_string();
        let files: [(&str, Vec<u8>); 5] = [
            ("request.json", parts.request.to_vec()),
            ("forwarded.json", parts.forwarded.to_vec()),
            ("upstream.txt", parts.upstream.concat()),
            ("client.txt", parts.client.concat()),
            ("meta.json", meta.into_bytes()),
        ];
        for (suffix, bytes) in files {
            let path = self.dir.join(format!("{n:06}-{suffix}"));
            if let Err(e) = paths::create_private_new(&path, &bytes) {
                let _ = writeln!(
                    io::stderr(),
                    "Tacita: cannot record {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// Header names as received (lowercase); values only from the allowlist.
pub fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        let shown = if KEEP.contains(&name.as_str()) {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        } else {
            "<redacted>".to_string()
        };
        out.entry(name.to_string())
            .and_modify(|v| {
                v.push_str(", ");
                v.push_str(&shown);
            })
            .or_insert(shown);
    }
    out
}

/// Chunks kept up to a length limit. A chunk is a refcounted `Bytes` clone,
/// so nothing is copied while the reply passes.
struct Buf {
    chunks: Vec<Bytes>,
    len: usize,
    limit: usize,
    truncated: bool,
}

impl Buf {
    fn new(limit: usize) -> Buf {
        Buf {
            chunks: Vec::new(),
            len: 0,
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &Bytes) {
        let room = self.limit - self.len;
        if bytes.len() > room {
            self.truncated = true;
        }
        let keep = bytes.len().min(room);
        if keep > 0 {
            self.chunks.push(bytes.slice(..keep));
            self.len += keep;
        }
    }
}

/// One request's side copy. `finish` hands it to the recorder.
pub struct Capture {
    rec: Arc<Recorder>,
    headers: BTreeMap<String, String>,
    request: Bytes,
    forwarded: Bytes,
    upstream: Buf,
    client: Buf,
}

impl Capture {
    pub fn new(rec: Arc<Recorder>, headers: &HeaderMap, request: Bytes) -> Capture {
        Capture::with_limit(rec, headers, request, MAX_CAPTURE)
    }

    fn with_limit(
        rec: Arc<Recorder>,
        headers: &HeaderMap,
        request: Bytes,
        limit: usize,
    ) -> Capture {
        Capture {
            rec,
            headers: header_map(headers),
            request,
            forwarded: Bytes::new(),
            upstream: Buf::new(limit),
            client: Buf::new(limit),
        }
    }

    pub fn forwarded(&mut self, bytes: Bytes) {
        self.forwarded = bytes;
    }

    pub fn upstream(&mut self, bytes: &Bytes) {
        self.upstream.push(bytes);
    }

    pub fn client(&mut self, bytes: &Bytes) {
        self.client.push(bytes);
    }

    /// Writes the files on a blocking thread; the caller does not wait.
    pub fn finish(self, n: u64, status: u16) {
        let parts = RecordParts {
            headers: self.headers,
            status,
            request: self.request,
            forwarded: self.forwarded,
            truncated: self.upstream.truncated || self.client.truncated,
            upstream: self.upstream.chunks,
            client: self.client.chunks,
        };
        let rec = self.rec;
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                drop(h.spawn_blocking(move || rec.save(n, parts)));
            }
            Err(_) => rec.save(n, parts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn buf_stops_at_the_limit_and_says_so() {
        let mut b = Buf::new(5);
        b.push(&Bytes::from_static(b"abc"));
        assert!(!b.truncated);
        b.push(&Bytes::from_static(b"defg"));
        assert_eq!(b.chunks.concat(), b"abcde");
        assert!(b.truncated);
        b.push(&Bytes::from_static(b"h"));
        assert_eq!(b.chunks.concat(), b"abcde");
    }

    #[test]
    fn capture_truncation_reaches_the_meta_file() {
        let t = tempfile::tempdir().unwrap();
        let rec = Arc::new(Recorder::new(&t.path().join("r")).unwrap());
        let mut c = Capture::with_limit(
            rec.clone(),
            &HeaderMap::new(),
            Bytes::from_static(b"req"),
            4,
        );
        c.upstream(&Bytes::from_static(b"123456"));
        c.client(&Bytes::from_static(b"ab"));
        // No runtime here: `finish` saves inline.
        c.finish(7, 200);
        let r = t.path().join("r");
        assert_eq!(
            std::fs::read(r.join("000007-upstream.txt")).unwrap(),
            b"1234"
        );
        assert_eq!(std::fs::read(r.join("000007-client.txt")).unwrap(), b"ab");
        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r.join("000007-meta.json")).unwrap()).unwrap();
        assert_eq!(m["truncated"], true);
        assert_eq!(
            std::fs::metadata(r.join("000007-meta.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // Never overwrites.
        rec.save(
            7,
            RecordParts {
                headers: BTreeMap::new(),
                status: 1,
                request: Bytes::from_static(b"other"),
                forwarded: Bytes::new(),
                upstream: vec![],
                client: vec![],
                truncated: false,
            },
        );
        assert_eq!(
            std::fs::read(r.join("000007-request.json")).unwrap(),
            b"req"
        );
    }

    #[test]
    fn only_allowlisted_header_values_are_kept() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer sk-ant-x"));
        h.insert("user-agent", HeaderValue::from_static("cc/1"));
        h.append("cookie", HeaderValue::from_static("a=b"));
        h.append("cookie", HeaderValue::from_static("c=d"));
        let m = header_map(&h);
        assert_eq!(m["authorization"], "<redacted>");
        assert_eq!(m["cookie"], "<redacted>, <redacted>");
        assert_eq!(m["user-agent"], "cc/1");
    }

    #[test]
    fn for_run_makes_distinct_private_dirs() {
        let t = tempfile::tempdir().unwrap();
        let a = Recorder::for_run(t.path()).unwrap();
        let b = Recorder::for_run(t.path()).unwrap();
        assert_ne!(a.dir, b.dir);
        assert_eq!(
            std::fs::metadata(&b.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
