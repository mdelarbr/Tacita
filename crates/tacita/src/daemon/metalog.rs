//! The metadata-only request log: what a request looked like, never what it
//! said. No body content and no credential value is ever written here.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tacita_core::sse::{SseEvent, SseParser};

use super::route::strip_secret;

/// Characters of `system[1].text` kept.
const SYSTEM1_CHARS: usize = 60;
/// Characters kept of short values: model, session id, stop reason, ids.
const SHORT_CHARS: usize = 64;
/// Characters kept of `upstream_error`.
const ERROR_CHARS: usize = 300;
/// Bytes of a non-SSE error body read on the side.
const ERROR_TEE_BYTES: usize = 64 * 1024;

#[derive(Serialize, Default, Debug, Clone, PartialEq)]
pub struct Usage {
    pub input: Option<u64>,
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Serialize, Default)]
pub struct RequestMeta {
    /// Unix time in milliseconds when the request arrived.
    pub ts: u64,
    pub n: u64,
    pub method: String,
    /// The path after the secret, or "(wrong secret)".
    pub path: String,
    /// Only when the secret matched.
    pub query: Option<String>,
    pub route: &'static str,
    pub provider: Option<&'static str>,
    pub protection: Option<&'static str>,
    pub session: Option<String>,
    pub model_in: Option<String>,
    pub model_out: Option<String>,
    /// Sorted, lowercase.
    pub header_names: Vec<String>,
    pub oauth_bearer: bool,
    pub oauth_beta: bool,
    pub system1_head: Option<String>,
    pub scrubbed: Option<usize>,
    pub scrubbed_system: Option<usize>,
    pub restored: Option<usize>,
    pub dangling: Option<usize>,
    /// The status sent to the client; 0 if none was sent.
    pub status: u16,
    pub ratelimit: BTreeMap<String, String>,
    pub retry_after: Option<String>,
    pub request_id: Option<String>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<String>,
    pub upstream_error: Option<String>,
    pub tacita_error: Option<String>,
    pub ms: u64,
}

impl RequestMeta {
    /// The fields known from the request line and headers.
    /// The path and query are recorded only when the secret matched. Any
    /// repeat of the secret in them, in a header name or in the session id
    /// is written as `[secret]`.
    pub fn begin(
        n: u64,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        secret: &str,
    ) -> RequestMeta {
        let (path, query) = match strip_secret(uri.path(), secret) {
            Some(rest) => (
                mask_secret(rest, secret),
                uri.query().map(|q| mask_secret(q, secret)),
            ),
            None => ("(wrong secret)".to_string(), None),
        };
        let mut header_names: Vec<String> = headers
            .keys()
            .map(|k| mask_secret(k.as_str(), secret))
            .collect();
        header_names.sort();
        RequestMeta {
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            n,
            method: cut(method.as_str(), SHORT_CHARS),
            path,
            query,
            session: headers
                .get("x-claude-code-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|v| cut(&mask_secret(v, secret), SHORT_CHARS)),
            header_names,
            oauth_bearer: headers
                .get_all(AUTHORIZATION)
                .iter()
                .any(|v| v.as_bytes().starts_with(b"Bearer sk-ant-oat")),
            oauth_beta: headers
                .get_all("anthropic-beta")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .any(|item| item.trim().starts_with("oauth-")),
            ..RequestMeta::default()
        }
    }

    /// `model` and the head of `system[1].text`, when the body has them.
    /// Lone surrogates are read as U+FFFD, as the scrub reads them.
    pub fn peek_request(&mut self, body: &[u8]) {
        #[derive(Deserialize)]
        struct Peek {
            model: Option<Value>,
            system: Option<Value>,
        }
        let body = tacita_core::json::clean_lone_surrogates(body);
        let Ok(p) = serde_json::from_slice::<Peek>(&body) else {
            return;
        };
        self.model_in = p
            .model
            .as_ref()
            .and_then(Value::as_str)
            .map(|m| cut(m, SHORT_CHARS));
        self.system1_head = p
            .system
            .as_ref()
            .and_then(|s| s.get(1))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .map(|t| cut(t, SYSTEM1_CHARS));
    }

    /// The upstream status and its safe headers.
    pub fn upstream_response(&mut self, status: StatusCode, headers: &HeaderMap) {
        self.status = status.as_u16();
        for (name, value) in headers {
            if name.as_str().starts_with("anthropic-ratelimit-unified-")
                && let Ok(v) = value.to_str()
            {
                self.ratelimit.insert(name.to_string(), cut(v, SHORT_CHARS));
            }
        }
        self.retry_after = header_text(headers, "retry-after");
        self.request_id = header_text(headers, "request-id");
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| cut(v, SHORT_CHARS))
}

fn mask_secret(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        return s.to_string();
    }
    s.replace(secret, "[secret]")
}

