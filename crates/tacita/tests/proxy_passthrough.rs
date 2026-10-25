mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::*;
use tacita::daemon::Timing;

const MIN_BODY: &str = r#"{"model":"claude-opus-5-5","max_tokens":10,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;

fn message_start(cache_read: u64) -> Value {
    json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":2,"cache_read_input_tokens":cache_read,"output_tokens":1}}})
}

async fn start_off(up: &FakeUpstream, or: &FakeUpstream) -> Tacita {
    start_tacita(
        CONFIG_OFF,
        None,
        up,
        or,
        Default::default(),
        Timing::default(),
    )
    .await
}

#[tokio::test]
async fn forwards_auth_and_beta_headers_unchanged() {
    let up = FakeUpstream::start(vec![Reply::sse(sse(&[(
        "message_stop",
        json!({"type":"message_stop"}),
    )]))])
    .await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_tacita(
        CONFIG_OFF,
        None,
        &up,
        &or,
        Default::default(),
        Timing::default(),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("authorization", "Bearer sk-ant-oat01-TESTTOKEN")
        .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219")
        .header("x-claude-code-session-id", "sess-1")
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let got = &up.received.lock().unwrap()[0];
    assert_eq!(got.path_and_query, "/v1/messages?beta=true");
    assert_eq!(
        got.headers["authorization"],
        "Bearer sk-ant-oat01-TESTTOKEN"
    );
    assert_eq!(
        got.headers["anthropic-beta"],
        "oauth-2025-04-20,claude-code-20250219"
    );
    assert_eq!(got.headers["x-claude-code-session-id"], "sess-1");
    assert_eq!(got.body, MIN_BODY.as_bytes());
    assert!(or.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_query_means_no_query_upstream() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), stop_only());
    assert_eq!(
        up.received.lock().unwrap()[0].path_and_query,
        "/v1/messages"
    );
    assert_eq!(up.received.lock().unwrap()[0].method, "POST");
}

#[tokio::test]
async fn streams_without_buffering() {
    let first = sse(&[("message_start", message_start(0))]);
    let second = stop_only();
    let up = FakeUpstream::start(vec![Reply::chunks(
        200,
        &[("content-type", "text/event-stream")],
        vec![(0, first.clone()), (400, second.clone())],
    )])
    .await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let sent = Instant::now();
    let mut resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    let chunk = resp.chunk().await.unwrap().unwrap();
    let first_at = sent.elapsed();
    assert!(first_at < Duration::from_millis(300), "{first_at:?}");
    assert_eq!(chunk, first);
    let mut rest = Vec::new();
    while let Some(c) = resp.chunk().await.unwrap() {
        rest.extend_from_slice(&c);
    }
    assert_eq!(rest, second);
    assert!(sent.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn count_tokens_is_404_and_not_forwarded() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages/count_tokens?beta=true", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    let msg = tacita_error(resp, 404).await;
    assert_eq!(msg, "Tacita: count_tokens is not forwarded");
    assert_eq!(up.count(), 0);
    assert_eq!(or.count(), 0);
    let m = meta_lines(&t, 1).await;
    assert_eq!(m[0]["route"], "count_tokens");
    assert_eq!(m[0]["status"], 404);
}

#[tokio::test]
async fn hello_is_answered_locally() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let head = c
        .head(format!("{}/api/hello", t.base))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    // The tag of the config the daemon loaded, for `tacita status`.
    let tag = tacita::config::config_tag(CONFIG_OFF.as_bytes());
    assert_eq!(head.headers()["x-tacita-config"], tag.as_str());
    let get = c.get(format!("{}/api/hello", t.base)).send().await.unwrap();
    assert_eq!(get.status(), 200);
    assert!(get.bytes().await.unwrap().is_empty());
    let post = c
        .post(format!("{}/api/hello", t.base))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 404);
    assert_eq!(up.count(), 0);
    let m = meta_lines(&t, 3).await;
    assert_eq!(m[0]["route"], "hello");
    assert_eq!(m[1]["route"], "hello");
    assert_eq!(m[2]["route"], "refused");
}

