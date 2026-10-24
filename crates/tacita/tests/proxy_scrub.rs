mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::*;
use tacita::daemon::Timing;
use tacita_core::pairs::{Pair, Pairs};
use tacita_core::request::scrub_request;

const FIXTURE: &str = include_str!("../../tacita-core/tests/fixtures/claude_code_request.json");

/// Tacita's own ping. The upstream's pings (built by `sse`) have no space.
const PING: &str = "event: ping\ndata: {\"type\": \"ping\"}\n\n";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn timing(ping_every: u64, hold_limit: u64) -> Timing {
    Timing {
        ping_every: ms(ping_every),
        hold_limit: ms(hold_limit),
        ..Timing::default()
    }
}

fn body_with_text(text: &str) -> String {
    json!({"model":"claude-opus-5-5","max_tokens":10,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}],"messages":[{"role":"user","content":[{"type":"text","text":text}]}]}).to_string()
}

fn test_pairs() -> Pairs {
    let p = |real: &str, standin: &str| Pair {
        real: real.into(),
        standin: standin.into(),
    };
    Pairs::new(vec![
        p("acmewidgets", "client_x01"),
        p("AcmeWidgets", "ClientX01"),
        p("Jeanne Fictive", "person_x01"),
    ])
    .unwrap()
}

async fn start_with(config: &str, up: &FakeUpstream, timing: Timing) -> Tacita {
    let or = FakeUpstream::start(vec![]).await;
    start_tacita(config, None, up, &or, Default::default(), timing).await
}

async fn start_pairs(up: &FakeUpstream, timing: Timing) -> Tacita {
    start_with(CONFIG_PAIRS, up, timing).await
}

async fn post(t: &Tacita, body: impl Into<reqwest::Body>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn text_body(resp: reqwest::Response) -> String {
    String::from_utf8(resp.bytes().await.unwrap().to_vec()).unwrap()
}

/// Asserts a Tacita 502 and whether Claude Code may retry it (no
/// `x-should-retry: false`); returns the message.
async fn bad_gateway(resp: reqwest::Response, retry: bool) -> String {
    let flag = resp
        .headers()
        .get("x-should-retry")
        .map(|v| v.to_str().unwrap().to_string());
    assert_eq!(flag.as_deref(), if retry { None } else { Some("false") });
    tacita_error(resp, 502).await
}

/// `(name, data)` of every event of an SSE body.
fn events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|e| !e.trim().is_empty())
        .map(|e| {
            let name = e.lines().find_map(|l| l.strip_prefix("event: "));
            let data = e.lines().find_map(|l| l.strip_prefix("data: "));
            (
                name.unwrap_or("").to_string(),
                data.map_or(Value::Null, |d| serde_json::from_str(d).unwrap()),
            )
        })
        .collect()
}

fn text_of(evs: &[(String, Value)]) -> String {
    evs.iter()
        .filter(|(_, d)| d["delta"]["type"] == "text_delta")
        .map(|(_, d)| d["delta"]["text"].as_str().unwrap())
        .collect()
}

/// The messages of the `error` events.
fn errors(evs: &[(String, Value)]) -> Vec<String> {
    evs.iter()
        .filter(|(n, _)| n == "error")
        .map(|(_, d)| d["error"]["message"].as_str().unwrap().to_string())
        .collect()
}

fn tool_starts(evs: &[(String, Value)]) -> usize {
    evs.iter()
        .filter(|(n, d)| n == "content_block_start" && d["content_block"]["type"] == "tool_use")
        .count()
}

fn message_start() -> (&'static str, Value) {
    (
        "message_start",
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}),
    )
}

fn message_end() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ]
}

fn tool_start(index: u64) -> (&'static str, Value) {
    (
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("toolu_{index}"),"name":"Read","input":{}}}),
    )
}

fn tool_delta(index: u64, part: &str) -> (&'static str, Value) {
    (
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":part}}),
    )
}

