mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::*;
use tacita::config::{Config, ConfigError};
use tacita::daemon::{DaemonOptions, Server, Timing};
use tacita::secrets::{MemoryStore, SecretStore};

const TOKEN: &str = "sk-ant-oat01-TESTTOKEN";
const SUFFIXES: [&str; 5] = [
    "request.json",
    "forwarded.json",
    "upstream.txt",
    "client.txt",
    "meta.json",
];

fn with_record_dir(config: &str, dir: &Path) -> String {
    let t = config.replace(
        "# record_dir = \"/Users/you/tacita-corpus\"",
        &format!("record_dir = {:?}", dir.to_str().unwrap()),
    );
    assert_ne!(t, config);
    t
}

fn body(text: &str) -> String {
    json!({"model":"claude-opus-5-5","max_tokens":10,"messages":[{"role":"user","content":[{"type":"text","text":text}]}]}).to_string()
}

async fn post(t: &Tacita, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/9.9")
        .header("x-api-key", "sk-ant-api03-KEYVALUE")
        .header("cookie", "a=b")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn start(
    config: &str,
    state: Option<&str>,
    replies: Vec<Reply>,
    or_replies: Vec<Reply>,
    secrets: Arc<MemoryStore>,
) -> Tacita {
    let a = FakeUpstream::start(replies).await;
    let o = FakeUpstream::start(or_replies).await;
    start_tacita(config, state, &a, &o, secrets, Timing::default()).await
}

fn run_dirs(corpus: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(corpus)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Polls up to 2 s for the five files of request `n` in the only run dir.
async fn files(corpus: &Path, n: u64) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let runs = run_dirs(corpus);
        if let [run] = runs.as_slice() {
            let got: Vec<_> = SUFFIXES
                .iter()
                .map(|s| std::fs::read(run.join(format!("{n:06}-{s}"))))
                .collect();
            if got.iter().all(Result::is_ok) {
                for s in SUFFIXES {
                    let mode = std::fs::metadata(run.join(format!("{n:06}-{s}")))
                        .unwrap()
                        .permissions()
                        .mode();
                    assert_eq!(mode & 0o777, 0o600, "{s}");
                }
                assert_eq!(
                    std::fs::metadata(run).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                return got.into_iter().map(Result::unwrap).collect();
            }
        }
        assert!(Instant::now() < deadline, "files for {n} not written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn meta(f: &[Vec<u8>]) -> Value {
    serde_json::from_slice(&f[4]).unwrap()
}

#[tokio::test]
async fn records_five_files_without_credentials() {
    let corpus = tempfile::tempdir().unwrap();
    let up = sse(&[("message_stop", json!({"type":"message_stop"}))]);
    let t = start(
        &with_record_dir(CONFIG_OFF, corpus.path()),
        None,
        vec![Reply::sse(up.clone())],
        vec![],
        Arc::new(MemoryStore::default()),
    )
    .await;
    let req = body("hello");
    let got = post(&t, req.clone()).await.bytes().await.unwrap();
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[0], req.as_bytes());
    assert_eq!(f[1], req.as_bytes(), "off mode forwards the request as is");
    assert_eq!(f[2], up);
    assert_eq!(f[3], got.to_vec());
    for (i, file) in f.iter().enumerate() {
        assert!(!String::from_utf8_lossy(file).contains("TESTTOKEN"), "{i}");
        assert!(!String::from_utf8_lossy(file).contains("KEYVALUE"), "{i}");
    }
    let m = meta(&f);
    assert_eq!(m["status"], 200);
    assert_eq!(m["truncated"], false);
    assert_eq!(m["headers"]["authorization"], "<redacted>");
    assert_eq!(m["headers"]["x-api-key"], "<redacted>");
    assert_eq!(m["headers"]["cookie"], "<redacted>");
    assert_eq!(m["headers"]["anthropic-version"], "2023-06-01");
    assert_eq!(m["headers"]["user-agent"], "claude-cli/9.9");
}

#[tokio::test]
async fn protected_sse_records_scrubbed_and_restored_sides() {
    let corpus = tempfile::tempdir().unwrap();
    let up = sse(&[
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi client_x01"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ]);
    let t = start(
        &with_record_dir(CONFIG_PAIRS, corpus.path()),
        None,
        vec![Reply::sse(up.clone())],
        vec![],
        Arc::new(MemoryStore::default()),
    )
    .await;
    let req = body("about acmewidgets");
    let got = post(&t, req.clone()).await.bytes().await.unwrap();
    assert!(String::from_utf8_lossy(&got).contains("hi acmewidgets"));
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[0], req.as_bytes());
    let fwd = String::from_utf8(f[1].clone()).unwrap();
    assert!(fwd.contains("client_x01") && !fwd.contains("acmewidgets"));
    assert_eq!(f[2], up);
    assert_eq!(f[3], got.to_vec());
}

#[tokio::test]
async fn protected_json_and_upstream_errors_are_recorded() {
    let corpus = tempfile::tempdir().unwrap();
    let msg = json!({"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"yo client_x01"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}).to_string();
    let t = start(
        &with_record_dir(CONFIG_PAIRS, corpus.path()),
        None,
        vec![
            Reply::status(200, &[("content-type", "application/json")], msg.clone()),
            Reply::status(429, &[("content-type", "application/json")], "{\"e\":1}"),
        ],
        vec![],
        Arc::new(MemoryStore::default()),
    )
    .await;
    let got = post(&t, body("x")).await.bytes().await.unwrap();
    assert!(String::from_utf8_lossy(&got).contains("yo acmewidgets"));
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[2], msg.as_bytes());
    assert_eq!(f[3], got.to_vec());
    let r = post(&t, body("y")).await;
    assert_eq!(r.status(), 429);
    r.bytes().await.unwrap();
    let f = files(corpus.path(), 2).await;
    assert_eq!(f[2], b"{\"e\":1}");
    assert_eq!(f[3], b"{\"e\":1}");
    assert_eq!(meta(&f)["status"], 429);
}

#[tokio::test]
async fn refused_requests_are_recorded_with_tacitas_error() {
    let corpus = tempfile::tempdir().unwrap();
    let t = start(
        &with_record_dir(CONFIG_PAIRS, corpus.path()),
        None,
        vec![],
        vec![],
        Arc::new(MemoryStore::default()),
    )
    .await;
    // Block probe.
    let req = body("TACITA_BLOCK_PROBE");
    let r = post(&t, req.clone()).await;
    let client = r.bytes().await.unwrap();
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[0], req.as_bytes());
    assert!(f[1].is_empty() && f[2].is_empty());
    assert_eq!(f[3], client.to_vec());
    assert_eq!(meta(&f)["status"], 400);
    // Scrub error: not JSON.
    let r = post(&t, "not json".to_string()).await;
    let client = r.bytes().await.unwrap();
    let f = files(corpus.path(), 2).await;
    assert!(f[1].is_empty() && f[2].is_empty());
    assert!(String::from_utf8_lossy(&f[3]).starts_with("{\"type\":\"error\""));
    assert_eq!(f[3], client.to_vec());
}

#[tokio::test]
async fn unreachable_upstream_is_recorded() {
    let corpus = tempfile::tempdir().unwrap();
    let a = FakeUpstream::unreachable();
    let o = FakeUpstream::start(vec![]).await;
    let t = start_tacita(
        &with_record_dir(CONFIG_OFF, corpus.path()),
        None,
        &a,
        &o,
        Arc::new(MemoryStore::default()),
        Timing::default(),
    )
    .await;
    let client = post(&t, body("x")).await.bytes().await.unwrap();
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[1], body("x").as_bytes(), "forwarded was attempted");
    assert!(f[2].is_empty());
    assert_eq!(f[3], client.to_vec());
    assert_eq!(meta(&f)["status"], 502);
}

#[tokio::test]
async fn openrouter_recording_never_holds_the_injected_key() {
    let corpus = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemoryStore::default());
    secrets.set("openrouter", "sk-or-v1-TEST").unwrap();
    let t = start(
        &with_record_dir(CONFIG_PAIRS, corpus.path()),
        Some("provider = \"openrouter\"\nopenrouter_model = \"qwen/qwen3-coder\"\n"),
        vec![],
        vec![Reply::Echo],
        secrets,
    )
    .await;
    let req = body("about acmewidgets");
    let got = post(&t, req.clone()).await.bytes().await.unwrap();
    let f = files(corpus.path(), 1).await;
    assert_eq!(f[0], req.as_bytes());
    let fwd = String::from_utf8(f[1].clone()).unwrap();
    assert!(fwd.contains("qwen/qwen3-coder") && !fwd.contains("acmewidgets"));
    assert!(!f[2].is_empty());
    assert_eq!(f[3], got.to_vec());
    for file in &f {
        let s = String::from_utf8_lossy(file);
        assert!(!s.contains("sk-or-v1-TEST") && !s.contains("TESTTOKEN"));
    }
    assert_eq!(meta(&f)["headers"]["authorization"], "<redacted>");
}

#[tokio::test]
async fn numbering_matches_the_meta_line_and_runs_do_not_clobber() {
    let corpus = tempfile::tempdir().unwrap();
    let cfg = with_record_dir(CONFIG_OFF, corpus.path());
    let mut homes = vec![];
    for _ in 0..2 {
        let t = start(
            &cfg,
            None,
            vec![Reply::sse(stop_only())],
            vec![],
            Arc::new(MemoryStore::default()),
        )
        .await;
        post(&t, body("a")).await.bytes().await.unwrap();
        let lines = meta_lines(&t, 1).await;
        assert_eq!(lines[0]["n"], 1);
        homes.push(t);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let runs = run_dirs(corpus.path());
        if runs.len() == 2 && runs.iter().all(|r| r.join("000001-meta.json").exists()) {
            assert!(
                runs.iter()
                    .all(|r| r.file_name().unwrap().to_str().unwrap().starts_with("run-"))
            );
            break;
        }
        assert!(Instant::now() < deadline, "{runs:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn nothing_is_written_without_record_dir() {
    let t = start(
        CONFIG_OFF,
        None,
        vec![Reply::sse(stop_only())],
        vec![],
        Arc::new(MemoryStore::default()),
    )
    .await;
    post(&t, body("a")).await.bytes().await.unwrap();
    meta_lines(&t, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut names: Vec<_> = std::fs::read_dir(&t.home)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(
        names
            .iter()
            .all(|n| !n.contains("run-") && !n.contains("corpus")),
        "{names:?}"
    );
}

#[tokio::test]
async fn daemon_refuses_to_start_when_record_dir_cannot_be_created() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("afile");
    std::fs::write(&file, b"x").unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        with_record_dir(CONFIG_OFF, &file.join("corpus")),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opts = DaemonOptions {
        home: home.path().to_path_buf(),
        anthropic_upstream: "http://127.0.0.1:1".into(),
        openrouter_upstream: "http://127.0.0.1:1".into(),
        secrets: Arc::new(MemoryStore::default()),
        timing: Timing::default(),
    };
    let e = match Server::new(opts, listener) {
        Ok(_) => panic!("must refuse"),
        Err(e) => format!("{e:#}"),
    };
    assert!(e.contains("record_dir") || e.contains("corpus"), "{e}");
}

fn load(dir: &Path) -> Result<Config, ConfigError> {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        with_record_dir(CONFIG_OFF, dir),
    )
    .unwrap();
    Config::load(home.path())
}

#[test]
fn record_dir_inside_git_repo_is_refused() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let e = load(&repo.path().join("corpus")).err().unwrap();
    assert!(matches!(e, ConfigError::RecordDirInRepo), "{e}");
    // Deeper and not yet existing.
    let e = load(&repo.path().join("a/b/corpus")).err().unwrap();
    assert!(matches!(e, ConfigError::RecordDirInRepo));
    // The repo itself.
    assert!(matches!(
        load(repo.path()),
        Err(ConfigError::RecordDirInRepo)
    ));
    // Outside a repo is fine.
    let other = tempfile::tempdir().unwrap();
    assert!(load(&other.path().join("corpus")).is_ok());
}

#[test]
fn record_dir_in_worktree_with_git_file_or_behind_symlink_is_refused() {
    let wt = tempfile::tempdir().unwrap();
    std::fs::write(wt.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(matches!(
        load(&wt.path().join("corpus")),
        Err(ConfigError::RecordDirInRepo)
    ));
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("l");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    assert!(matches!(
        load(&link.join("corpus")),
        Err(ConfigError::RecordDirInRepo)
    ));
}
