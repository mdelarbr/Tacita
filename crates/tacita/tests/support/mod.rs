#![allow(dead_code)]
//! Shared helpers for the daemon integration tests: a fake upstream, a
//! Tacita started on a random port, and SSE builders.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tacita::daemon::{DaemonOptions, Timing, serve};
use tacita::secrets::MemoryStore;

pub const SECRET: &str = "0123456789abcdef0123456789abcdef";

/// The default config text (see `Config::create_default`) with `SECRET`.
macro_rules! default_config {
    ($anthropic_protection:literal) => {
        concat!(
            "port = 47821\n",
            "secret = \"0123456789abcdef0123456789abcdef\"\n",
            "\n",
            "[anthropic]\n",
            "protection = ",
            $anthropic_protection,
            "\n",
            "\n",
            "[openrouter]\n",
            "protection = \"pairs\"   # \"off\" is refused\n",
            "only = []              # optional trusted OpenRouter provider slugs\n",
            "\n",
            "[step0]\n",
            "pairs = [\n",
            "  { real = \"acmewidgets\", standin = \"client_x01\" },\n",
            "  { real = \"AcmeWidgets\", standin = \"ClientX01\" },\n",
            "  { real = \"Jeanne Fictive\", standin = \"person_x01\" },\n",
            "]\n",
            "block_probe = \"TACITA_BLOCK_PROBE\"\n",
            "stream_probe = \"TACITA_STREAM_PROBE\"\n",
            "guard_probe = \"TACITA_GUARD_PROBE\"\n",
            "# record_dir = \"/Users/you/tacita-corpus\"   # Step 0 test 5; never inside a git repo\n",
        )
    };
}

