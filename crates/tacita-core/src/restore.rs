//! Restoring the provider's replies: stand-ins back to real values.
//!
//! Streamed text is restored as it arrives, holding back only a tail that
//! could still become a stand-in. A `tool_use` block is held until it stops,
//! then emitted once with its input restored. Thinking is never restored.

use crate::errors::error_event;
use crate::json::{Lit, ScanError, Seg, decode, encode, replace, scan, scan_tolerant, splice};
use crate::pairs::Pairs;
use crate::sse::SseEvent;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_tool_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_tool_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Messages never quote reply content, which can hold business names.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("unexpected event from upstream: {0}")]
    BadEvent(String),
    #[error("a tool call was larger than the limit and was not passed on")]
    ToolTooLarge,
    #[error("stream probe: this reply was stopped on purpose (Step 0)")]
    Probe,
}

fn bad(what: &str) -> RestoreError {
    RestoreError::BadEvent(what.to_string())
}

fn type_of(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn index_of(v: &Value) -> Option<u64> {
    v.get("index").and_then(Value::as_u64)
}

/// Adds `out` to the probe tail and reports a match. The tail keeps at most
/// `probe.len() - 1` bytes, starting on a char boundary: a match that ends in
/// a later delta starts on a char boundary inside that window.
fn probe_hit(probe: Option<&str>, tail: &mut String, out: &str) -> bool {
    let Some(probe) = probe else { return false };
    tail.push_str(out);
    if tail.contains(probe) {
        return true;
    }
    let mut cut = tail.len().saturating_sub(probe.len() - 1);
    while !tail.is_char_boundary(cut) {
        cut += 1;
    }
    tail.drain(..cut);
    false
}

/// Returns `ev` with the one `delta.<field>` string literal replaced by `text`,
/// every other byte of the data unchanged.
fn with_delta_field(ev: &SseEvent, field: &str, text: &str) -> Result<SseEvent, RestoreError> {
    let data = ev.data.as_bytes();
    let lits = scan(data).map_err(|_| bad("event data is not valid JSON"))?;
    let mut found = lits.iter().filter(|l| {
        !l.is_key
            && matches!(l.path.as_slice(), [Seg::Key(d), Seg::Key(f)] if d == "delta" && f == field)
    });
    let (Some(lit), None) = (found.next(), found.next()) else {
        return Err(bad("delta event without exactly one string to restore"));
    };
    let data = splice(data, vec![(lit.span.clone(), encode(text).into_bytes())]);
    let data = String::from_utf8(data).map_err(|_| bad("restored event is not UTF-8"))?;
    // Only deltas are rebuilt, and dispatch checked that their name is exactly this.
    Ok(SseEvent::build("content_block_delta", &data))
}

#[derive(Default)]
struct TextBlock {
    /// Restorable text not sent yet: a proper prefix of a stand-in.
    carry: String,
    /// The block's last `text_delta`, the template for flushing `carry`.
    last_delta: Option<SseEvent>,
    probe_tail: String,
}

impl TextBlock {
    /// Restores `carry + text` up to the hold point; returns the text to send
    /// now and the number of stand-ins replaced.
    fn delta(
        &mut self,
        pairs: &Pairs,
        probe: Option<&str>,
        text: &str,
    ) -> Result<(String, usize), RestoreError> {
        let mut s = std::mem::take(&mut self.carry);
        s.push_str(text);
        let h = pairs.hold_from(&s);
        let (out, n) = pairs.restore_str(&s[..h]);
        self.carry = s.split_off(h);
        if probe_hit(probe, &mut self.probe_tail, &out) {
            return Err(RestoreError::Probe);
        }
        Ok((out, n))
    }

    /// Sends the carry as a `text_delta` built from the block's last delta.
    fn flush(
        &mut self,
        pairs: &Pairs,
        probe: Option<&str>,
    ) -> Result<Option<(SseEvent, usize)>, RestoreError> {
        if self.carry.is_empty() {
            return Ok(None);
        }
        let carry = std::mem::take(&mut self.carry);
        let (out, n) = pairs.restore_str(&carry);
        if probe_hit(probe, &mut self.probe_tail, &out) {
            return Err(RestoreError::Probe);
        }
        let template = self
            .last_delta
            .as_ref()
            .ok_or_else(|| bad("text carry without a delta"))?;
        Ok(Some((with_delta_field(template, "text", &out)?, n)))
    }
}

struct HeldTool {
    index: u64,
    start: SseEvent,
    first_delta: Option<SseEvent>,
    /// Concatenated `partial_json` values.
    input: String,
}

/// The events Tacita handles. Every other event is forwarded untouched.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Start,
    Delta,
    Stop,
    MessageDelta,
    MessageStop,
    Error,
}