#[tokio::test]
async fn wrong_secret_and_unknown_paths_are_refused() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let wrong = format!("http://127.0.0.1:{}/{}/v1/messages", t.port, "f".repeat(32));
    let resp = c.post(wrong).body(MIN_BODY).send().await.unwrap();
    tacita_error(resp, 404).await;
    let resp = c.get(format!("{}/v1/models", t.base)).send().await.unwrap();
    tacita_error(resp, 404).await;
    assert_eq!(up.count(), 0);
    assert_eq!(or.count(), 0);
    let lines = wait_for_lines(&t.log_path(), 2).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    for l in &lines {
        assert!(l.contains(r#""route":"refused""#), "{l}");
    }
}

#[tokio::test]
async fn log_never_holds_the_secret_or_unmatched_paths() {
    const WRONG: &str = "fedcba9876543210fedcba9876543210";
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let root = format!("http://127.0.0.1:{}", t.port);
    let r = c
        .post(format!("{root}/{WRONG}/v1/messages?q=LEAKQ"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = c
        .get(format!("{}/v1/models?x=1", t.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = c
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("origin", "https://x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .post(format!("{root}/{WRONG}/v1/messages?q=LEAKQ"))
        .header("origin", "https://x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c.post(format!("{root}/{SECRET}")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let m = meta_lines(&t, 5).await;
    let fields = |i: usize| {
        (
            m[i]["route"].as_str().unwrap().to_string(),
            m[i]["path"].as_str().unwrap().to_string(),
            m[i]["query"].clone(),
        )
    };
    assert_eq!(
        fields(0),
        ("refused".into(), "(wrong secret)".into(), Value::Null)
    );
    assert_eq!(
        fields(1),
        ("refused".into(), "/v1/models".into(), json!("x=1"))
    );
    assert_eq!(
        fields(2),
        (
            "forbidden".into(),
            "/v1/messages".into(),
            json!("beta=true")
        )
    );
    assert_eq!(
        fields(3),
        ("forbidden".into(), "(wrong secret)".into(), Value::Null)
    );
    assert_eq!(
        fields(4),
        ("refused".into(), "(wrong secret)".into(), Value::Null)
    );
    let log = std::fs::read_to_string(t.log_path()).unwrap();
    assert!(!log.contains(SECRET));
    assert!(!log.contains(WRONG));
    assert!(!log.contains("LEAKQ"));
}

#[tokio::test]
async fn foreign_host_and_browser_headers_are_forbidden() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let cases = [
        ("host", format!("evil.example:{}", t.port)),
        ("host", format!("127.0.0.1:{}", t.port.wrapping_add(1))),
        ("origin", "https://x".to_string()),
        ("sec-fetch-site", "cross-site".to_string()),
        ("referer", "https://x/".to_string()),
        ("sec-fetch-dest", "empty".to_string()),
        ("sec-fetch-user", "?1".to_string()),
    ];
    for (name, value) in &cases {
        let resp = c
            .post(format!("{}/v1/messages", t.base))
            .header(*name, value)
            .body(MIN_BODY)
            .send()
            .await
            .unwrap();
        tacita_error(resp, 403).await;
    }
    assert_eq!(up.count(), 0);
    for m in meta_lines(&t, cases.len()).await {
        assert_eq!(m["route"], "forbidden");
        assert_eq!(m["status"], 403);
    }
}

#[tokio::test]
async fn sec_fetch_mode_alone_is_allowed() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .header("sec-fetch-mode", "cors")
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), stop_only());
    assert_eq!(up.count(), 1);
    assert_eq!(
        up.received.lock().unwrap()[0].headers["sec-fetch-mode"],
        "cors"
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // Even unprotected: Claude Code's fetch would follow a passed-on
    // redirect and resend the body and the bearer to wherever it points.
    // The client here follows redirects too.
    for code in [301u16, 302, 303, 307, 308] {
        let (up, elsewhere) = redirecting(code).await;
        let or = FakeUpstream::start(vec![]).await;
        let t = start_off(&up, &or).await;
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", t.base))
            .header("authorization", "Bearer sk-ant-oat01-TESTTOKEN")
            .body(MIN_BODY)
            .send()
            .await
            .unwrap();
        redirect_refused(resp).await;
        assert_eq!((up.count(), elsewhere.count()), (1, 0), "{code}");
        let m = &meta_lines(&t, 1).await[0];
        assert_eq!(m["status"], 502, "{code}");
        assert_eq!(m["tacita_error"], REDIRECT_REFUSED);
    }
}

#[tokio::test]
async fn upstream_errors_pass_verbatim() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
    let headers = [
        ("content-type", "application/json"),
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-reset", "1761300000"),
        ("retry-after", "30"),
        ("x-should-retry", "true"),
        ("request-id", "req_011"),
    ];
    let up = FakeUpstream::start(vec![Reply::status(429, &headers, body)]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    for (k, v) in headers {
        assert_eq!(resp.headers()[k], v, "{k}");
    }
    assert_eq!(resp.headers()["content-length"], body.len().to_string());
    assert_eq!(resp.bytes().await.unwrap(), body.as_bytes());
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 429);
    assert_eq!(
        m["ratelimit"],
        json!({"anthropic-ratelimit-unified-reset":"1761300000","anthropic-ratelimit-unified-status":"rejected"})
    );
    assert_eq!(m["retry_after"], "30");
    assert_eq!(m["request_id"], "req_011");
    assert_eq!(m["upstream_error"], "rate_limit_error: Error");
    assert_eq!(m["tacita_error"], Value::Null);
}

#[tokio::test]
async fn oversized_body_is_refused() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(vec![b' '; 64 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    let msg = tacita_error(resp, 413).await;
    assert_eq!(msg, "Tacita: request larger than 64 MiB");
    assert_eq!(up.count(), 0);
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 413);
    assert_eq!(m["tacita_error"], "request larger than 64 MiB");
}

#[tokio::test]
async fn upstream_unreachable_is_a_visible_error() {
    let up = FakeUpstream::unreachable();
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    // Transient: Claude Code may retry it.
    assert!(resp.headers().get("x-should-retry").is_none());
    let msg = tacita_error(resp, 502).await;
    assert!(
        msg.starts_with("Tacita: cannot reach 127.0.0.1:1: "),
        "{msg}"
    );
    assert!(!msg.contains("beta") && !msg.contains("/v1/"), "{msg}");
    assert!(msg.len() < 200, "{msg}");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 502);
    assert_eq!(
        m["tacita_error"].as_str().unwrap(),
        &msg["Tacita: ".len()..]
    );
}

#[tokio::test]
async fn metalog_records_safe_fields_only() {
    let reply = sse(&[
        ("message_start", message_start(7)),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":9}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ]);
    let up = FakeUpstream::start(vec![Reply::sse(reply)]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("authorization", "Bearer sk-ant-oat01-TESTTOKEN")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header("x-claude-code-session-id", "sess-1")
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.bytes().await.unwrap();
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["oauth_bearer"], true);
    assert_eq!(m["oauth_beta"], true);
    assert_eq!(
        m["system1_head"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(m["usage"]["cache_read"], 7);
    assert_eq!(
        m["usage"],
        json!({"input":3,"cache_creation":2,"cache_read":7,"output":9})
    );
    assert_eq!(m["stop_reason"], "end_turn");
    assert_eq!(m["n"], 1);
    assert_eq!(m["method"], "POST");
    assert_eq!(m["path"], "/v1/messages");
    assert_eq!(m["query"], "beta=true");
    assert_eq!(m["route"], "messages");
    assert_eq!(m["provider"], "anthropic");
    assert_eq!(m["protection"], "off");
    // Off mode counts nothing: null, never a reassuring 0.
    assert_eq!(m.get("reals_left"), Some(&Value::Null));
    assert_eq!(m["session"], "sess-1");
    assert_eq!(m["model_in"], "claude-opus-5-5");
    assert_eq!(m["model_out"], "claude-opus-5-5");
    assert_eq!(m["status"], 200);
    let names: Vec<&str> = m["header_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&"authorization") && names.contains(&"anthropic-beta"));
    assert!(names.windows(2).all(|w| w[0] < w[1]), "{names:?}");
    assert!(m["ts"].as_u64().unwrap() > 1_700_000_000_000);
    assert!(m["ms"].is_u64());
    let log = std::fs::read_to_string(t.log_path()).unwrap();
    assert!(!log.contains("TESTTOKEN"));
    assert!(!log.contains("sk-ant-oat"));
    assert!(!log.contains(SECRET));
    assert!(!log.contains(r#""hi""#));
}

#[tokio::test]
async fn observation_never_alters_the_stream() {
    // An event with invalid UTF-8 stops observation; the bytes still pass.
    let mut broken = b"event: ping\ndata: \xff\xfe\n\n".to_vec();
    broken.extend_from_slice(&sse(&[("message_start", message_start(7))]));
    broken.extend_from_slice(&stop_only());
    // A compressed stream is never parsed, only forwarded.
    let gzipped = sse(&[("message_start", message_start(7))]);
    let up = FakeUpstream::start(vec![
        Reply::sse(broken.clone()),
        Reply::status(
            200,
            &[
                ("content-type", "text/event-stream"),
                ("content-encoding", "gzip"),
            ],
            gzipped.clone(),
        ),
    ])
    .await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let r1 = c
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.bytes().await.unwrap(), broken);
    let r2 = c
        .post(format!("{}/v1/messages", t.base))
        .header("accept-encoding", "gzip")
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.headers()["content-encoding"], "gzip");
    assert_eq!(r2.bytes().await.unwrap(), gzipped);
    assert_eq!(
        up.received.lock().unwrap()[1].headers["accept-encoding"],
        "gzip"
    );
    for m in meta_lines(&t, 2).await {
        assert_eq!(m["status"], 200);
        assert_eq!(m["usage"], Value::Null);
    }
}

#[tokio::test]
async fn meta_line_is_written_once_when_the_client_leaves() {
    let mut chunks = vec![(0, sse(&[("message_start", message_start(7))]))];
    for _ in 0..20 {
        chunks.push((200, sse(&[("ping", json!({"type":"ping"}))])));
    }
    let up = FakeUpstream::start(vec![Reply::chunks(
        200,
        &[("content-type", "text/event-stream")],
        chunks,
    )])
    .await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let mut resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    resp.chunk().await.unwrap().unwrap();
    drop(resp);
    // The upstream would take 4 s to finish; the line comes from the drop.
    let m = meta_lines(&t, 1).await;
    assert_eq!(m[0]["status"], 200);
    assert_eq!(m[0]["usage"]["cache_read"], 7);
    assert_eq!(m[0]["stop_reason"], Value::Null);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wait_for_lines(&t.log_path(), 2).await.len(), 1);
}

#[tokio::test]
async fn a_changed_config_stops_forwarding_until_restarted() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let send = || async {
        reqwest::Client::new()
            .post(format!("{}/v1/messages", t.base))
            .body(MIN_BODY)
            .send()
            .await
            .unwrap()
    };
    assert_eq!(send().await.status(), 200);
    let file = t.home.join("config.toml");
    let message = "config.toml changed since the daemon started; restart `tacita daemon`";
    // Any byte changed (here only a comment), then the file gone.
    let changed = CONFIG_OFF.replace("Step 0 test 1", "Step 0 test one");
    for edit in [Some(changed.as_str()), None] {
        match edit {
            Some(text) => std::fs::write(&file, text).unwrap(),
            None => std::fs::remove_file(&file).unwrap(),
        }
        let resp = send().await;
        assert_eq!(resp.headers()["x-should-retry"], "false");
        assert_eq!(tacita_error(resp, 500).await, format!("Tacita: {message}"));
    }
    assert_eq!(up.count(), 1);
    // The same bytes again are the config the daemon runs on.
    std::fs::write(&file, CONFIG_OFF).unwrap();
    assert_eq!(send().await.status(), 200);
    assert_eq!(up.count(), 2);
    let m = meta_lines(&t, 4).await;
    for line in &m[1..3] {
        assert_eq!(line["status"], 500);
        assert_eq!(line["tacita_error"], message);
        assert_eq!(line["provider"], Value::Null);
    }
}

#[tokio::test]
async fn bad_state_is_a_visible_500() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_tacita(
        CONFIG_OFF,
        Some("provider = \"nope\"\n"),
        &up,
        &or,
        Default::default(),
        Timing::default(),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    let msg = tacita_error(resp, 500).await;
    assert!(
        msg.starts_with("Tacita: cannot read state.toml: state.toml line 1: "),
        "{msg}"
    );
    assert_eq!(up.count(), 0);
}

#[tokio::test]
async fn openrouter_without_a_key_is_refused_visibly() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let openrouter = start_tacita(
        CONFIG_OFF,
        Some("provider = \"openrouter\"\nopenrouter_model = \"qwen/qwen3-coder\"\n"),
        &up,
        &or,
        Default::default(),
        Timing::default(),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", openrouter.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    let msg = tacita_error(resp, 400).await;
    assert_eq!(
        msg,
        "Tacita: no OpenRouter key: run `tacita key set openrouter`"
    );
    assert_eq!(up.count(), 0);
    assert_eq!(or.count(), 0);
    let o = &meta_lines(&openrouter, 1).await[0];
    assert_eq!(
        (&o["provider"], &o["protection"]),
        (&json!("openrouter"), &json!("pairs"))
    );
}

#[test]
fn config_consts_are_the_default_config() {
    let home = tempfile::tempdir().unwrap();
    let cfg = tacita::config::Config::create_default(home.path()).unwrap();
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert_eq!(text.replace(&cfg.secret, SECRET), CONFIG_OFF);
    assert_eq!(
        CONFIG_OFF.replace(r#"protection = "off"     #"#, r#"protection = "pairs"   #"#),
        CONFIG_PAIRS
    );
}

#[tokio::test]
async fn echo_streams_the_last_user_text_in_small_deltas() {
    let up = FakeUpstream::start(vec![Reply::Echo]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let body = MIN_BODY.replace(r#""text":"hi""#, r#""text":"hello acmewidgets""#);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", t.base))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let text = String::from_utf8(resp.bytes().await.unwrap().to_vec()).unwrap();
    let pieces: Vec<String> = text
        .split("\n\n")
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")))
        .map(|d| serde_json::from_str::<Value>(d).unwrap())
        .filter(|v| v["delta"]["type"] == "text_delta")
        .map(|v| v["delta"]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(pieces.concat(), "hello acmewidgets");
    assert_eq!(pieces.len(), 6, "{pieces:?}");
    assert!(
        pieces[..5].iter().all(|p| p.chars().count() == 3),
        "{pieces:?}"
    );
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["stop_reason"], "end_turn");
    assert_eq!(m["usage"]["output"], 3);
}

#[tokio::test]
async fn a_secret_repeated_in_path_or_query_is_never_logged() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_off(&up, &or).await;
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{}/{SECRET}/v1/messages", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = c
        .post(format!("{}/v1/messages?k={SECRET}", t.base))
        .body(MIN_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let m = meta_lines(&t, 2).await;
    assert_eq!(m[0]["path"], "/[secret]/v1/messages");
    assert_eq!(m[1]["path"], "/v1/messages");
    assert_eq!(m[1]["query"], "k=[secret]");
    assert!(
        !std::fs::read_to_string(t.log_path())
            .unwrap()
            .contains(SECRET)
    );
}