fn block_stop(index: u64) -> (&'static str, Value) {
    (
        "content_block_stop",
        json!({"type":"content_block_stop","index":index}),
    )
}

fn text_start(index: u64) -> (&'static str, Value) {
    (
        "content_block_start",
        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
    )
}

fn text_delta(index: u64, text: &str) -> (&'static str, Value) {
    (
        "content_block_delta",
        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
    )
}

/// The Task 7 tool block.
fn task7_tool() -> Vec<(&'static str, Value)> {
    vec![
        tool_start(1),
        tool_delta(1, "{\"file_path\": \"/Users/dev/code/cli"),
        tool_delta(1, "ent_x01/a.md\", \"n\": 1e3}"),
        block_stop(1),
    ]
}

fn sse_reply(chunks: Vec<(u64, Vec<(&str, Value)>)>) -> Reply {
    Reply::chunks(
        200,
        &[("content-type", "text/event-stream")],
        chunks.into_iter().map(|(d, evs)| (d, sse(&evs))).collect(),
    )
}

fn input_json_deltas(evs: &[(String, Value)]) -> Vec<String> {
    evs.iter()
        .filter(|(_, d)| d["delta"]["type"] == "input_json_delta")
        .map(|(_, d)| d["delta"]["partial_json"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn scrubbed_request_reaches_upstream() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages?beta=true", t.base))
        .header("accept-encoding", "gzip, br")
        .header("authorization", "Bearer sk-ant-oat01-TESTTOKEN")
        .body(FIXTURE)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), stop_only());
    let expected = scrub_request(FIXTURE.as_bytes(), &test_pairs()).unwrap();
    let (body, headers) = {
        let r = &up.received.lock().unwrap()[0];
        (r.body.clone(), r.headers.clone())
    };
    assert_eq!(body, expected.body);
    let sent: Value = serde_json::from_slice(&body).unwrap();
    let original: Value = serde_json::from_str(FIXTURE).unwrap();
    let text = sent.pointer("/messages/0/content/0/text").unwrap();
    assert!(!text.as_str().unwrap().contains("acmewidgets"), "{text}");
    assert_eq!(sent["system"][1], original["system"][1]);
    let enc: Vec<_> = headers.get_all("accept-encoding").iter().collect();
    assert_eq!(enc, ["identity"]);
    assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-TESTTOKEN");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["protection"], "pairs");
    assert_eq!(m["scrubbed"], expected.replacements);
    assert_eq!(m["scrubbed_system"], expected.in_system);
    assert_eq!(expected.in_system, 2);
}

