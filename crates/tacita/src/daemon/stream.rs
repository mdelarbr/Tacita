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
use super::{MetaHandle, Timing, bounded, short_reason};

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
                    self.m.cap_upstream(&chunk);
                    if self.hold_deadline.is_some_and(|d| Instant::now() >= d) {
                        return Some(self.hold_expired());
                    }
                    self.feed(&chunk)
                }
                Some(Err(e)) => self.upstream_ended(Some(short_reason(&e))),
                None => self.upstream_ended(None),
            };
            if !out.is_empty() {
                return Some(self.sent(out));
            }
        }
        None
    }

    fn sent(&mut self, out: Vec<u8>) -> Bytes {
        self.last_sent = Instant::now();
        let out = Bytes::from(out);
        self.m.cap_client(&out);
        out
    }

    /// Parses `chunk` and pushes its events through the restorer.
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        // `push` returns the events before a parse error and the error on
        // the next call; the empty push surfaces it now. Whatever events it
        // returns are forwarded too, never dropped.
        let (events, parse_error) = match self.parser.push(chunk) {
            Ok(mut events) => match self.parser.push(&[]) {
                Ok(more) => {
                    events.extend(more);
                    (events, None)
                }
                Err(e) => (events, Some(e)),
            },
            Err(e) => (Vec::new(), Some(e)),
        };
        let mut out = Vec::new();
        for ev in events {
            match self.restorer.push(ev) {
                Ok(emitted) => {
                    if self.pass_on(emitted, &mut out) {
                        // The upstream's own error ends the reply: drop any
                        // held tool call and stop.
                        self.finish_restorer(&mut out);
                        self.done = true;
                        return out;
                    }
                }
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

    /// Observes and appends upstream-derived events; returns whether one
    /// was an upstream `error` event.
    fn pass_on(&mut self, events: Vec<SseEvent>, out: &mut Vec<u8>) -> bool {
        let mut upstream_error = false;
        for ev in events {
            observe(&mut self.m.meta, &ev);
            match ev.event.as_deref() {
                Some("message_stop") => self.complete = true,
                Some("error") => {
                    self.complete = true;
                    upstream_error = true;
                }
                _ => {}
            }
            out.extend_from_slice(&ev.raw);
        }
        upstream_error
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

    /// Flushes text carries and drops a held tool call.
    fn finish_restorer(&mut self, out: &mut Vec<u8>) {
        for ev in self.restorer.finish() {
            // `finish` makes no `error` event but its own (a probe hit).
            if ev.event.as_deref() == Some("error") {
                out.extend_from_slice(&ev.raw);
                if let Some(reason) = own_error_reason(&ev) {
                    self.m.meta.tacita_error.get_or_insert(reason);
                }
            } else {
                self.pass_on(vec![ev], out);
            }
        }
    }

    /// The upstream ended, or broke with `cause`: flush text carries, drop a
    /// held tool call, and say so unless the reply was complete. The log
    /// also gets the cause. An unfinished event left in the parser is
    /// dropped.
    fn upstream_ended(&mut self, cause: Option<String>) -> Vec<u8> {
        let mut out = Vec::new();
        self.finish_restorer(&mut out);
        if !self.complete {
            self.fail(CUT_OFF, &mut out);
            let logged = &mut self.m.meta.tacita_error;
            if let Some(cause) = cause
                && logged.as_deref() == Some(CUT_OFF)
            {
                *logged = Some(bounded(&format!("{CUT_OFF}: {cause}")).to_string());
            }
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
        let upstream = futures_util::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))));
        run_stream(upstream, restorer, Timing::default()).await
    }

    async fn run_stream(
        upstream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
        restorer: Restorer,
        timing: Timing,
    ) -> (String, Value) {
        let home = tempfile::tempdir().unwrap();
        let m = MetaHandle {
            meta: RequestMeta::default(),
            log: Arc::new(MetaLog::open(home.path()).unwrap()),
            start: std::time::Instant::now(),
            cap: None,
        };
        let body = restoring_body(upstream, restorer, timing, m);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let line = std::fs::read_to_string(home.path().join("logs/requests.jsonl")).unwrap();
        let lines: Vec<&str> = line.lines().collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        (
            String::from_utf8(bytes.to_vec()).unwrap(),
            serde_json::from_str(lines[0]).unwrap(),
        )
    }

    #[tokio::test]
    async fn hold_limit_is_checked_on_each_chunk_when_the_upstream_never_waits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let mut chunks = vec![sse(&[(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
        )])];
        for _ in 0..20 {
            chunks.push(sse(&[(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            )]));
        }
        chunks.push(sse(&[
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]));
        // Every poll is ready at once, after 30 ms of blocking work: the
        // timeout never gets to fire (it polls the upstream first) and no
        // ping is due within 10 s. Only the check made before each chunk is
        // pushed can end the hold.
        let pulled = Arc::new(AtomicUsize::new(0));
        let counter = pulled.clone();
        let upstream = futures_util::stream::iter(chunks).map(move |c| {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            Ok(Bytes::from(c))
        });
        let timing = Timing {
            ping_every: Duration::from_secs(10),
            hold_limit: Duration::from_millis(100),
            ..Timing::default()
        };
        let restorer = Restorer::new(pairs(), Limits::default(), None);
        let (out, meta) = run_stream(upstream, restorer, timing).await;
        let message = "a tool call took longer than 0.1 s to arrive and was not passed on";
        assert_eq!(out.as_bytes(), error_event(message).raw, "{out}");
        assert_eq!(meta["tacita_error"], message);
        // The start at 30 ms, then deltas at 60, 90, 120 and 150 ms: the
        // first chunk past the 130 ms deadline ends it, long before the end.
        let n = pulled.load(Ordering::SeqCst);
        assert!((4..10).contains(&n), "{n}");
    }

    #[tokio::test]
    async fn the_chunk_that_trips_the_hold_deadline_is_still_recorded() {
        use crate::daemon::record::{Capture, Recorder};
        use std::sync::Mutex;
        use std::time::Duration;
        let mut chunks = vec![sse(&[(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
        )])];
        for _ in 0..20 {
            chunks.push(sse(&[(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            )]));
        }
        let pulled: Arc<Mutex<Vec<u8>>> = Arc::default();
        let seen = pulled.clone();
        let upstream = futures_util::stream::iter(chunks).map(move |c| {
            std::thread::sleep(Duration::from_millis(30));
            seen.lock().unwrap().extend_from_slice(&c);
            Ok(Bytes::from(c))
        });
        let timing = Timing {
            ping_every: Duration::from_secs(10),
            hold_limit: Duration::from_millis(100),
            ..Timing::default()
        };
        let home = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();
        let rec = Arc::new(Recorder::new(&corpus.path().join("r")).unwrap());
        let m = MetaHandle {
            meta: RequestMeta::default(),
            log: Arc::new(MetaLog::open(home.path()).unwrap()),
            start: std::time::Instant::now(),
            cap: Some(Capture::new(rec, &Default::default(), Default::default())),
        };
        let restorer = Restorer::new(pairs(), Limits::default(), None);
        let body = restoring_body(upstream, restorer, timing, m);
        axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let file = corpus.path().join("r/000000-upstream.txt");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !corpus.path().join("r/000000-meta.json").exists() {
            assert!(std::time::Instant::now() < deadline, "not written");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pulled = pulled.lock().unwrap().clone();
        assert_eq!(std::fs::read(file).unwrap(), pulled);
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