impl Kind {
    fn from_name(name: &str) -> Option<Kind> {
        Some(match name {
            "content_block_start" => Kind::Start,
            "content_block_delta" => Kind::Delta,
            "content_block_stop" => Kind::Stop,
            "message_delta" => Kind::MessageDelta,
            "message_stop" => Kind::MessageStop,
            "error" => Kind::Error,
            _ => return None,
        })
    }
}

pub struct Restorer {
    pairs: Arc<Pairs>,
    limits: Limits,
    probe: Option<String>,
    texts: BTreeMap<u64, TextBlock>,
    held: Option<HeldTool>,
    restored: usize,
    dangling: usize,
}

impl Restorer {
    /// An empty `fail_on_text` counts as no probe.
    pub fn new(pairs: Arc<Pairs>, limits: Limits, fail_on_text: Option<String>) -> Restorer {
        Restorer {
            pairs,
            limits,
            probe: fail_on_text.filter(|p| !p.is_empty()),
            texts: BTreeMap::new(),
            held: None,
            restored: 0,
            dangling: 0,
        }
    }

    pub fn push(&mut self, ev: SseEvent) -> Result<Vec<SseEvent>, RestoreError> {
        // Empty data never parses: comment-only blocks and stray blank lines
        // (no name) go out raw, a handled name without data is refused.
        let parsed = serde_json::from_str::<Value>(&ev.data)
            .ok()
            .filter(Value::is_object);
        let by_type = parsed.as_ref().and_then(type_of).and_then(Kind::from_name);
        let by_name = ev.event.as_deref().and_then(Kind::from_name);
        // Claude Code picks events by name and parses the data of handled
        // names only, so for those the name and the data's type must agree.
        let (kind, v) = match (by_name, by_type, parsed) {
            (None, None, _) => return Ok(vec![ev]),
            (Some(n), Some(t), Some(v)) if n == t => (n, v),
            (Some(_), _, _) => return Err(bad("an event's data does not match its name")),
            (None, Some(_), _) => return Err(bad("an event's name does not match its data")),
        };
        match kind {
            Kind::Start => self.start(ev, &v),
            Kind::Delta => self.delta(ev, &v),
            Kind::Stop => self.stop(ev, &v),
            Kind::MessageDelta | Kind::MessageStop if self.held.is_some() => {
                Err(bad("the message ended while a tool call was still open"))
            }
            Kind::MessageDelta | Kind::MessageStop | Kind::Error => {
                let mut out = self.flush_all()?;
                out.push(ev);
                Ok(out)
            }
        }
    }

