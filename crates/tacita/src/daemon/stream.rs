//! The protected reply stream: upstream SSE events through the restorer,
//! Tacita's own pings while a tool call is held, and a visible error event
//! whenever the reply cannot be passed on restored.

use std::convert::Infallible;
use std::pin::Pin;

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tacita_core::errors::error_event;
use tacita_core::restore::Restorer;
use tacita_core::sse::{SseEvent, SseParser};
use tokio::time::{Instant, timeout_at};

use super::metalog::observe;
use super::{MetaHandle, Timing, bounded};

/// Tacita's own keep-alive, sent only while a tool call is held.
pub const PING: &[u8] = b"event: ping\ndata: {\"type\": \"ping\"}\n\n";

const CUT_OFF: &str =
    "the reply from upstream was cut off; any tool call in progress was not passed on";

type Upstream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The client's body for a 2xx event stream: every upstream event goes
/// through `restorer`, and every way the stream can fail ends it with an
/// `error` event. The meta line is written when the body is dropped.
pub fn restoring_body(
    upstream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    restorer: Restorer,
    timing: Timing,
    meta: MetaHandle,
) -> Body {
    let state = Restoring {
        upstream: Box::pin(upstream),
        parser: SseParser::default(),
        restorer,
        timing,
        m: meta,
        hold_deadline: None,
        last_sent: Instant::now(),
        complete: false,
        done: false,
    };
    let stream = futures_util::stream::unfold(state, |mut s| async move {
        let chunk = s.next_chunk().await?;
        Some((Ok::<_, Infallible>(chunk), s))
    });
    Body::from_stream(stream)
}

struct Restoring {
    upstream: Upstream,
    parser: SseParser,
    restorer: Restorer,
    timing: Timing,
    m: MetaHandle,
    /// When the current hold must have ended; `None` while nothing is held.
    hold_deadline: Option<Instant>,
    /// When bytes last went to the client.
    last_sent: Instant,
    /// `message_stop` or an upstream `error` event was passed on.
    complete: bool,
    done: bool,
}

impl Restoring {
    /// The next bytes for the client; `None` ends the stream.
    async fn next_chunk(&mut self) -> Option<Bytes> {
        while !self.done {
            let item = match self.hold_deadline {
                None => self.upstream.next().await,
                Some(deadline) => {
                    let ping_at = self.last_sent + self.timing.ping_every;
                    match timeout_at(ping_at.min(deadline), self.upstream.next()).await {
                        Ok(item) => item,
                        Err(_) if Instant::now() >= deadline => return Some(self.hold_expired()),
                        Err(_) => return Some(self.sent(PING.to_vec())),
                    }
                }
            };
            let out = match item {
                Some(Ok(chunk)) => {
                    if self.hold_deadline.is_some_and(|d| Instant::now() >= d) {
                        return Some(self.hold_expired());
                    }
                    self.feed(&chunk)
                }
                Some(Err(_)) | None => self.upstream_ended(),
            };
            if !out.is_empty() {
                return Some(self.sent(out));
            }
        }
        None
    }

    fn sent(&mut self, out: Vec<u8>) -> Bytes {
        self.last_sent = Instant::now();
        Bytes::from(out)
    }

    /// Parses `chunk` and pushes its events through the restorer.
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        // `push` returns the events before a parse error and the error on
        // the next call; the empty push surfaces it now.
        let (events, parse_error) = match self.parser.push(chunk) {
            Ok(events) => (events, self.parser.push(&[]).err()),
            Err(e) => (Vec::new(), Some(e)),
        };
        let mut out = Vec::new();
        for ev in events {
            match self.restorer.push(ev) {
                Ok(emitted) => self.pass_on(emitted, &mut out),
                Err(e) => {
                    self.fail(&e.to_string(), &mut out);
                    return out;
                }
            }
            self.track_hold();
        }
        if let Some(e) = parse_error {
            self.fail(&e.to_string(), &mut out);
        }
        out
    }

    /// The hold age runs from the tool call's start, one deadline per hold.
    fn track_hold(&mut self) {
        match (self.restorer.is_holding(), self.hold_deadline) {
            (true, None) => self.hold_deadline = Some(Instant::now() + self.timing.hold_limit),
            (false, Some(_)) => self.hold_deadline = None,
            _ => {}
        }
    }

    fn pass_on(&mut self, events: Vec<SseEvent>, out: &mut Vec<u8>) {
        for ev in events {
            observe(&mut self.m.meta, &ev);
            if matches!(ev.event.as_deref(), Some("message_stop" | "error")) {
                self.complete = true;
            }
            out.extend_from_slice(&ev.raw);
        }
    }

    /// Ends the stream with Tacita's own error event.
    fn fail(&mut self, reason: &str, out: &mut Vec<u8>) {
        let reason = bounded(reason);
        out.extend_from_slice(&error_event(reason).raw);
        self.m
            .meta
            .tacita_error
            .get_or_insert_with(|| reason.to_string());
        self.done = true;
    }

    fn hold_expired(&mut self) -> Bytes {
        let reason = format!(
            "a tool call took longer than {} s to arrive and was not passed on",
            self.timing.hold_limit.as_secs_f64()
        );
        let mut out = Vec::new();
        self.fail(&reason, &mut out);
        self.sent(out)
    }

    /// The upstream ended or broke: flush text carries, drop a held tool
    /// call, and say so unless the reply was complete. An unfinished event
    /// left in the parser is dropped.
    fn upstream_ended(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in self.restorer.finish() {
            // `finish` makes no `error` event but its own (a probe hit).
            if ev.event.as_deref() == Some("error") {
                out.extend_from_slice(&ev.raw);
                if let Some(reason) = own_error_reason(&ev) {
                    self.m.meta.tacita_error.get_or_insert(reason);
                }
            } else {
                self.pass_on(vec![ev], &mut out);
            }
        }
        if !self.complete {
            self.fail(CUT_OFF, &mut out);
        }
        self.done = true;
        out
    }
}

