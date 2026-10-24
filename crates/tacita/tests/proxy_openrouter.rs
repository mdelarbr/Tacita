mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::*;
use tacita::daemon::Timing;
use tacita::secrets::{MemoryStore, SecretStore};

const FIXTURE: &str = include_str!("../../tacita-core/tests/fixtures/claude_code_request.json");
const STATE: &str = "provider = \"openrouter\"\nopenrouter_model = \"qwen/qwen3-coder\"\n";
const KEY: &str = "sk-or-v1-TEST";

fn store(key: Option<&str>) -> Arc<MemoryStore> {
    let s = Arc::new(MemoryStore::default());
    if let Some(k) = key {
        s.set("openrouter", k).unwrap();
    }
    s
}

struct Rig {
    t: Tacita,
    anthropic: FakeUpstream,
    or: FakeUpstream,
}

async fn rig(state: Option<&str>, key: Option<&str>, or_replies: Vec<Reply>) -> Rig {
    let anthropic = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let or = FakeUpstream::start(or_replies).await;
    let t = start_tacita(
        CONFIG_PAIRS,
        state,
        &anthropic,
        &or,
        store(key),
        Timing::default(),
    )
    .await;
    Rig { t, anthropic, or }
}

async fn post(t: &Tacita, body: impl Into<reqwest::Body>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat01-SECRET")
        .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219")
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-session-id", "sess-1")
        .header("x-api-key", "sk-ant-api03-X")
        .header("cookie", "a=b")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .body(body)
        .send()
        .await
        .unwrap()
}

fn simple(text: &str) -> String {
    json!({"model":"claude-opus-5-5","max_tokens":10,"messages":[{"role":"user","content":[{"type":"text","text":text}]}]}).to_string()
}

fn last_request(u: &FakeUpstream) -> (Vec<(String, String)>, Vec<u8>, String) {
    let r = u.received.lock().unwrap();
    let r = r.last().unwrap();
    let h = r
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    (h, r.body.clone(), r.path_and_query.clone())
}

#[tokio::test]
async fn openrouter_gets_its_key_never_the_anthropic_bearer() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let resp = post(&r.t, FIXTURE).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;
    assert_eq!(r.anthropic.count(), 0);
    let (headers, body, path) = last_request(&r.or);
    assert_eq!(path, "/v1/messages");
    let get = |n: &str| {
        headers
            .iter()
            .filter(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("authorization"), ["Bearer sk-or-v1-TEST"]);
    assert_eq!(get("anthropic-beta"), ["claude-code-20250219"]);
    assert_eq!(get("anthropic-version"), ["2023-06-01"]);
    assert_eq!(get("accept-encoding"), ["identity"]);
    assert_eq!(get("content-type"), ["application/json"]);
    for (k, v) in &headers {
        assert!(!v.contains("sk-ant"), "{k}: {v}");
        assert!(!k.starts_with("x-claude-code-"), "{k}");
        assert!(!k.starts_with("anthropic-dangerous-"), "{k}");
        assert_ne!(k, "x-api-key");
        assert_ne!(k, "cookie");
    }
    assert!(!String::from_utf8_lossy(&body).contains("sk-ant"));
}

#[tokio::test]
async fn openrouter_body_adapted_and_scrubbed() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let resp = post(&r.t, FIXTURE).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;
    let (_, body, _) = last_request(&r.or);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["model"], "qwen/qwen3-coder");
    assert_eq!(v["provider"]["zdr"], true);
    assert_eq!(v["provider"]["data_collection"], "deny");
    assert_eq!(v["provider"]["allow_fallbacks"], false);
    assert!(v["metadata"].get("user_id").is_none());
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("\"thinking\""), "thinking block left");
    for p in ["/system/2/text", "/messages/0/content/0/text"] {
        let t = v.pointer(p).unwrap().as_str().unwrap();
        assert!(!t.contains("acmewidgets"), "{p}: {t}");
    }
    let tr = v["messages"][2]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        !tr.contains("acmewidgets") && !tr.contains("Jeanne"),
        "{tr}"
    );
    // Left: two tool names, the tool_use id, the tool_use_id, the image data.
    assert_eq!(text.matches("acmewidgets").count(), 5, "{text}");
}