pub const CONFIG_OFF: &str =
    default_config!(r#""off"     # "off" = Step 0 test 1, "pairs" = test 2"#);
pub const CONFIG_PAIRS: &str =
    default_config!(r#""pairs"   # "off" = Step 0 test 1, "pairs" = test 2"#);

/// One request as the fake upstream saw it.
pub struct Received {
    pub method: String,
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub enum Reply {
    /// A body with a known length.
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// A streamed body: each chunk is sent after its delay in milliseconds.
    Chunks {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: Vec<(u64, Vec<u8>)>,
    },
    /// An SSE text reply that streams back `messages[-1].content[0].text`
    /// in 3-character deltas.
    Echo,
    /// `Echo`, but each event is its own chunk, sent after `delay_ms`.
    EchoSlow { delay_ms: u64 },
}

fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl Reply {
    pub fn sse(body: Vec<u8>) -> Reply {
        Reply::status(200, &[("content-type", "text/event-stream")], body)
    }

    pub fn status(code: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> Reply {
        Reply::Full {
            status: code,
            headers: owned(headers),
            body: body.into(),
        }
    }

    pub fn chunks(status: u16, headers: &[(&str, &str)], chunks: Vec<(u64, Vec<u8>)>) -> Reply {
        Reply::Chunks {
            status,
            headers: owned(headers),
            chunks,
        }
    }
}

pub struct FakeUpstream {
    pub url: String,
    pub received: Arc<Mutex<Vec<Received>>>,
}

struct Fake {
    replies: Vec<Reply>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeUpstream {
    /// Replies are used in order; the last one repeats. With no replies,
    /// every request gets a 500.
    pub async fn start(replies: Vec<Reply>) -> FakeUpstream {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let fake = Arc::new(Fake {
            replies,
            received: received.clone(),
        });
        let app = axum::Router::new().fallback(fake_reply).with_state(fake);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeUpstream { url, received }
    }

    /// An upstream nothing listens on.
    pub fn unreachable() -> FakeUpstream {
        FakeUpstream {
            url: "http://127.0.0.1:1".into(),
            received: Default::default(),
        }
    }

    pub fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

async fn fake_reply(State(fake): State<Arc<Fake>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap()
        .to_vec();
    let index = {
        let mut r = fake.received.lock().unwrap();
        r.push(Received {
            method: parts.method.to_string(),
            path_and_query: parts
                .uri
                .path_and_query()
                .map(|p| p.to_string())
                .unwrap_or_default(),
            headers: parts.headers.clone(),
            body: body.clone(),
        });
        r.len() - 1
    };
    let reply = fake.replies.get(index).or(fake.replies.last()).cloned();
    match reply {
        None => respond(500, &[], Body::from("fake upstream: no reply configured")),
        Some(Reply::Full {
            status,
            headers,
            body,
        }) => respond(status, &headers, Body::from(body)),
        Some(Reply::Chunks {
            status,
            headers,
            chunks,
        }) => respond(status, &headers, chunked(chunks)),
        Some(Reply::Echo) => respond(
            200,
            &owned(&[("content-type", "text/event-stream")]),
            Body::from(sse(&echo_events(&body))),
        ),
        Some(Reply::EchoSlow { delay_ms }) => {
            let chunks = echo_events(&body)
                .iter()
                .map(|ev| (delay_ms, sse(std::slice::from_ref(ev))))
                .collect();
            respond(
                200,
                &owned(&[("content-type", "text/event-stream")]),
                chunked(chunks),
            )
        }
    }
}

/// Each chunk is sent after its delay in milliseconds.
fn chunked(chunks: Vec<(u64, Vec<u8>)>) -> Body {
    let stream = futures_util::stream::iter(chunks).then(|(delay, bytes)| async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok::<_, Infallible>(Bytes::from(bytes))
    });
    Body::from_stream(stream)
}

fn respond(status: u16, headers: &[(String, String)], body: Body) -> Response {
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    for (k, v) in headers {
        r.headers_mut().append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    r
}

fn echo_events(request: &[u8]) -> Vec<(&'static str, Value)> {
    let v: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let content = v["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].clone())
        .unwrap_or(Value::Null);
    let text = match &content {
        Value::String(s) => s.clone(),
        c => c[0]["text"].as_str().unwrap_or("").to_string(),
    };
    let chars: Vec<char> = text.chars().collect();
    let mut events = vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_echo","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
    ];
    for piece in chars.chunks(3) {
        let piece: String = piece.iter().collect();
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":piece}}),
        ));
    }
    events.push((
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    events.push((
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
    ));
    events.push(("message_stop", json!({"type":"message_stop"})));
    events
}

/// A reply that is only `message_stop`.
pub fn stop_only() -> Vec<u8> {
    sse(&[("message_stop", json!({"type":"message_stop"}))])
}

/// Asserts a Tacita error response and returns its message.
pub async fn tacita_error(resp: reqwest::Response, status: u16) -> String {
    assert_eq!(resp.status(), status);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    let msg = v["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.starts_with("Tacita: "), "{msg}");
    msg
}

/// `event: <name>\ndata: <compact json>\n\n` for each event.
pub fn sse(events: &[(&str, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in events {
        out.extend_from_slice(format!("event: {name}\ndata: {data}\n\n").as_bytes());
    }
    out
}

pub struct Tacita {
    pub base: String,
    pub home: PathBuf,
    pub port: u16,
    _dir: tempfile::TempDir,
}

impl Tacita {
    pub fn log_path(&self) -> PathBuf {
        self.home.join("logs/requests.jsonl")
    }
}

pub async fn start_tacita(
    config_toml: &str,
    state_toml: Option<&str>,
    anthropic: &FakeUpstream,
    openrouter: &FakeUpstream,
    secrets: Arc<MemoryStore>,
    timing: Timing,
) -> Tacita {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::fs::write(home.join("config.toml"), config_toml).unwrap();
    if let Some(state) = state_toml {
        std::fs::write(home.join("state.toml"), state).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opts = DaemonOptions {
        home: home.clone(),
        anthropic_upstream: anthropic.url.clone(),
        openrouter_upstream: openrouter.url.clone(),
        secrets,
        timing,
    };
    tokio::spawn(async move {
        if let Err(e) = serve(opts, listener).await {
            panic!("tacita serve failed: {e:#}");
        }
    });
    Tacita {
        base: format!("http://127.0.0.1:{port}/{SECRET}"),
        home,
        port,
        _dir: dir,
    }
}

/// Polls every 20 ms, for up to 2 s, until `path` has at least `n` lines.
/// Returns whatever lines are there at the end.
pub async fn wait_for_lines(path: &Path, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let lines: Vec<String> = std::fs::read_to_string(path)
            .map(|t| t.lines().map(String::from).collect())
            .unwrap_or_default();
        if lines.len() >= n || Instant::now() >= deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits for exactly `n` lines in `requests.jsonl` and parses them; panics
/// with what it found if there are not exactly `n` after 2 s.
pub async fn meta_lines(t: &Tacita, n: usize) -> Vec<Value> {
    let lines = wait_for_lines(&t.log_path(), n).await;
    if lines.len() != n {
        panic!(
            "expected {n} line(s) in requests.jsonl within 2 s, found {}: {lines:#?}",
            lines.len()
        );
    }
    lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad meta line {l}: {e}")))
        .collect()
}