    /// Flushes text carries. A tool block still held is dropped. A failure
    /// while flushing (the probe) replaces the flushed text with an error event.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        self.held = None;
        let out = self
            .flush_all()
            .unwrap_or_else(|e| vec![error_event(&e.to_string())]);
        self.texts.clear();
        out
    }

    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Stand-ins replaced so far, in text and tool input.
    pub fn restored(&self) -> usize {
        self.restored
    }

    /// Non-empty text carries flushed so far.
    pub fn dangling(&self) -> usize {
        self.dangling
    }

    fn start(&mut self, ev: SseEvent, v: &Value) -> Result<Vec<SseEvent>, RestoreError> {
        if self.held.is_some() {
            return Err(bad("a block started while a tool call was still open"));
        }
        let index = index_of(v);
        if index.is_some_and(|i| self.texts.contains_key(&i)) {
            return Err(bad("a block started at the index of an open text block"));
        }
        match v.get("content_block").and_then(type_of) {
            Some("text") => {
                let index = index.ok_or_else(|| bad("text block without an index"))?;
                self.texts.insert(index, TextBlock::default());
                Ok(vec![ev])
            }
            Some("tool_use") => {
                let index = index.ok_or_else(|| bad("tool call without an index"))?;
                self.held = Some(HeldTool {
                    index,
                    start: ev,
                    first_delta: None,
                    input: String::new(),
                });
                Ok(Vec::new())
            }
            _ => Ok(vec![ev]),
        }
    }

    fn delta(&mut self, ev: SseEvent, v: &Value) -> Result<Vec<SseEvent>, RestoreError> {
        let index = index_of(v);
        let delta_type = v.get("delta").and_then(type_of);
        if let Some(held) = &mut self.held {
            if index != Some(held.index) {
                return Err(bad(
                    "a delta for another block arrived while a tool call was open",
                ));
            }
            if delta_type != Some("input_json_delta") {
                return Err(bad("a tool call got a delta that is not input_json_delta"));
            }
            let part = v["delta"].get("partial_json").and_then(Value::as_str);
            let part = part.ok_or_else(|| bad("input_json_delta without partial_json"))?;
            if held.input.len() + part.len() > self.limits.max_tool_bytes {
                return Err(RestoreError::ToolTooLarge);
            }
            held.input.push_str(part);
            held.first_delta.get_or_insert(ev);
            return Ok(Vec::new());
        }
        if delta_type != Some("text_delta") {
            return Ok(vec![ev]);
        }
        let block = index.and_then(|i| self.texts.get_mut(&i));
        let block =
            block.ok_or_else(|| bad("text_delta for a block that is not an open text block"))?;
        let text = v["delta"].get("text").and_then(Value::as_str);
        let text = text.ok_or_else(|| bad("text_delta without text"))?;
        let (out, n) = block.delta(&self.pairs, self.probe.as_deref(), text)?;
        self.restored += n;
        let emitted = if out.is_empty() {
            Vec::new()
        } else if out == text {
            // Nothing changed: the upstream bytes go out untouched.
            vec![ev.clone()]
        } else {
            vec![with_delta_field(&ev, "text", &out)?]
        };
        block.last_delta = Some(ev);
        Ok(emitted)
    }

    fn stop(&mut self, ev: SseEvent, v: &Value) -> Result<Vec<SseEvent>, RestoreError> {
        let index = index_of(v);
        if self.held.as_ref().is_some_and(|h| index != Some(h.index)) {
            return Err(bad("another block stopped while a tool call was open"));
        }
        if let Some(held) = self.held.take() {
            let mut out = vec![held.start];
            if let Some(first) = held.first_delta.filter(|_| !held.input.is_empty()) {
                let input = self.restore_tool_input(&held.input)?;
                out.push(with_delta_field(&first, "partial_json", &input)?);
            }
            out.push(ev);
            return Ok(out);
        }
        let Some(index) = index else {
            if !self.texts.is_empty() {
                return Err(bad("a block stopped without an index while text was open"));
            }
            return Ok(vec![ev]);
        };
        let mut out = Vec::new();
        if let Some(mut block) = self.texts.remove(&index)
            && let Some((flushed, n)) = block.flush(&self.pairs, self.probe.as_deref())?
        {
            self.restored += n;
            self.dangling += 1;
            out.push(flushed);
        }
        out.push(ev);
        Ok(out)
    }

    /// Flushes every text carry, in index order.
    fn flush_all(&mut self) -> Result<Vec<SseEvent>, RestoreError> {
        let mut out = Vec::new();
        for block in self.texts.values_mut() {
            if let Some((flushed, n)) = block.flush(&self.pairs, self.probe.as_deref())? {
                self.restored += n;
                self.dangling += 1;
                out.push(flushed);
            }
        }
        Ok(out)
    }

    /// Restores every complete key and value literal of the tool input.
    /// Literals after a scan error stay as they are; the guard hook blocks
    /// any stand-in that reaches a tool call.
    fn restore_tool_input(&mut self, input: &str) -> Result<String, RestoreError> {
        let bytes = input.as_bytes();
        let mut edits = Vec::new();
        for lit in scan_tolerant(bytes) {
            let raw = &bytes[lit.span.clone()];
            let decoded = decode(raw).map_err(|_| bad("tool call input has an invalid string"))?;
            let found = self.pairs.find_standins(&decoded.text);
            if found.is_empty() {
                continue;
            }
            self.restored += found.len();
            edits.push((lit.span, replace(raw, &decoded, &found)));
        }
        String::from_utf8(splice(bytes, edits))
            .map_err(|_| bad("restored tool call input is not UTF-8"))
    }
}