#[tokio::test]
async fn anthropic_mode_unchanged_with_a_key_stored() {
    let r = rig(None, Some(KEY), vec![Reply::Echo]).await;
    let resp = post(&r.t, simple("hello")).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;
    assert_eq!(r.or.count(), 0);
    let (headers, body, _) = last_request(&r.anthropic);
    let auth = headers.iter().find(|(k, _)| k == "authorization").unwrap();
    assert_eq!(auth.1, "Bearer sk-ant-oat01-SECRET");
    assert!(headers.iter().any(|(k, _)| k == "x-claude-code-session-id"));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["model"], "claude-opus-5-5");
    assert!(v.get("provider").is_none());
}

#[tokio::test]
async fn missing_key_is_a_visible_error() {
    let r = rig(Some(STATE), None, vec![Reply::Echo]).await;
    let msg = tacita_error(post(&r.t, simple("hi")).await, 400).await;
    assert!(msg.contains("no OpenRouter key"), "{msg}");
    assert!(msg.contains("tacita key set openrouter"), "{msg}");
    assert_eq!((r.or.count(), r.anthropic.count()), (0, 0));
}

#[tokio::test]
async fn missing_model_is_a_visible_error() {
    // State::load already refuses this file; the request fails visibly.
    let r = rig(
        Some("provider = \"openrouter\"\n"),
        Some(KEY),
        vec![Reply::Echo],
    )
    .await;
    let resp = post(&r.t, simple("hi")).await;
    assert_eq!(resp.headers()["x-should-retry"], "false");
    let msg = tacita_error(resp, 500).await;
    assert!(msg.contains("tacita use openrouter --model"), "{msg}");
    assert_eq!((r.or.count(), r.anthropic.count()), (0, 0));
}

#[tokio::test]
async fn unknown_model_is_a_visible_error() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let body = simple("hi").replace("claude-opus-5-5", "gpt-4o");
    let msg = tacita_error(post(&r.t, body).await, 400).await;
    assert!(msg.contains("gpt-4o"), "{msg}");
    assert_eq!((r.or.count(), r.anthropic.count()), (0, 0));
}

#[tokio::test]
async fn provider_field_in_the_request_is_refused() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let body = simple("hi").replace("{\"model\"", "{\"provider\":{\"zdr\":false},\"model\"");
    let msg = tacita_error(post(&r.t, body).await, 400).await;
    assert!(msg.contains("provider"), "{msg}");
    assert_eq!(r.or.count(), 0);
}

#[tokio::test]
async fn block_probe_wins_over_a_missing_key() {
    let r = rig(Some(STATE), None, vec![Reply::Echo]).await;
    let msg = tacita_error(post(&r.t, simple("TACITA_BLOCK_PROBE")).await, 400).await;
    assert!(msg.contains("block probe"), "{msg}");
    assert_eq!(r.or.count(), 0);
}

#[tokio::test]
async fn unusable_key_is_a_visible_error_without_the_key() {
    let r = rig(Some(STATE), Some("sk-or-v1-BAD\nKEY"), vec![Reply::Echo]).await;
    let msg = tacita_error(post(&r.t, simple("hi")).await, 400).await;
    assert!(msg.contains("not usable"), "{msg}");
    assert!(!msg.contains("sk-or") && !msg.contains("BAD"), "{msg}");
    assert_eq!(r.or.count(), 0);
}

#[tokio::test]
async fn a_beta_header_of_only_oauth_is_not_sent() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", r.t.base))
        .header("content-type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .body(simple("hi"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;
    let (headers, _, _) = last_request(&r.or);
    assert!(headers.iter().all(|(k, _)| k != "anthropic-beta"));
}

#[tokio::test]
async fn names_restored_from_openrouter_stream() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let resp = post(&r.t, simple("Ask acmewidgets about Jeanne Fictive")).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let (_, sent, _) = last_request(&r.or);
    let sent = String::from_utf8(sent).unwrap();
    assert!(sent.contains("client_x01") && !sent.contains("acmewidgets"));
    assert!(text.contains("acmewidgets"), "{text}");
    assert!(text.contains("Jeanne Fictive"), "{text}");
    assert!(!text.contains("client_x01") && !text.contains("person_x01"));
}

const COMMENT: &str = ": OPENROUTER PROCESSING\n\n";

#[tokio::test]
async fn processing_comments_pass_between_events() {
    let mut sse_body = COMMENT.as_bytes().to_vec();
    sse_body.extend(stop_only());
    let r = rig(Some(STATE), Some(KEY), vec![Reply::sse(sse_body)]).await;
    let resp = post(&r.t, simple("hi")).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains(COMMENT), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
}

