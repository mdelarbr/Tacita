//! Incremental `text/event-stream` parser (WHATWG rules) that keeps the exact
//! bytes of every event so they can be forwarded unchanged.

#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub raw: Vec<u8>,
}

impl SseEvent {
    /// One `data:` line per `\n`-separated line of `data`.
    pub fn build(event: &str, data: &str) -> SseEvent {
        let mut raw = format!("event: {event}\n");
        for line in data.split('\n') {
            raw.push_str("data: ");
            raw.push_str(line);
            raw.push('\n');
        }
        raw.push('\n');
        SseEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
            raw: raw.into_bytes(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("invalid UTF-8 in SSE event")]
    Utf8,
}

#[derive(Default)]
pub struct SseParser {
    /// Bytes from the first byte of the current event onward.
    buf: Vec<u8>,
    /// Start of the current (incomplete) line within `buf`.
    line_start: usize,
    /// Next byte to examine for a line terminator; never moves backwards
    /// within a line, so each byte is scanned once.
    scan: usize,
    event: Option<Vec<u8>>,
    data: Vec<u8>,
    has_data: bool,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut consumed = 0; // bytes of buf belonging to finished events
        let mut error = None;
        'lines: loop {
            let mut i = self.scan;
            let term_end = loop {
                match self.buf.get(i) {
                    None => {
                        self.scan = i;
                        break 'lines;
                    }
                    Some(b'\n') => break i + 1,
                    Some(b'\r') => match self.buf.get(i + 1) {
                        None => {
                            self.scan = i; // wait for a possible LF
                            break 'lines;
                        }
                        Some(b'\n') => break i + 2,
                        Some(_) => break i + 1,
                    },
                    Some(_) => i += 1,
                }
            };
            let line_end = i;
            if line_end == self.line_start {
                let raw = self.buf[consumed..term_end].to_vec();
                consumed = term_end;
                match self.finish_event(raw) {
                    Ok(ev) => out.push(ev),
                    Err(e) => error = Some(e),
                }
            } else {
                self.field(self.line_start, line_end);
            }
            self.line_start = term_end;
            self.scan = term_end;
        }
        self.buf.drain(..consumed);
        self.line_start -= consumed;
        self.scan -= consumed;
        match error {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// Returns the bytes of an unfinished event and resets the parser.
    pub fn finish(&mut self) -> Vec<u8> {
        self.line_start = 0;
        self.scan = 0;
        self.event = None;
        self.data.clear();
        self.has_data = false;
        std::mem::take(&mut self.buf)
    }

    fn field(&mut self, start: usize, end: usize) {
        let line = &self.buf[start..end];
        let (name, mut value) = match line.iter().position(|&b| b == b':') {
            Some(0) => return, // comment
            Some(p) => (&line[..p], &line[p + 1..]),
            None => (line, &line[line.len()..]),
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match name {
            b"event" => self.event = Some(value.to_vec()),
            b"data" => {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
            _ => {}
        }
    }

    fn finish_event(&mut self, raw: Vec<u8>) -> Result<SseEvent, SseError> {
        let event = self.event.take();
        let data = std::mem::take(&mut self.data);
        self.has_data = false;
        let event = match event {
            Some(e) => Some(String::from_utf8(e).map_err(|_| SseError::Utf8)?),
            None => None,
        };
        let data = String::from_utf8(data).map_err(|_| SseError::Utf8)?;
        Ok(SseEvent { event, data, raw })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_anthropic_events() {
        let mut p = SseParser::default();
        let evs = p.push(b"event: ping\ndata: {\"type\": \"ping\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("ping"));
        assert_eq!(evs[0].data, "{\"type\": \"ping\"}");
        assert_eq!(evs[0].raw, b"event: ping\ndata: {\"type\": \"ping\"}\n\n".to_vec());
    }

    #[test]
    fn handles_crlf_cr_and_multiline_data() {
        let mut p = SseParser::default();
        let evs = p.push(b"event: x\r\ndata: a\r\ndata: b\r\n\r\ndata: c\r\rdata: d").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "a\nb");
        assert_eq!(evs[1].data, "c");
    }

    #[test]
    fn cr_at_chunk_end_is_not_a_blank_line() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: a\r").unwrap().is_empty());
        let evs = p.push(b"\ndata: b\n\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "a\nb");
    }

    #[test]
    fn rejects_invalid_utf8() {
        assert!(SseParser::default().push(b"data: \xff\n\n").is_err());
        assert!(SseParser::default().push(b"event: \xff\ndata: a\n\n").is_err());
    }

    #[test]
    fn finish_returns_unterminated_bytes() {
        let mut p = SseParser::default();
        p.push(b"data: partial").unwrap();
        assert_eq!(p.finish(), b"data: partial".to_vec());
    }

    #[test]
    fn build_writes_one_data_line_per_line() {
        assert_eq!(SseEvent::build("x", "d").raw, b"event: x\ndata: d\n\n".to_vec());
        let ev = SseEvent::build("x", "a\nb");
        assert_eq!(ev.raw, b"event: x\ndata: a\ndata: b\n\n".to_vec());
        let evs = SseParser::default().push(&ev.raw).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "a\nb");
    }

    #[test]
    fn blank_line_alone_is_an_event() {
        let evs = SseParser::default().push(b"\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event, None);
        assert_eq!(evs[0].data, "");
        assert_eq!(evs[0].raw, b"\n".to_vec());
    }

    #[test]
    fn field_rules() {
        let evs = SseParser::default()
            .push(b"event:x\ndata\ndata:  b\n: note\nid: 7\n\n")
            .unwrap();
        assert_eq!(evs[0].event.as_deref(), Some("x"));
        assert_eq!(evs[0].data, "\n b");
    }

    #[test]
    fn comment_only_block_is_returned() {
        let evs = SseParser::default().push(b": hi\n\n").unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].raw, b": hi\n\n".to_vec());
    }

    #[test]
    fn multibyte_split_across_chunks_is_fine() {
        let s = "data: é😀\n\n";
        let bytes = s.as_bytes();
        let mut p = SseParser::default();
        let mut evs = Vec::new();
        for b in bytes.chunks(1) {
            evs.extend(p.push(b).unwrap());
        }
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "é😀");
    }

    #[test]
    fn one_mib_line_in_small_chunks_is_linear() {
        let big = "x".repeat(1 << 20);
        let stream = format!("event: big\ndata: {big}\n\n");
        let mut p = SseParser::default();
        let mut evs = Vec::new();
        for c in stream.as_bytes().chunks(16) {
            evs.extend(p.push(c).unwrap());
        }
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, big);
    }