#[tokio::test]
async fn echo_round_trip_restores_names() {
    let up = FakeUpstream::start(vec![Reply::Echo]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let resp = post(&t, body_with_text("hello acmewidgets and Jeanne Fictive")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert!(resp.headers().get("content-length").is_none());
    let evs = events(&text_body(resp).await);
    let sent: Value = serde_json::from_slice(&up.received.lock().unwrap()[0].body).unwrap();
    assert_eq!(
        sent.pointer("/messages/0/content/0/text").unwrap(),
        "hello client_x01 and person_x01"
    );
    assert_eq!(text_of(&evs), "hello acmewidgets and Jeanne Fictive");
    assert!(errors(&evs).is_empty());
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["restored"], 2);
    assert_eq!(m["dangling"], 0);
    assert_eq!(m["stop_reason"], "end_turn");
    assert_eq!(m["usage"]["output"], 3);
    assert_eq!(m["tacita_error"], Value::Null);
}

#[tokio::test]
async fn tool_use_restored_in_one_delta() {
    let mut evs = vec![message_start()];
    evs.extend(task7_tool());
    evs.extend(message_end());
    let up = FakeUpstream::start(vec![Reply::sse(sse(&evs))]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let got = events(&text_body(post(&t, body_with_text("read it")).await).await);
    let deltas = input_json_deltas(&got);
    assert_eq!(deltas.len(), 1, "{deltas:?}");
    let input: Value = serde_json::from_str(&deltas[0]).unwrap();
    assert!(
        input["file_path"]
            .as_str()
            .unwrap()
            .contains("/acmewidgets/"),
        "{input}"
    );
    assert_eq!(tool_starts(&got), 1);
    assert_eq!(got.last().unwrap().0, "message_stop");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["restored"], 1);
    assert_eq!(m["stop_reason"], "tool_use");
}

#[tokio::test]
async fn pings_sent_while_holding() {
    let mut rest = task7_tool()[1..].to_vec();
    rest.extend(message_end());
    let up = FakeUpstream::start(vec![sse_reply(vec![
        (0, vec![message_start(), tool_start(1)]),
        (300, rest),
    ])])
    .await;
    let t = start_pairs(&up, timing(50, 2000)).await;
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    let at = text
        .find(r#""type":"tool_use""#)
        .expect("the tool call arrives");
    let before = &text[..at];
    assert!(before.matches(PING).count() >= 2, "{before}");
    assert_eq!(
        before.matches("event: ping").count(),
        before.matches(PING).count()
    );
    let evs = events(&text);
    assert_eq!(input_json_deltas(&evs).len(), 1);
    assert!(errors(&evs).is_empty());
}

#[tokio::test]
async fn pings_continue_while_held_deltas_arrive() {
    let mut chunks = vec![(0, vec![message_start(), tool_start(1)])];
    for _ in 0..12 {
        chunks.push((40, vec![tool_delta(1, "{}")]));
    }
    let mut end = vec![block_stop(1)];
    end.extend(message_end());
    chunks.push((0, end));
    let up = FakeUpstream::start(vec![sse_reply(chunks)]).await;
    let t = start_pairs(&up, timing(100, 2000)).await;
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    let at = text
        .find(r#""type":"tool_use""#)
        .expect("the tool call arrives");
    assert!(text[..at].matches(PING).count() >= 2, "{text}");
}

#[tokio::test]
async fn no_tacita_ping_while_not_holding() {
    let up = FakeUpstream::start(vec![sse_reply(vec![
        (
            0,
            vec![message_start(), text_start(0), text_delta(0, "hi ")],
        ),
        (300, vec![text_delta(0, "there"), block_stop(0)]),
        (0, message_end()),
    ])])
    .await;
    let t = start_pairs(&up, timing(50, 2000)).await;
    let text = text_body(post(&t, body_with_text("hi")).await).await;
    assert_eq!(text.matches("event: ping").count(), 0, "{text}");
    assert_eq!(text_of(&events(&text)), "hi there");
}

#[tokio::test]
async fn no_tacita_ping_while_upstream_bytes_keep_flowing() {
    let mut chunks = vec![(0, vec![message_start(), tool_start(1)])];
    for _ in 0..15 {
        chunks.push((30, vec![("ping", json!({"type":"ping"}))]));
    }
    let mut end = task7_tool()[1..].to_vec();
    end.extend(message_end());
    chunks.push((0, end));
    let up = FakeUpstream::start(vec![sse_reply(chunks)]).await;
    let t = start_pairs(&up, timing(200, 2000)).await;
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    assert_eq!(text.matches(PING).count(), 0, "{text}");
    assert_eq!(text.matches("event: ping").count(), 15);
    assert_eq!(tool_starts(&events(&text)), 1);
}

#[tokio::test]
async fn hold_limit_fails_closed() {
    let mut rest = task7_tool()[2..].to_vec();
    rest.extend(message_end());
    let up = FakeUpstream::start(vec![sse_reply(vec![
        (
            0,
            vec![
                message_start(),
                task7_tool()[0].clone(),
                task7_tool()[1].clone(),
            ],
        ),
        (600, rest),
    ])])
    .await;
    let t = start_pairs(&up, timing(50, 200)).await;
    let sent = Instant::now();
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    assert!(sent.elapsed() < ms(550), "{:?}", sent.elapsed());
    let evs = events(&text);
    let message = "a tool call took longer than 0.2 s to arrive and was not passed on";
    assert_eq!(errors(&evs), [format!("Tacita: {message}")]);
    assert_eq!(evs.last().unwrap().0, "error");
    assert!(!text.contains(r#""type":"tool_use""#), "{text}");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["tacita_error"], message);
}

#[tokio::test]
async fn hold_deadline_restarts_with_each_hold() {
    let mut last = vec![tool_delta(2, "{}"), block_stop(2)];
    last.extend(message_end());
    let up = FakeUpstream::start(vec![sse_reply(vec![
        (0, vec![message_start(), tool_start(0)]),
        (250, vec![tool_delta(0, "{}"), block_stop(0)]),
        (
            400,
            vec![
                text_start(1),
                text_delta(1, "mark"),
                block_stop(1),
                tool_start(2),
            ],
        ),
        (250, last),
    ])])
    .await;
    // 900 ms from the first tool start to the second tool stop, but no
    // single hold lasts 600 ms.
    let t = start_pairs(&up, timing(50, 600)).await;
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    let evs = events(&text);
    assert!(errors(&evs).is_empty(), "{text}");
    assert_eq!(tool_starts(&evs), 2);
    // Nothing is held between the first tool call and `mark`: no ping.
    let gap_start = text.find("toolu_0").unwrap();
    let gap_end = text.find("mark").unwrap();
    let gap = &text[gap_start..gap_end];
    assert_eq!(gap.matches(PING).count(), 0, "{gap}");
    let second = &text[gap_end..text.find("toolu_2").unwrap()];
    assert!(second.matches(PING).count() >= 1, "{second}");
}

#[tokio::test]
async fn stream_cut_mid_tool_drops_the_tool() {
    let up = FakeUpstream::start(vec![Reply::sse(sse(&[
        message_start(),
        text_start(0),
        text_delta(0, "see client_x0"),
        block_stop(0),
        tool_start(1),
        tool_delta(1, "{\"file_path\": \"/a"),
    ]))])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let text = text_body(post(&t, body_with_text("hi")).await).await;
    let evs = events(&text);
    assert_eq!(text_of(&evs), "see client_x0");
    assert!(!text.contains(r#""type":"tool_use""#), "{text}");
    assert_eq!(evs.last().unwrap().0, "error");
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1);
    assert!(
        errs[0].starts_with("Tacita: ") && errs[0].contains("cut off"),
        "{errs:?}"
    );
    let m = &meta_lines(&t, 1).await[0];
    assert!(
        m["tacita_error"].as_str().unwrap().contains("cut off"),
        "{m}"
    );
    assert_eq!(m["dangling"], 1);
}

#[tokio::test]
async fn unreadable_json_reply_fails_closed() {
    let up = FakeUpstream::start(vec![Reply::status(
        200,
        &[("content-type", "application/json")],
        r#"{"content":[{"type":"text","text":"see client_x01"#,
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let msg = bad_gateway(post(&t, body_with_text("hi")).await, false).await;
    assert!(
        msg.starts_with("Tacita: could not restore the reply: "),
        "{msg}"
    );
    assert!(!msg.contains("client_x01") && !msg.contains("see"), "{msg}");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 502);
    assert_eq!(
        m["tacita_error"].as_str().unwrap(),
        msg.strip_prefix("Tacita: ").unwrap()
    );
}

#[tokio::test]
async fn compressed_stream_fails_closed() {
    let up = FakeUpstream::start(vec![Reply::status(
        200,
        &[
            ("content-type", "text/event-stream"),
            ("content-encoding", "gzip"),
        ],
        vec![0x1f, 0x8b, 8, 0],
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let msg = bad_gateway(post(&t, body_with_text("hi")).await, false).await;
    assert_eq!(
        msg,
        "Tacita: compressed stream from upstream; refusing to pass it on unrestored"
    );
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 502);
    assert!(m["tacita_error"].is_string());
}

#[tokio::test]
async fn compressed_json_fails_closed_and_identity_is_accepted() {
    let message = r#"{"type":"message","content":[{"type":"text","text":"see client_x01"}]}"#;
    let up = FakeUpstream::start(vec![
        Reply::status(
            200,
            &[
                ("content-type", "application/json"),
                ("content-encoding", "GZIP"),
            ],
            message,
        ),
        Reply::status(
            200,
            &[
                ("content-type", "application/json"),
                ("content-encoding", "Identity"),
            ],
            message,
        ),
    ])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let msg = bad_gateway(post(&t, body_with_text("hi")).await, false).await;
    assert!(
        msg.starts_with("Tacita: could not restore the reply: "),
        "{msg}"
    );
    let resp = post(&t, body_with_text("hi")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        text_body(resp).await,
        message.replace("client_x01", "acmewidgets")
    );
    let m = meta_lines(&t, 2).await;
    assert!(m[0]["tacita_error"].is_string());
    assert_eq!(m[1]["tacita_error"], Value::Null);
}

#[tokio::test]
async fn media_types_match_on_type_and_subtype() {
    let sse_body = sse(&[
        text_start(0),
        text_delta(0, "see client_x01"),
        block_stop(0),
        ("message_stop", json!({"type":"message_stop"})),
    ]);
    let message = r#"{"type":"message","content":[{"type":"text","text":"see client_x01"}]}"#;
    let up = FakeUpstream::start(vec![
        Reply::status(
            200,
            &[("content-type", "Text/Event-Stream; charset=utf-8")],
            sse_body,
        ),
        Reply::status(
            200,
            &[("content-type", "Application/JSON; charset=utf-8")],
            message,
        ),
    ])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let evs = events(&text_body(post(&t, body_with_text("hi")).await).await);
    assert_eq!(text_of(&evs), "see acmewidgets");
    let resp = post(&t, body_with_text("hi")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        text_body(resp).await,
        message.replace("client_x01", "acmewidgets")
    );
}

#[tokio::test]
async fn replies_neither_sse_nor_json_fail_closed() {
    let up = FakeUpstream::start(vec![
        Reply::status(200, &[], "see client_x01"),
        Reply::status(200, &[("content-type", "text/plain")], "see client_x01"),
        Reply::status(
            200,
            &[
                ("content-type", "application/json"),
                ("content-type", "application/json"),
            ],
            r#"{"content":[{"type":"text","text":"see client_x01"}]}"#,
        ),
    ])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    for _ in 0..3 {
        let msg = bad_gateway(post(&t, body_with_text("hi")).await, false).await;
        assert!(
            msg.starts_with("Tacita: could not restore the reply: "),
            "{msg}"
        );
        assert!(!msg.contains("client_x01"), "{msg}");
    }
    let m = meta_lines(&t, 3).await;
    assert!(m.iter().all(|l| l["tacita_error"].is_string()));
}

#[tokio::test]
async fn block_probe_blocks_before_forwarding() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let t = start_with(CONFIG_OFF, &up, Timing::default()).await;
    let resp = post(&t, body_with_text("please TACITA_BLOCK_PROBE now")).await;
    let msg = tacita_error(resp, 400).await;
    assert_eq!(
        msg,
        "Tacita: block probe: this request was blocked on purpose (Step 0)"
    );
    assert_eq!(up.count(), 0);
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["route"], "messages");
    assert_eq!(m["tacita_error"], "block probe");
    assert_eq!(m["status"], 400);
}

#[tokio::test]
async fn block_probe_comes_before_scrub() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let t = start_pairs(&up, Timing::default()).await;
    // Not JSON at all: the scrub would refuse it, but the probe wins.
    let msg = tacita_error(post(&t, "TACITA_BLOCK_PROBE {").await, 400).await;
    assert_eq!(
        msg,
        "Tacita: block probe: this request was blocked on purpose (Step 0)"
    );
    assert_eq!(up.count(), 0);
    assert_eq!(meta_lines(&t, 1).await[0]["tacita_error"], "block probe");
}

#[tokio::test]
async fn without_a_block_probe_nothing_is_blocked() {
    let config = CONFIG_OFF.replace("block_probe = \"TACITA_BLOCK_PROBE\"\n", "");
    assert_ne!(config, CONFIG_OFF);
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let t = start_with(&config, &up, Timing::default()).await;
    let resp = post(&t, body_with_text("please TACITA_BLOCK_PROBE now")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(up.count(), 1);
}

#[tokio::test]
async fn stream_probe_ends_stream_with_error() {
    let up = FakeUpstream::start(vec![Reply::Echo]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let text = text_body(post(&t, body_with_text("TACITA_STREAM_PROBE")).await).await;
    let evs = events(&text);
    let message = "stream probe: this reply was stopped on purpose (Step 0)";
    assert_eq!(errors(&evs), [format!("Tacita: {message}")]);
    assert_eq!(evs.last().unwrap().0, "error");
    assert!(!text_of(&evs).contains("TACITA_STREAM_PROBE"));
    assert_eq!(meta_lines(&t, 1).await[0]["tacita_error"], message);
}

#[tokio::test]
async fn stream_probe_found_when_the_reply_ends_is_an_error() {
    let config = CONFIG_PAIRS.replace(
        "stream_probe = \"TACITA_STREAM_PROBE\"",
        "stream_probe = \"see client_x0\"",
    );
    assert_ne!(config, CONFIG_PAIRS);
    // The carry `client_x0` is only flushed when the stream ends.
    let up = FakeUpstream::start(vec![Reply::sse(sse(&[
        message_start(),
        text_start(0),
        text_delta(0, "see client_x0"),
    ]))])
    .await;
    let t = start_with(&config, &up, Timing::default()).await;
    let evs = events(&text_body(post(&t, body_with_text("hi")).await).await);
    let errs = errors(&evs);
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(errs[0].starts_with("Tacita: stream probe"), "{errs:?}");
    assert!(errs[1].contains("cut off"), "{errs:?}");
    assert_eq!(text_of(&evs), "see ");
    let m = &meta_lines(&t, 1).await[0];
    assert!(
        m["tacita_error"]
            .as_str()
            .unwrap()
            .starts_with("stream probe"),
        "{m}"
    );
}

#[tokio::test]
async fn stream_probe_is_inert_when_protection_is_off() {
    let up = FakeUpstream::start(vec![Reply::Echo]).await;
    let t = start_with(CONFIG_OFF, &up, Timing::default()).await;
    let evs = events(&text_body(post(&t, body_with_text("TACITA_STREAM_PROBE")).await).await);
    assert_eq!(text_of(&evs), "TACITA_STREAM_PROBE");
    assert!(errors(&evs).is_empty());
}

#[tokio::test]
async fn bad_event_ends_stream_with_error() {
    let up = FakeUpstream::start(vec![Reply::sse(sse(&[
        message_start(),
        ("content_block_delta", json!({"type":"message_stop"})),
        ("message_stop", json!({"type":"message_stop"})),
    ]))])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let evs = events(&text_body(post(&t, body_with_text("hi")).await).await);
    assert_eq!(evs[0].0, "message_start");
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].starts_with("Tacita: unexpected event from upstream: "),
        "{errs:?}"
    );
    assert_eq!(evs.last().unwrap().0, "error");
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(
        m["tacita_error"].as_str().unwrap(),
        errs[0].strip_prefix("Tacita: ").unwrap()
    );
}

#[tokio::test]
async fn unparseable_stream_ends_with_error() {
    let mut body = sse(&[message_start()]);
    body.extend_from_slice(b"event: content_block_delta\ndata: \xff\n\n");
    body.extend(stop_only());
    let up = FakeUpstream::start(vec![Reply::status(
        200,
        &[("content-type", "text/event-stream")],
        body,
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let text = text_body(post(&t, body_with_text("hi")).await).await;
    let evs = events(&text);
    assert_eq!(evs.len(), 2, "{text}");
    assert_eq!(evs[0].0, "message_start");
    assert_eq!(errors(&evs), ["Tacita: invalid UTF-8 in SSE event"]);
    assert_eq!(
        meta_lines(&t, 1).await[0]["tacita_error"],
        "invalid UTF-8 in SSE event"
    );
}

#[tokio::test]
async fn concurrent_sessions_restore_independently() {
    // Each event is its own chunk 20 ms apart: the 8 replies overlap.
    let up = FakeUpstream::start(vec![Reply::EchoSlow { delay_ms: 20 }]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let runs = (0..8).map(|i| {
        let t = &t;
        async move {
            let mut resp = post(t, body_with_text(&format!("n{i} acmewidgets"))).await;
            let mut bytes = Vec::new();
            let mut first = None;
            while let Some(chunk) = resp.chunk().await.unwrap() {
                first.get_or_insert_with(Instant::now);
                bytes.extend_from_slice(&chunk);
            }
            let text = String::from_utf8(bytes).unwrap();
            (i, text_of(&events(&text)), first.unwrap(), Instant::now())
        }
    });
    let results = futures_util::future::join_all(runs).await;
    for (i, got, _, _) in &results {
        assert_eq!(*got, format!("n{i} acmewidgets"));
    }
    // Every reply had started before any reply ended.
    let last_first = results.iter().map(|r| r.2).max().unwrap();
    let first_end = results.iter().map(|r| r.3).min().unwrap();
    assert!(last_first < first_end);
    assert_eq!(up.count(), 8);
    let lines = meta_lines(&t, 8).await;
    assert!(lines.iter().all(|m| m["restored"] == 1), "{lines:?}");
}

#[tokio::test]
async fn non_streaming_json_restored() {
    let message = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"see client_x01"}],"stop_reason":"end_turn"}"#;
    let up = FakeUpstream::start(vec![Reply::status(
        200,
        &[
            ("content-type", "application/json"),
            ("request-id", "req_9"),
        ],
        message,
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let resp = post(&t, body_with_text("hi")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["request-id"], "req_9");
    let length = resp.headers().get("content-length").cloned();
    let body = text_body(resp).await;
    assert_eq!(body, message.replace("client_x01", "acmewidgets"));
    if let Some(length) = length {
        assert_eq!(length.to_str().unwrap(), body.len().to_string());
    }
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["restored"], 1);
    assert_eq!(m["status"], 200);
    assert_eq!(m["tacita_error"], Value::Null);
}

#[tokio::test]
async fn error_bodies_not_restored() {
    let error =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad client_x01"}}"#;
    let up = FakeUpstream::start(vec![Reply::status(
        400,
        &[
            ("content-type", "application/json"),
            ("x-should-retry", "false"),
        ],
        error,
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let resp = post(&t, body_with_text("hi")).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(resp.headers()["x-should-retry"], "false");
    assert_eq!(resp.bytes().await.unwrap(), error.as_bytes());
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["status"], 400);
    assert_eq!(m["tacita_error"], Value::Null);
    assert_eq!(m["upstream_error"], "invalid_request_error: bad client_x01");
}

#[tokio::test]
async fn unreadable_request_is_refused_without_quoting_it() {
    let up = FakeUpstream::start(vec![Reply::sse(stop_only())]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let resp = post(&t, r#"{"acmewidgets":1,"acmewidgets":2}"#).await;
    let msg = tacita_error(resp, 400).await;
    assert_eq!(
        msg,
        "Tacita: could not read this request: request body has an object with a repeated key"
    );
    assert_eq!(up.count(), 0);
    assert_eq!(
        meta_lines(&t, 1).await[0]["tacita_error"],
        "could not read this request: request body has an object with a repeated key"
    );
}

#[tokio::test]
async fn meta_line_written_once_when_the_client_leaves_mid_hold() {
    let mut chunks = vec![(0, vec![message_start(), tool_start(1)])];
    for _ in 0..20 {
        chunks.push((200, vec![("ping", json!({"type":"ping"}))]));
    }
    let up = FakeUpstream::start(vec![sse_reply(chunks)]).await;
    let t = start_pairs(&up, Timing::default()).await;
    let mut resp = post(&t, body_with_text("hi")).await;
    resp.chunk().await.unwrap().unwrap();
    drop(resp);
    let m = meta_lines(&t, 1).await;
    assert_eq!(m[0]["status"], 200);
    assert_eq!(m[0]["restored"], 0);
    assert_eq!(m[0]["dangling"], 0);
    tokio::time::sleep(ms(300)).await;
    assert_eq!(wait_for_lines(&t.log_path(), 2).await.len(), 1);
}

#[tokio::test]
async fn json_reply_broken_midway_is_a_retryable_502() {
    // The declared length is never reached: the upstream drops mid-body.
    let up = FakeUpstream::start(vec![Reply::chunks(
        200,
        &[
            ("content-type", "application/json"),
            ("content-length", "100000"),
        ],
        vec![(
            0,
            br#"{"content":[{"type":"text","text":"see client_x01"#.to_vec(),
        )],
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let msg = bad_gateway(post(&t, body_with_text("hi")).await, true).await;
    assert!(
        msg.starts_with("Tacita: could not restore the reply: it broke off: "),
        "{msg}"
    );
    assert!(!msg.contains("client_x01"), "{msg}");
    assert_eq!(meta_lines(&t, 1).await[0]["status"], 502);
}

#[tokio::test]
async fn upstream_error_event_ends_the_stream() {
    let overloaded =
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}});
    let up = FakeUpstream::start(vec![sse_reply(vec![
        (0, vec![message_start(), tool_start(1), tool_delta(1, "{")]),
        (200, vec![("error", overloaded)]),
        // The upstream stays open long after its error.
        (3000, vec![("ping", json!({"type":"ping"}))]),
    ])])
    .await;
    let t = start_pairs(&up, timing(50, 2000)).await;
    let sent = Instant::now();
    let text = text_body(post(&t, body_with_text("read it")).await).await;
    assert!(sent.elapsed() < ms(1500), "{:?}", sent.elapsed());
    let evs = events(&text);
    assert_eq!(errors(&evs), ["Overloaded"]);
    assert_eq!(evs.last().unwrap().0, "error");
    let after = &text[text.find("event: error").unwrap()..];
    assert!(!after.contains("event: ping"), "{after}");
    assert!(text.matches(PING).count() >= 1, "{text}");
    assert_eq!(tool_starts(&evs), 0);
    let m = &meta_lines(&t, 1).await[0];
    assert_eq!(m["upstream_error"], "overloaded_error: Overloaded");
    assert_eq!(m["tacita_error"], Value::Null);
}

#[tokio::test]
async fn upstream_reset_is_logged_with_its_cause() {
    let up = FakeUpstream::start(vec![Reply::chunks(
        200,
        &[
            ("content-type", "text/event-stream"),
            ("content-length", "100000"),
        ],
        // The events are flushed; the upstream then drops before the
        // declared length is reached.
        vec![
            (
                0,
                sse(&[message_start(), text_start(0), text_delta(0, "hi")]),
            ),
            (100, b": more\n\n".to_vec()),
        ],
    )])
    .await;
    let t = start_pairs(&up, Timing::default()).await;
    let evs = events(&text_body(post(&t, body_with_text("hi")).await).await);
    let cut = "the reply from upstream was cut off; any tool call in progress was not passed on";
    assert_eq!(text_of(&evs), "hi");
    assert_eq!(errors(&evs), [format!("Tacita: {cut}")]);
    let m = &meta_lines(&t, 1).await[0];
    let logged = m["tacita_error"].as_str().unwrap();
    assert!(logged.starts_with(&format!("{cut}: ")), "{logged}");
    assert!(
        logged.len() > cut.len() + 2 && logged.len() <= 200,
        "{logged}"
    );
}