#[tokio::test]
async fn processing_comments_pass_while_a_tool_block_is_held() {
    let ev = |name, data: Value| sse(&[(name, data)]);
    let delta = |part: &str| {
        ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}}),
        )
    };
    let mut body = ev(
        "message_start",
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}}),
    );
    body.extend(COMMENT.as_bytes());
    body.extend(ev(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_0","name":"Read","input":{}}}),
    ));
    body.extend(COMMENT.as_bytes());
    body.extend(delta("{\"file_path\":\"/x/client"));
    body.extend(COMMENT.as_bytes());
    body.extend(delta("_x01/a\"}"));
    body.extend(ev(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    body.extend(ev(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}),
    ));
    body.extend(stop_only());
    let r = rig(Some(STATE), Some(KEY), vec![Reply::sse(body)]).await;
    let resp = post(&r.t, simple("hi")).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text.matches(COMMENT).count(), 3, "{text}");
    let deltas: Vec<&str> = text
        .split("\n\n")
        .filter(|b| b.contains("input_json_delta"))
        .collect();
    assert_eq!(deltas.len(), 1, "{text}");
    assert!(deltas[0].contains("/x/acmewidgets/a"), "{}", deltas[0]);
    assert!(!text.contains("client_x01"), "{text}");
}

#[tokio::test]
async fn missing_key_is_reported_before_an_unknown_model() {
    let r = rig(Some(STATE), None, vec![Reply::Echo]).await;
    let body = simple("hi").replace("claude-opus-5-5", "gpt-4o");
    let msg = tacita_error(post(&r.t, body).await, 400).await;
    assert!(msg.contains("no OpenRouter key"), "{msg}");
    assert_eq!(r.or.count(), 0);
}

#[tokio::test]
async fn upstream_errors_pass_verbatim() {
    let body = r#"{"error":{"message":"rate limited"}}"#;
    let r = rig(
        Some(STATE),
        Some(KEY),
        vec![Reply::status(
            429,
            &[("content-type", "application/json"), ("retry-after", "7")],
            body,
        )],
    )
    .await;
    let resp = post(&r.t, simple("hi")).await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "7");
    assert_eq!(resp.text().await.unwrap(), body);
}

#[tokio::test]
async fn redirects_from_openrouter_are_refused() {
    // `post` follows redirects, as Claude Code's fetch does: a passed-on
    // 307 would resend the original body and the Anthropic bearer.
    let anthropic = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let (or, elsewhere) = redirecting(307).await;
    let t = start_tacita(
        CONFIG_PAIRS,
        Some(STATE),
        &anthropic,
        &or,
        store(Some(KEY)),
        Timing::default(),
    )
    .await;
    redirect_refused(post(&t, simple("hi acmewidgets")).await).await;
    assert_eq!((or.count(), elsewhere.count(), anthropic.count()), (1, 0, 0));
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 502);
    assert_eq!(m["tacita_error"], REDIRECT_REFUSED);
}

#[tokio::test]
async fn meta_line_names_models_and_never_holds_the_key() {
    let r = rig(Some(STATE), Some(KEY), vec![Reply::Echo]).await;
    let resp = post(&r.t, simple("hi")).await;
    let _ = resp.bytes().await;
    let lines = meta_lines(&r.t, 1).await;
    let m = &lines[0];
    assert_eq!(m["provider"], "openrouter");
    assert_eq!(m["model_in"], "claude-opus-5-5");
    assert_eq!(m["model_out"], "qwen/qwen3-coder");
    let raw = std::fs::read_to_string(r.t.log_path()).unwrap();
    assert!(
        !raw.contains("sk-or-v1-TEST") && !raw.contains("sk-ant"),
        "{raw}"
    );
}

/// Run by hand: touches the real login Keychain (service dev.tacita.test).
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn keychain_round_trip() {
    use tacita::secrets::KeychainStore;
    /// Deletes the test item even if an assertion panics.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = security_framework::passwords::delete_generic_password(
                "dev.tacita.test",
                "openrouter",
            );
        }
    }
    let _cleanup = Cleanup;
    let s = KeychainStore::new("dev.tacita.test");
    assert_eq!(s.get("openrouter").unwrap(), None);
    s.set("openrouter", "sk-or-v1-ROUNDTRIP").unwrap();
    assert_eq!(
        s.get("openrouter").unwrap().as_deref(),
        Some("sk-or-v1-ROUNDTRIP")
    );
}