/// The message of a Tacita error event, without the `Tacita: ` prefix.
fn own_error_reason(ev: &SseEvent) -> Option<String> {
    let v: Value = serde_json::from_str(&ev.data).ok()?;
    let message = v.pointer("/error/message")?.as_str()?;
    Some(bounded(message.strip_prefix("Tacita: ").unwrap_or(message)).to_string())
}

impl Drop for Restoring {
    /// Runs before the `MetaHandle` field writes the line.
    fn drop(&mut self) {
        self.m.meta.restored = Some(self.restorer.restored());
        self.m.meta.dangling = Some(self.restorer.dangling());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::metalog::{MetaLog, RequestMeta};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tacita_core::pairs::{Pair, Pairs};
    use tacita_core::restore::Limits;

    fn sse(events: &[(&str, Value)]) -> Vec<u8> {
        events
            .iter()
            .flat_map(|(n, d)| format!("event: {n}\ndata: {d}\n\n").into_bytes())
            .collect()
    }

    fn pairs() -> Arc<Pairs> {
        Arc::new(
            Pairs::new(vec![Pair {
                real: "acmewidgets".into(),
                standin: "client_x01".into(),
            }])
            .unwrap(),
        )
    }

    /// Runs `chunks` through `restoring_body`; returns the client bytes and
    /// the meta line.
    async fn run(chunks: Vec<Vec<u8>>, restorer: Restorer) -> (String, Value) {
        let home = tempfile::tempdir().unwrap();
        let m = MetaHandle {
            meta: RequestMeta::default(),
            log: Arc::new(MetaLog::open(home.path()).unwrap()),
            start: std::time::Instant::now(),
        };
        let upstream = futures_util::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))));
        let body = restoring_body(upstream, restorer, Timing::default(), m);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let line = std::fs::read_to_string(home.path().join("logs/requests.jsonl")).unwrap();
        let lines: Vec<&str> = line.lines().collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        (
            String::from_utf8(bytes.to_vec()).unwrap(),
            serde_json::from_str(lines[0]).unwrap(),
        )
    }

    #[test]
    fn ping_bytes_are_exact() {
        assert_eq!(PING, b"event: ping\ndata: {\"type\": \"ping\"}\n\n");
    }

    #[tokio::test]
    async fn a_tool_call_over_the_limit_ends_the_stream_with_an_error() {
        let start = sse(&[(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
        )]);
        let delta = sse(&[(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\": \"/a/client_x01\"}"}}),
        )]);
        let restorer = Restorer::new(pairs(), Limits { max_tool_bytes: 8 }, None);
        let (out, meta) = run(vec![start, delta], restorer).await;
        let message = "a tool call was larger than the limit and was not passed on";
        let expected = tacita_core::errors::error_event(message).raw;
        assert_eq!(out.as_bytes(), expected, "{out}");
        assert_eq!(meta["tacita_error"], message);
        assert_eq!(meta["restored"], 0);
        assert_eq!(meta["dangling"], 0);
    }

    #[tokio::test]
    async fn a_complete_reply_ends_without_an_error() {
        let evs = sse(&[
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi client_x0"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]);
        let (out, meta) = run(vec![evs], Restorer::new(pairs(), Limits::default(), None)).await;
        assert!(out.contains(r#""text":"hi ""#), "{out}");
        assert!(out.contains(r#""text":"acmewidgets""#), "{out}");
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        assert!(!out.contains("event: error"), "{out}");
        assert_eq!(meta["tacita_error"], Value::Null);
        assert_eq!(meta["restored"], 1);
        assert_eq!(meta["dangling"], 0);
    }
}