/// Restores `content[i].text` of text blocks, and every key and value under
/// `content[i].input` of `tool_use` blocks. Nothing else is touched.
fn restorable(root: &Value, lit: &Lit) -> bool {
    let [Seg::Key(content), Seg::Index(i), Seg::Key(field), rest @ ..] = lit.path.as_slice() else {
        return false;
    };
    if content != "content" {
        return false;
    }
    match (
        field.as_str(),
        root.get("content")
            .and_then(|c| c.get(*i))
            .and_then(type_of),
    ) {
        ("text", Some("text")) => rest.is_empty() && !lit.is_key,
        // Everything under `input`, but not the key `input` itself.
        ("input", Some("tool_use")) => !(rest.is_empty() && lit.is_key),
        _ => false,
    }
}

/// Restores a non-streaming reply; returns the body and the number of
/// stand-ins replaced.
pub fn restore_message(body: &[u8], pairs: &Pairs) -> Result<(Vec<u8>, usize), ScanError> {
    let lits = scan(body)?;
    let root: Value = serde_json::from_slice(body).map_err(|_| ScanError {
        offset: 0,
        reason: "rejected by serde_json",
    })?;
    let mut edits = Vec::new();
    let mut count = 0;
    for lit in lits.iter().filter(|l| restorable(&root, l)) {
        let raw = &body[lit.span.clone()];
        let decoded = decode(raw).map_err(|_| ScanError {
            offset: lit.span.start,
            reason: "invalid string",
        })?;
        let found = pairs.find_standins(&decoded.text);
        if found.is_empty() {
            continue;
        }
        count += found.len();
        edits.push((lit.span.clone(), replace(raw, &decoded, &found)));
    }
    Ok((splice(body, edits), count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairs::{Pair, test_pairs};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    fn pairs() -> Arc<Pairs> {
        Arc::new(test_pairs())
    }
    fn ev(name: &str, data: Value) -> SseEvent {
        SseEvent::build(name, &data.to_string())
    }
    fn text_start(index: u64) -> SseEvent {
        ev(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        )
    }
    fn text_delta(index: u64, text: &str) -> SseEvent {
        ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        )
    }
    fn tool_start(index: u64) -> SseEvent {
        ev(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
        )
    }
    fn tool_delta(index: u64, partial: &str) -> SseEvent {
        ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":partial}}),
        )
    }
    fn stop(index: u64) -> SseEvent {
        ev(
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        )
    }
    fn message_stop() -> SseEvent {
        ev("message_stop", json!({"type":"message_stop"}))
    }
    fn text_stream(parts: &[&str]) -> Vec<SseEvent> {
        let mut v = vec![text_start(0)];
        v.extend(parts.iter().map(|p| text_delta(0, p)));
        v.push(stop(0));
        v.push(message_stop());
        v
    }
    fn try_run(r: &mut Restorer, evs: Vec<SseEvent>) -> Result<Vec<SseEvent>, RestoreError> {
        let mut out = Vec::new();
        for e in evs {
            out.extend(r.push(e)?);
        }
        out.extend(r.finish());
        Ok(out)
    }
    fn run(r: &mut Restorer, evs: Vec<SseEvent>) -> Vec<SseEvent> {
        try_run(r, evs).unwrap()
    }
    fn data(e: &SseEvent) -> Value {
        serde_json::from_str(&e.data).unwrap()
    }
    fn text_of(evs: &[SseEvent]) -> String {
        evs.iter()
            .filter_map(|e| serde_json::from_str::<Value>(&e.data).ok())
            .filter(|d| d["delta"]["type"] == "text_delta")
            .map(|d| d["delta"]["text"].as_str().unwrap().to_string())
            .collect()
    }
    fn restorer() -> Restorer {
        Restorer::new(pairs(), Limits::default(), None)
    }

    #[test]
    fn restores_stand_in_split_across_deltas() {
        let out = run(
            &mut Restorer::new(pairs(), Limits::default(), None),
            text_stream(&["the cli", "ent_x0", "1 file"]),
        );
        assert_eq!(text_of(&out), "the acmewidgets file");
    }
    #[test]
    fn dangling_prefix_flushed_before_stop() {
        let mut r = Restorer::new(pairs(), Limits::default(), None);
        let out = run(&mut r, text_stream(&["the proj", "ect plan ClientX0"]));
        assert_eq!(text_of(&out), "the project plan ClientX0");
        let stop = out
            .iter()
            .position(|e| e.event.as_deref() == Some("content_block_stop"))
            .unwrap();
        assert!(out[..stop].iter().any(|e| e.data.contains("ClientX0")));
        assert_eq!(r.dangling(), 1);
    }
    #[test]
    fn tool_use_buffered_and_restored_once() {
        let evs = vec![
            ev(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
            ),
            ev(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\": \"/Users/dev/code/cli"}}),
            ),
            ev(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"ent_x01/a.md\", \"n\": 1e3}"}}),
            ),
            ev(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
        ];
        let out = run(&mut Restorer::new(pairs(), Limits::default(), None), evs);
        assert_eq!(out.len(), 3);
        let d: serde_json::Value = serde_json::from_str(&out[1].data).unwrap();
        assert_eq!(
            d["delta"]["partial_json"],
            "{\"file_path\": \"/Users/dev/code/acmewidgets/a.md\", \"n\": 1e3}"
        );
    }

    proptest! {
        #[test]
        fn any_split_equals_full_restore(
            toks in prop::collection::vec(
                prop::sample::select(
                    [
                        "client_x01", "ClientX01", "person_x01", "client_x0", "ClientX", "clie", "x0", "1", " ",
                        "é", "😀", "a",
                    ]
                    .to_vec(),
                ),
                0..=30,
            ),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..=6),
        ) {
            let full: String = toks.concat();
            let bounds: Vec<usize> = full.char_indices().map(|(i, _)| i).chain([full.len()]).collect();
            let mut at: Vec<usize> = cuts.iter().map(|c| bounds[c.index(bounds.len())]).collect();
            at.sort_unstable();
            at.dedup();
            let mut parts = Vec::new();
            let mut prev = 0;
            for a in at {
                parts.push(&full[prev..a]);
                prev = a;
            }
            parts.push(&full[prev..]);
            let out = run(&mut restorer(), text_stream(&parts));
            prop_assert_eq!(text_of(&out), test_pairs().restore_str(&full).0);
            let texts: Vec<Value> = out.iter().map(data).filter(|d| d["delta"]["type"] == "text_delta").collect();
            prop_assert!(texts.iter().all(|d| d["delta"]["text"] != ""), "empty text_delta in {:?}", texts);
        }
    }

    #[test]
    fn tool_input_real_value_is_escaped() {
        let p = Pairs::new(vec![Pair {
            real: "L'Oréal \"SA\"".into(),
            standin: "company_x01".into(),
        }])
        .unwrap();
        let mut r = Restorer::new(Arc::new(p), Limits::default(), None);
        let out = run(
            &mut r,
            vec![
                tool_start(0),
                tool_delta(0, "{\"q\":\"company_x01\"}"),
                stop(0),
            ],
        );
        let input: Value =
            serde_json::from_str(data(&out[1])["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(input["q"], "L'Oréal \"SA\"");
    }
    #[test]
    fn ping_during_hold_passes_through_immediately() {
        let mut r = restorer();
        assert!(r.push(tool_start(1)).unwrap().is_empty());
        let ping = ev("ping", json!({"type":"ping"}));
        assert_eq!(r.push(ping.clone()).unwrap(), vec![ping]);
        assert!(r.is_holding());
    }
    #[test]
    fn thinking_is_never_restored() {
        let start = ev(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        );
        let think = ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"see client_x01"}}),
        );
        let sig = ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"client_x01"}}),
        );
        let evs = vec![start, think, sig, stop(0)];
        let out = run(&mut restorer(), evs.clone());
        assert_eq!(
            out.iter().map(|e| &e.raw).collect::<Vec<_>>(),
            evs.iter().map(|e| &e.raw).collect::<Vec<_>>()
        );
    }
    #[test]
    fn overlapping_blocks_fail_closed() {
        let mut r = restorer();
        r.push(tool_start(1)).unwrap();
        assert!(matches!(
            r.push(text_start(2)),
            Err(RestoreError::BadEvent(_))
        ));
        let mut r = restorer();
        r.push(tool_start(1)).unwrap();
        assert!(matches!(
            r.push(tool_start(2)),
            Err(RestoreError::BadEvent(_))
        ));
    }
    #[test]
    fn tool_too_large_fails() {
        let mut r = Restorer::new(pairs(), Limits { max_tool_bytes: 10 }, None);
        let res = try_run(
            &mut r,
            vec![
                tool_start(0),
                tool_delta(0, "{\"a\":"),
                tool_delta(0, "\"0123456\"}"),
                stop(0),
            ],
        );
        assert!(matches!(res, Err(RestoreError::ToolTooLarge)));
    }
    #[test]
    fn finish_drops_held_tool_block() {
        let mut r = restorer();
        let mut out = r.push(tool_start(0)).unwrap();
        out.extend(r.push(tool_delta(0, "{\"a\":\"client_x01\"}")).unwrap());
        out.extend(r.finish());
        assert!(!out.iter().any(|e| e.data.contains("tool_use")));
        assert!(!r.is_holding());
    }
    #[test]
    fn probe_split_across_deltas_fails() {
        let mut r = Restorer::new(
            pairs(),
            Limits::default(),
            Some("TACITA_STREAM_PROBE".into()),
        );
        let res = try_run(&mut r, text_stream(&["TACITA_STR", "EAM_PRO", "BE"]));
        assert!(matches!(res, Err(RestoreError::Probe)));
    }
    #[test]
    fn probe_tail_never_cuts_inside_a_char() {
        // The 5-byte tail window of "😀é" starts inside the emoji.
        let mut r = Restorer::new(pairs(), Limits::default(), Some("zz😀".into()));
        let res = try_run(&mut r, text_stream(&["😀é", "zz", "😀"]));
        assert!(matches!(res, Err(RestoreError::Probe)));
        let mut r = Restorer::new(pairs(), Limits::default(), Some("zz😀".into()));
        let out = run(&mut r, text_stream(&["😀é", "z", "z", "é"]));
        assert_eq!(text_of(&out), "😀ézzé");
    }
    #[test]
    fn restore_message_non_streaming() {
        let body = br#"{"content":[{"type":"text","text":"client_x01"},{"type":"thinking","thinking":"client_x01"},{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/x/client_x01"}}]}"#;
        let (out, n) = restore_message(body, &test_pairs()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["content"][0]["text"], "acmewidgets");
        assert_eq!(v["content"][1]["thinking"], "client_x01");
        assert_eq!(v["content"][2]["input"]["file_path"], "/x/acmewidgets");
        assert_eq!(n, 2);
    }
    #[test]
    fn restore_message_touches_only_text_and_tool_input() {
        let body = br#"{"id":"client_x01","content":[{"type":"tool_use","id":"client_x01","name":"client_x01","input":{"client_x01":["ClientX01", 1e3]}},{"type":"text","text":"x","citations":[{"cited_text":"client_x01"}]},{"type":"server_tool_use","input":{"q":"client_x01"}}]}"#;
        let (out, n) = restore_message(body, &test_pairs()).unwrap();
        let want = br#"{"id":"client_x01","content":[{"type":"tool_use","id":"client_x01","name":"client_x01","input":{"acmewidgets":["AcmeWidgets", 1e3]}},{"type":"text","text":"x","citations":[{"cited_text":"client_x01"}]},{"type":"server_tool_use","input":{"q":"client_x01"}}]}"#;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            String::from_utf8(want.to_vec()).unwrap()
        );
        assert_eq!(n, 2);
        assert!(restore_message(b"{\"content\":[", &test_pairs()).is_err());
    }
    #[test]
    fn changed_event_keeps_every_other_byte() {
        let before = r#"{"type":"content_block_delta","index":0, "delta":{"text":"a client_x01 b","type":"text_delta"},"zz":"client_x01"}"#;
        let after = r#"{"type":"content_block_delta","index":0, "delta":{"text":"a acmewidgets b","type":"text_delta"},"zz":"client_x01"}"#;
        let mut r = restorer();
        r.push(text_start(0)).unwrap();
        let out = r
            .push(SseEvent::build("content_block_delta", before))
            .unwrap();
        assert_eq!(out, vec![SseEvent::build("content_block_delta", after)]);
    }
    #[test]
    fn carries_flushed_in_index_order_before_message_stop() {
        let mut r = restorer();
        let evs = vec![
            text_start(1),
            text_start(0),
            text_delta(1, "b ClientX0"),
            text_delta(0, "a client_x0"),
            message_stop(),
        ];
        let out = run(&mut r, evs);
        assert_eq!(out.len(), 7);
        assert_eq!(
            (data(&out[4])["index"].as_u64(), text_of(&out[4..5])),
            (Some(0), "client_x0".to_string())
        );
        assert_eq!(
            (data(&out[5])["index"].as_u64(), text_of(&out[5..6])),
            (Some(1), "ClientX0".to_string())
        );
        assert_eq!(out[6].event.as_deref(), Some("message_stop"));
        assert_eq!(r.dangling(), 2);
    }
    #[test]
    fn counters_cover_text_and_tool_input() {
        let mut r = restorer();
        let evs = vec![
            text_start(0),
            text_delta(0, "client_x01 Client"),
            text_delta(0, "X01"),
            stop(0),
            tool_start(1),
            tool_delta(1, "{\"person_x01\":\"client_x01\"}"),
            stop(1),
            message_stop(),
        ];
        run(&mut r, evs);
        assert_eq!((r.restored(), r.dangling()), (4, 0));
    }
    #[test]
    fn empty_tool_input_emits_start_and_stop_only() {
        let out = run(
            &mut restorer(),
            vec![tool_start(0), tool_delta(0, ""), stop(0)],
        );
        assert_eq!(out, vec![tool_start(0), stop(0)]);
    }
    #[test]
    fn tool_input_after_scan_error_is_left_as_is() {
        let out = run(
            &mut restorer(),
            vec![
                tool_start(0),
                tool_delta(0, "{\"a\":\"client_x01\", oops \"b\":\"client_x01\"}"),
                stop(0),
            ],
        );
        assert_eq!(
            data(&out[1])["delta"]["partial_json"],
            "{\"a\":\"acmewidgets\", oops \"b\":\"client_x01\"}"
        );
    }
    #[test]
    fn foreign_block_events_during_hold_fail_closed() {
        let thinking = ev(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"x"}}),
        );
        let wrong_kind = text_delta(1, "x");
        for e in [
            text_delta(0, "x"),
            thinking,
            stop(0),
            wrong_kind,
            message_stop(),
        ] {
            let mut r = restorer();
            r.push(text_start(0)).unwrap();
            r.push(tool_start(1)).unwrap();
            assert!(matches!(r.push(e), Err(RestoreError::BadEvent(_))));
            assert!(r.is_holding());
        }
    }
    #[test]
    fn text_delta_without_open_text_block_fails_closed() {
        assert!(matches!(
            restorer().push(text_delta(3, "x")),
            Err(RestoreError::BadEvent(_))
        ));
    }
    #[test]
    fn handled_event_with_non_object_data_fails_closed() {
        for name in [
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
            "error",
        ] {
            for data in ["not json", "[1]", "\"s\""] {
                let res = restorer().push(SseEvent::build(name, data));
                assert!(
                    matches!(res, Err(RestoreError::BadEvent(_))),
                    "{name} {data}"
                );
            }
        }
        let odd = SseEvent::build("ping", "not json");
        assert_eq!(restorer().push(odd.clone()).unwrap(), vec![odd]);
    }
    #[test]
    fn empty_data_events_forwarded_raw() {
        // Comment-only blocks, stray blank lines and unhandled names.
        let mut r = restorer();
        r.push(tool_start(1)).unwrap();
        for wire in [
            ": keepalive\n\n",
            "\n",
            "event: ping\n\n",
            "event: surprise\n: x\n\n",
        ] {
            let evs = crate::sse::SseParser::default()
                .push(wire.as_bytes())
                .unwrap();
            assert_eq!(
                (evs.len(), evs[0].data.as_str(), evs[0].raw.as_slice()),
                (1, "", wire.as_bytes())
            );
            assert_eq!(r.push(evs[0].clone()).unwrap(), evs);
        }
        assert!(r.is_holding());
    }
    #[test]
    fn handled_event_names_need_data() {
        for name in [
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
            "error",
        ] {
            let wire = format!("event: {name}\n\n");
            let evs = crate::sse::SseParser::default()
                .push(wire.as_bytes())
                .unwrap();
            assert_eq!(evs[0].event.as_deref(), Some(name));
            let mut r = restorer();
            r.push(text_start(0)).unwrap();
            assert!(
                matches!(r.push(evs[0].clone()), Err(RestoreError::BadEvent(_))),
                "{name}"
            );
        }
    }
    fn named(event: Option<&str>, data: &str) -> SseEvent {
        match event {
            Some(name) => SseEvent::build(name, data),
            None => SseEvent {
                event: None,
                data: data.into(),
                raw: format!("data: {data}\n\n").into_bytes(),
            },
        }
    }
    #[test]
    fn handled_type_under_another_name_fails_closed() {
        // Claude Code would skip or ignore these, losing the carry with them.
        let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1 b"}}).to_string();
        for name in [
            Some("ping"),
            None,
            Some("message"),
            Some("content_block_stop"),
        ] {
            let mut r = restorer();
            r.push(text_start(0)).unwrap();
            r.push(text_delta(0, "a client_x0")).unwrap();
            assert!(
                matches!(r.push(named(name, &delta)), Err(RestoreError::BadEvent(_))),
                "{name:?}"
            );
        }
    }
    #[test]
    fn handled_name_with_another_data_type_fails_closed() {
        for data in [
            json!({"type":"ping"}),
            json!({"index":0}),
            json!({"type":"message_stop"}),
        ] {
            let res = restorer().push(ev("message_delta", data));
            assert!(matches!(res, Err(RestoreError::BadEvent(_))));
        }
    }
    #[test]
    fn other_events_are_forwarded_untouched() {
        let mut r = restorer();
        r.push(text_start(0)).unwrap();
        r.push(text_delta(0, "a client_x0")).unwrap();
        let start = json!({"type":"message_start","message":{"content":[{"type":"text","text":"client_x01"}]}});
        for e in [
            named(Some("ping"), r#"{"type": "ping"}"#),
            named(None, r#"{"type":"ping"}"#),
            named(Some("message"), "client_x01"),
            named(None, r#"{"type":"surprise","text":"client_x01"}"#),
            named(Some("message_start"), &start.to_string()),
        ] {
            assert_eq!(r.push(e.clone()).unwrap(), vec![e]);
        }
    }
    #[test]
    fn block_start_at_an_open_text_index_fails_closed() {
        let thinking = ev(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        );
        for e in [text_start(0), tool_start(0), thinking] {
            let mut r = restorer();
            r.push(text_start(0)).unwrap();
            assert!(matches!(r.push(e), Err(RestoreError::BadEvent(_))));
        }
    }
    #[test]
    fn block_stop_without_index_fails_closed_while_text_is_open() {
        for d in [
            json!({"type":"content_block_stop"}),
            json!({"type":"content_block_stop","index":"0"}),
            json!({"type":"content_block_stop","index":-1}),
        ] {
            let mut r = restorer();
            r.push(text_start(0)).unwrap();
            assert!(matches!(
                r.push(ev("content_block_stop", d.clone())),
                Err(RestoreError::BadEvent(_))
            ));
            let alone = ev("content_block_stop", d);
            assert_eq!(restorer().push(alone.clone()).unwrap(), vec![alone]);
        }
    }
    #[test]
    fn probe_hit_in_finish_becomes_an_error_event() {
        let mut r = Restorer::new(pairs(), Limits::default(), Some("zz client".into()));
        let mut out = r.push(text_start(0)).unwrap();
        out.extend(r.push(text_delta(0, "zz client_x0")).unwrap());
        assert_eq!(text_of(&out), "zz ");
        let probe =
            crate::errors::error_event("stream probe: this reply was stopped on purpose (Step 0)");
        assert_eq!(r.finish(), vec![probe]);
    }
    #[test]
    fn unchanged_text_delta_is_forwarded_byte_identical() {
        let wire = "event: content_block_delta\r\ndata:{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\\/b\"}}\r\n\r\n";
        let evs = crate::sse::SseParser::default()
            .push(wire.as_bytes())
            .unwrap();
        assert_eq!((evs.len(), evs[0].raw.as_slice()), (1, wire.as_bytes()));
        let mut r = restorer();
        r.push(text_start(0)).unwrap();
        assert_eq!(r.push(evs[0].clone()).unwrap(), evs);
    }
}