/// The first `max` characters.
fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Reads usage, the stop reason and errors from an event, by its name.
pub fn observe(meta: &mut RequestMeta, ev: &SseEvent) {
    let Some(name @ ("message_start" | "message_delta" | "error")) = ev.event.as_deref() else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
        return;
    };
    match name {
        "message_start" => {
            if let Some(u) = v.pointer("/message/usage") {
                meta.usage = Some(Usage {
                    input: u["input_tokens"].as_u64(),
                    cache_creation: u["cache_creation_input_tokens"].as_u64(),
                    cache_read: u["cache_read_input_tokens"].as_u64(),
                    output: u["output_tokens"].as_u64(),
                });
            }
        }
        "message_delta" => {
            if let Some(out) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                meta.usage.get_or_insert_with(Usage::default).output = Some(out);
            }
            if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                meta.stop_reason = Some(cut(r, SHORT_CHARS));
            }
        }
        _ => meta.upstream_error = error_text(&v),
    }
}

/// `"<error.type>: <error.message>"`, bounded.
fn error_text(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    let kind = e.get("type").and_then(Value::as_str).unwrap_or("?");
    let message = e.get("message").and_then(Value::as_str).unwrap_or("");
    Some(cut(&format!("{kind}: {message}"), ERROR_CHARS))
}

/// Reads a forwarded response on the side. Observation only: it never
/// changes, delays or stops the bytes, and gives up on anything unexpected.
pub struct Observer {
    sse: Option<SseParser>,
    tee: Option<Vec<u8>>,
}

impl Observer {
    /// SSE replies are parsed; other error replies are teed. A compressed
    /// reply is not read at all.
    pub fn new(status: StatusCode, headers: &HeaderMap) -> Observer {
        let identity = headers.get_all(CONTENT_ENCODING).iter().all(|v| {
            v.to_str()
                .is_ok_and(|e| e.trim().eq_ignore_ascii_case("identity"))
        });
        let is_sse = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split(';').next())
            .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("text/event-stream"));
        Observer {
            sse: (identity && is_sse).then(SseParser::default),
            tee: (identity && !is_sse && status.as_u16() >= 400).then(Vec::new),
        }
    }

    pub fn feed(&mut self, meta: &mut RequestMeta, chunk: &[u8]) {
        if let Some(parser) = &mut self.sse {
            match parser.push(chunk) {
                Ok(events) => events.iter().for_each(|ev| observe(meta, ev)),
                Err(_) => self.sse = None,
            }
        }
        if let Some(tee) = &mut self.tee {
            if tee.len() + chunk.len() > ERROR_TEE_BYTES {
                self.tee = None;
            } else {
                tee.extend_from_slice(chunk);
            }
        }
    }

    /// Idempotent.
    pub fn end(&mut self, meta: &mut RequestMeta) {
        self.sse = None;
        if let Some(tee) = self.tee.take()
            && let Ok(v) = serde_json::from_slice::<Value>(&tee)
        {
            meta.upstream_error = error_text(&v);
        }
    }
}

/// Rewrites `sk-ant-` and `sk-or-` followed by `[A-Za-z0-9_-]*` to
/// `sk-ant-[redacted]` and `sk-or-[redacted]`.
pub fn redact(line: &str) -> String {
    const PREFIXES: [&str; 2] = ["sk-ant-", "sk-or-"];
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some((at, prefix)) = PREFIXES
        .iter()
        .filter_map(|p| rest.find(p).map(|at| (at, *p)))
        .min()
    {
        out.push_str(&rest[..at + prefix.len()]);
        out.push_str("[redacted]");
        let token = &rest[at + prefix.len()..];
        let end = token
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(token.len());
        rest = &token[end..];
    }
    out.push_str(rest);
    out
}

fn summary(meta: &RequestMeta) -> String {
    redact(&format!(
        "#{} {} {} {} {} ms",
        meta.n,
        meta.path,
        meta.provider.unwrap_or("-"),
        meta.status,
        meta.ms
    ))
}