    fn stream() -> Vec<u8> {
        let s = "event: a\r\ndata: x\r\n\r\n\
                 \n\
                 : just a comment\n\n\
                 event: b\ndata: é😀 {\"k\": 1}\r\n\r\n\
                 event: c\r\ndata: l1\r\ndata: l2\r\n\r\n\
                 data: tail";
        s.as_bytes().to_vec()
    }

    proptest! {
        #[test]
        fn any_chunking_gives_same_events(mut cuts in proptest::collection::vec(0usize..200, 0..=6)) {
            let input = stream();
            let mut whole = SseParser::default();
            let expected = whole.push(&input).unwrap();
            let expected_tail = whole.finish();

            cuts.iter_mut().for_each(|c| *c %= input.len() + 1);
            cuts.sort();
            let mut p = SseParser::default();
            let mut got = Vec::new();
            let mut prev = 0;
            for c in cuts.into_iter().chain(std::iter::once(input.len())) {
                got.extend(p.push(&input[prev..c]).unwrap());
                prev = c;
            }
            let tail = p.finish();
            prop_assert_eq!(&got, &expected);
            prop_assert_eq!(&tail, &expected_tail);
            let mut round: Vec<u8> = got.iter().flat_map(|e| e.raw.clone()).collect();
            round.extend(tail);
            prop_assert_eq!(round, input);
        }
    }
}