/// `<home>/logs/requests.jsonl`, one redacted JSON line per request.
pub struct MetaLog {
    file: Mutex<File>,
}

impl MetaLog {
    pub fn open(home: &Path) -> std::io::Result<MetaLog> {
        let dir = home.join("logs");
        crate::paths::ensure_private_dir(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join("requests.jsonl"))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(MetaLog {
            file: Mutex::new(file),
        })
    }

    /// Writes the line, then a one-line summary on stderr. A write failure
    /// is reported on stderr. It never panics: it runs in drop guards, and
    /// `eprintln!` panics when stderr is closed (Rust ignores SIGPIPE).
    pub fn append(&self, meta: &RequestMeta) {
        let mut stderr = std::io::stderr().lock();
        match serde_json::to_string(meta) {
            Ok(json) => {
                let line = redact(&json) + "\n";
                let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
                if let Err(e) = file.write_all(line.as_bytes()) {
                    let _ = writeln!(stderr, "Tacita: cannot write logs/requests.jsonl: {e}");
                }
            }
            Err(e) => {
                let _ = writeln!(stderr, "Tacita: cannot encode a log line: {e}");
            }
        }
        let _ = writeln!(stderr, "{}", summary(meta));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const S: &str = "0123456789abcdef0123456789abcdef";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn ev(name: &str, data: Value) -> SseEvent {
        SseEvent::build(name, &data.to_string())
    }

    fn sse_bytes(events: &[(&str, Value)]) -> Vec<u8> {
        events
            .iter()
            .flat_map(|(n, d)| ev(n, d.clone()).raw)
            .collect()
    }

    fn start(cache_read: u64) -> Value {
        json!({"type":"message_start","message":{"usage":{"input_tokens":3,"cache_creation_input_tokens":2,"cache_read_input_tokens":cache_read,"output_tokens":1}}})
    }

    #[test]
    fn redact_hides_tokens() {
        assert_eq!(redact("x sk-ant-oat01-AbC_9 y"), "x sk-ant-[redacted] y");
    }

    #[test]
    fn redact_covers_openrouter_and_repeats() {
        assert_eq!(
            redact(r#"{"a":"sk-or-v1-abc","b":"sk-ant-api03-Z-z","c":"sk-ant-"}"#),
            r#"{"a":"sk-or-[redacted]","b":"sk-ant-[redacted]","c":"sk-ant-[redacted]"}"#
        );
        assert_eq!(redact("no keys here"), "no keys here");
        assert_eq!(redact("k=sk-ant-oat01-x.y"), "k=sk-ant-[redacted].y");
        assert_eq!(redact("sk-or-é"), "sk-or-[redacted]é");
    }

    #[test]
    fn begin_strips_the_secret_or_hides_the_path() {
        let h = headers(&[
            ("host", "127.0.0.1:1"),
            ("Authorization", "Bearer sk-ant-oat01-T"),
            ("anthropic-beta", "claude-code-20250219, oauth-2025-04-20"),
            ("x-claude-code-session-id", "sess-1"),
        ]);
        let uri: Uri = format!("/{S}/v1/messages?beta=true").parse().unwrap();
        let m = RequestMeta::begin(4, &Method::POST, &uri, &h, S);
        assert_eq!((m.n, m.method.as_str()), (4, "POST"));
        assert_eq!(m.path, "/v1/messages");
        assert_eq!(m.query.as_deref(), Some("beta=true"));
        assert_eq!(
            m.header_names,
            [
                "anthropic-beta",
                "authorization",
                "host",
                "x-claude-code-session-id"
            ]
        );
        assert!(m.oauth_bearer && m.oauth_beta);
        assert_eq!(m.session.as_deref(), Some("sess-1"));
        assert!(m.ts > 1_700_000_000_000);

        let uri: Uri = "/ffff/v1/messages?q=1".parse().unwrap();
        let m = RequestMeta::begin(5, &Method::GET, &uri, &h, S);
        assert_eq!(m.path, "(wrong secret)");
        assert_eq!(m.query, None);
    }

    #[test]
    fn a_repeated_secret_is_masked_everywhere_begin_records() {
        let name = format!("x-{S}");
        let h = headers(&[
            ("host", "127.0.0.1:1"),
            (name.as_str(), "1"),
            ("x-claude-code-session-id", S),
        ]);
        let uri: Uri = format!("/{S}/{S}/v1/messages?k={S}&j=a{S}b")
            .parse()
            .unwrap();
        let m = RequestMeta::begin(1, &Method::POST, &uri, &h, S);
        assert_eq!(m.path, "/[secret]/v1/messages");
        assert_eq!(m.query.as_deref(), Some("k=[secret]&j=a[secret]b"));
        assert_eq!(
            m.header_names,
            ["host", "x-[secret]", "x-claude-code-session-id"]
        );
        assert_eq!(m.session.as_deref(), Some("[secret]"));
    }

    #[test]
    fn oauth_flags_need_the_exact_prefixes() {
        let uri: Uri = "/".parse().unwrap();
        let h = headers(&[
            ("authorization", "Bearer sk-ant-api03-T"),
            ("anthropic-beta", "claude-code-20250219,xoauth-1"),
        ]);
        let m = RequestMeta::begin(1, &Method::POST, &uri, &h, S);
        assert!(!m.oauth_bearer && !m.oauth_beta);
        let h = headers(&[
            ("anthropic-beta", "claude-code-20250219"),
            ("anthropic-beta", "oauth-2025-04-20"),
        ]);
        assert!(RequestMeta::begin(1, &Method::POST, &uri, &h, S).oauth_beta);
    }

    #[test]
    fn peek_reads_model_and_system1_head_only() {
        let mut m = RequestMeta::default();
        let long = "é".repeat(80);
        let body = json!({"model":"claude-opus-5-5","system":[{"type":"text","text":"a"},{"type":"text","text":long}],"messages":[]});
        m.peek_request(body.to_string().as_bytes());
        assert_eq!(m.model_in.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(m.system1_head, Some("é".repeat(60)));

        let mut m = RequestMeta::default();
        m.peek_request(br#"{"model":7,"system":"one string"}"#);
        assert_eq!((m.model_in, m.system1_head), (None, None));
        let mut m = RequestMeta::default();
        m.peek_request(b"not json");
        assert_eq!((m.model_in, m.system1_head), (None, None));
        let mut m = RequestMeta::default();
        m.peek_request(json!({"model":"m".repeat(500)}).to_string().as_bytes());
        assert_eq!(m.model_in.unwrap().chars().count(), 64);
    }

    #[test]
    fn upstream_response_keeps_safe_headers() {
        let mut m = RequestMeta::default();
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
            ("anthropic-ratelimit-requests-limit", "50"),
            ("retry-after", "30"),
            ("request-id", "req_1"),
            ("set-cookie", "a=b"),
        ]);
        m.upstream_response(StatusCode::TOO_MANY_REQUESTS, &h);
        assert_eq!(m.status, 429);
        assert_eq!(
            m.ratelimit.keys().collect::<Vec<_>>(),
            [
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-status"
            ]
        );
        assert_eq!(m.retry_after.as_deref(), Some("30"));
        assert_eq!(m.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn observe_reads_usage_stop_reason_and_errors() {
        let mut m = RequestMeta::default();
        observe(&mut m, &ev("message_start", start(7)));
        observe(
            &mut m,
            &ev(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            ),
        );
        assert_eq!(
            m.usage,
            Some(Usage {
                input: Some(3),
                cache_creation: Some(2),
                cache_read: Some(7),
                output: Some(42)
            })
        );
        assert_eq!(m.stop_reason.as_deref(), Some("tool_use"));
        observe(
            &mut m,
            &ev(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
            ),
        );
        assert_eq!(
            m.upstream_error.as_deref(),
            Some("overloaded_error: Overloaded")
        );
        observe(&mut m, &ev("ping", json!({"type":"ping"})));
        observe(&mut m, &SseEvent::build("message_start", "{not json"));
        assert_eq!(m.usage.as_ref().unwrap().cache_read, Some(7));
    }

    #[test]
    fn observer_reads_sse_split_anywhere() {
        let bytes = sse_bytes(&[
            ("message_start", start(7)),
            ("message_stop", json!({"type":"message_stop"})),
        ]);
        let h = headers(&[("content-type", "text/event-stream; charset=utf-8")]);
        let mut o = Observer::new(StatusCode::OK, &h);
        let mut m = RequestMeta::default();
        for b in bytes.chunks(5) {
            o.feed(&mut m, b);
        }
        o.end(&mut m);
        o.end(&mut m);
        assert_eq!(m.usage.unwrap().cache_read, Some(7));
    }

    #[test]
    fn observer_gives_up_on_bad_sse_and_compressed_bodies() {
        let good = sse_bytes(&[("message_start", start(7))]);
        let sse = headers(&[("content-type", "text/event-stream")]);
        let mut o = Observer::new(StatusCode::OK, &sse);
        let mut m = RequestMeta::default();
        o.feed(&mut m, b"data: \xff\n\n");
        o.feed(&mut m, &good);
        o.end(&mut m);
        assert_eq!(m.usage, None);

        let gz = headers(&[
            ("content-type", "text/event-stream"),
            ("content-encoding", "gzip"),
        ]);
        let mut o = Observer::new(StatusCode::OK, &gz);
        let mut m = RequestMeta::default();
        o.feed(&mut m, &good);
        o.end(&mut m);
        assert_eq!(m.usage, None);

        let id = headers(&[
            ("content-type", "text/event-stream"),
            ("content-encoding", "identity"),
        ]);
        let mut o = Observer::new(StatusCode::OK, &id);
        let mut m = RequestMeta::default();
        o.feed(&mut m, &good);
        assert_eq!(m.usage.unwrap().cache_read, Some(7));
    }

    #[test]
    fn observer_tees_error_bodies_up_to_64_kib() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        let json_h = headers(&[("content-type", "application/json")]);
        let mut o = Observer::new(StatusCode::BAD_REQUEST, &json_h);
        let mut m = RequestMeta::default();
        o.feed(&mut m, &body[..10]);
        assert_eq!(m.upstream_error, None);
        o.feed(&mut m, &body[10..]);
        o.end(&mut m);
        assert_eq!(
            m.upstream_error.as_deref(),
            Some("invalid_request_error: bad")
        );

        // Successful replies are not teed.
        let mut o = Observer::new(StatusCode::OK, &json_h);
        let mut m = RequestMeta::default();
        o.feed(&mut m, body);
        o.end(&mut m);
        assert_eq!(m.upstream_error, None);

        // Past 64 KiB the tee is dropped.
        let big = format!(
            r#"{{"type":"error","error":{{"type":"x","message":"{}"}}}}"#,
            "a".repeat(64 * 1024)
        );
        let mut o = Observer::new(StatusCode::BAD_REQUEST, &json_h);
        let mut m = RequestMeta::default();
        o.feed(&mut m, big.as_bytes());
        o.end(&mut m);
        assert_eq!(m.upstream_error, None);

        // Compressed error bodies are not read.
        let gz = headers(&[("content-encoding", "gzip")]);
        let mut o = Observer::new(StatusCode::BAD_REQUEST, &gz);
        let mut m = RequestMeta::default();
        o.feed(&mut m, body);
        o.end(&mut m);
        assert_eq!(m.upstream_error, None);
    }

    #[test]
    fn upstream_error_text_is_bounded() {
        let mut m = RequestMeta::default();
        let long = "m".repeat(5000);
        observe(
            &mut m,
            &ev(
                "error",
                json!({"type":"error","error":{"type":"api_error","message":long}}),
            ),
        );
        assert!(m.upstream_error.unwrap().chars().count() <= 300);
    }

    #[test]
    fn summary_is_short_and_redacted() {
        let m = RequestMeta {
            n: 3,
            path: "/v1/sk-ant-oat01-T".into(),
            provider: Some("anthropic"),
            status: 200,
            ms: 12,
            ..Default::default()
        };
        assert_eq!(summary(&m), "#3 /v1/sk-ant-[redacted] anthropic 200 12 ms");
        assert_eq!(summary(&RequestMeta::default()), "#0  - 0 0 ms");
    }

    #[test]
    fn log_file_is_private_and_lines_are_redacted() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let log = MetaLog::open(t.path()).unwrap();
        let m = RequestMeta {
            n: 1,
            path: "/x/sk-or-v1-abc".into(),
            ..Default::default()
        };
        log.append(&m);
        log.append(&m);
        let file = t.path().join("logs/requests.jsonl");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&t.path().join("logs")), 0o700);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(!text.contains("abc"));
        let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["path"], "/x/sk-or-[redacted]");
        assert_eq!(v["usage"], Value::Null);
        // Reopening appends.
        MetaLog::open(t.path()).unwrap().append(&m);
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);
    }
}
