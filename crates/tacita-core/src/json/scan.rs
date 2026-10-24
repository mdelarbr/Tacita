//! Linear-time JSON scanner that reports string literals with their paths.

use super::string::decode;
use std::ops::Range;
use thiserror::Error;

/// Maximum nesting depth accepted by the scanner.
const MAX_DEPTH: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// One string literal (quotes included) and where it sits in the document.
#[derive(Clone, Debug)]
pub struct Lit {
    pub span: Range<usize>,
    pub path: Vec<Seg>,
    pub is_key: bool,
}

#[derive(Debug, Error)]
#[error("invalid JSON at byte {offset}: {reason}")]
pub struct ScanError {
    pub offset: usize,
    pub reason: &'static str,
}

struct Scanner<'a> {
    b: &'a [u8],
    pos: usize,
    path: Vec<Seg>,
    out: Vec<Lit>,
}

type Res<T> = Result<T, ScanError>;

impl<'a> Scanner<'a> {
    fn err<T>(&self, reason: &'static str) -> Res<T> {
        Err(ScanError {
            offset: self.pos,
            reason,
        })
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8, reason: &'static str) -> Res<()> {
        if self.b.get(self.pos) == Some(&c) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(reason)
        }
    }

    /// Scans one string literal, validating escapes like `string::decode`.
    fn string(&mut self) -> Res<Range<usize>> {
        let start = self.pos;
        let mut i = start + 1;
        loop {
            let Some(&c) = self.b.get(i) else {
                self.pos = self.b.len();
                return self.err("unterminated string");
            };
            match c {
                b'"' => break,
                0..=0x1f => {
                    self.pos = i;
                    return self.err("control character in string");
                }
                b'\\' => match self.b.get(i + 1) {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                    Some(b'u') => {
                        let ok = self
                            .b
                            .get(i + 2..i + 6)
                            .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
                        if !ok {
                            self.pos = i;
                            return self.err("bad \\u escape");
                        }
                        i += 6;
                    }
                    Some(_) => {
                        self.pos = i;
                        return self.err("bad escape");
                    }
                    None => {
                        self.pos = self.b.len();
                        return self.err("unterminated string");
                    }
                },
                _ => i += 1,
            }
        }
        let span = start..i + 1;
        if std::str::from_utf8(&self.b[span.clone()]).is_err() {
            self.pos = start;
            return self.err("invalid UTF-8");
        }
        self.pos = span.end;
        Ok(span)
    }

    fn digits(&mut self) -> Res<()> {
        let s = self.pos;
        while self.b.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.pos == s {
            self.err("expected digit")
        } else {
            Ok(())
        }
    }

    fn number(&mut self) -> Res<()> {
        if self.b.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.b.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits()?,
            _ => return self.err("expected digit"),
        }
        if self.b.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if matches!(self.b.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.b.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.digits()?;
        }
        Ok(())
    }

    fn word(&mut self, w: &'static [u8]) -> Res<()> {
        if self.b[self.pos..].starts_with(w) {
            self.pos += w.len();
            Ok(())
        } else {
            self.err("invalid literal")
        }
    }

    fn value(&mut self, depth: usize) -> Res<()> {
        self.ws();
        match self.b.get(self.pos) {
            None => self.err("unexpected end of input"),
            Some(b'"') => {
                let span = self.string()?;
                self.out.push(Lit {
                    span,
                    path: self.path.clone(),
                    is_key: false,
                });
                Ok(())
            }
            Some(b'{') => {
                if depth >= MAX_DEPTH {
                    return self.err("too deep");
                }
                self.pos += 1;
                self.ws();
                if self.b.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if self.b.get(self.pos) != Some(&b'"') {
                        return self.err("expected object key");
                    }
                    let span = self.string()?;
                    let Ok(key) = decode(&self.b[span.clone()]) else {
                        return self.err("invalid key");
                    };
                    self.path.push(Seg::Key(key.text));
                    self.out.push(Lit {
                        span,
                        path: self.path.clone(),
                        is_key: true,
                    });
                    self.ws();
                    self.eat(b':', "expected ':'")?;
                    self.value(depth + 1)?;
                    self.path.pop();
                    self.ws();
                    match self.b.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                if depth >= MAX_DEPTH {
                    return self.err("too deep");
                }
                self.pos += 1;
                self.ws();
                if self.b.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(());
                }
                let mut idx = 0;
                loop {
                    self.path.push(Seg::Index(idx));
                    self.value(depth + 1)?;
                    self.path.pop();
                    idx += 1;
                    self.ws();
                    match self.b.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b't') => self.word(b"true"),
            Some(b'f') => self.word(b"false"),
            Some(b'n') => self.word(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.err("unexpected character"),
        }
    }
}

fn run(json: &[u8]) -> (Vec<Lit>, Res<()>) {
    let mut s = Scanner {
        b: json,
        pos: 0,
        path: Vec::new(),
        out: Vec::new(),
    };
    let r = s.value(0).and_then(|()| {
        s.ws();
        if s.pos == json.len() {
            Ok(())
        } else {
            s.err("trailing characters")
        }
    });
    (s.out, r)
}

/// Strict scan: the input must be exactly one JSON value plus whitespace.
pub fn scan(json: &[u8]) -> Result<Vec<Lit>, ScanError> {
    let (out, r) = run(json);
    r.map(|()| out)
}

/// Returns every complete literal before the first error or the end of input.
pub fn scan_tolerant(json: &[u8]) -> Vec<Lit> {
    run(json).0
}

/// Applies sorted, non-overlapping edits to `src`.
pub fn splice(src: &[u8], edits: Vec<(Range<usize>, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut at = 0;
    for (r, rep) in edits {
        out.extend_from_slice(&src[at..r.start]);
        out.extend_from_slice(&rep);
        at = r.end;
    }
    out.extend_from_slice(&src[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn k(s: &str) -> Seg {
        Seg::Key(s.into())
    }

    #[test]
    fn paths_for_values_and_keys() {
        let j = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let lits = scan(j).unwrap();
        let text = lits
            .iter()
            .find(|l| !l.is_key && &j[l.span.clone()] == br#""hi""#)
            .unwrap();
        assert_eq!(
            text.path,
            vec![
                k("messages"),
                Seg::Index(0),
                k("content"),
                Seg::Index(0),
                k("text")
            ]
        );
        let role = lits
            .iter()
            .find(|l| l.is_key && &j[l.span.clone()] == br#""role""#)
            .unwrap();
        assert_eq!(role.path, vec![k("messages"), Seg::Index(0), k("role")]);
    }

    #[test]
    fn strict_rejects_trailing_garbage_and_truncation() {
        assert!(scan(br#"{"a":1} x"#).is_err());
        assert!(scan(br#"{"a":"b"#).is_err());
    }

    #[test]
    fn tolerant_keeps_complete_literals_of_truncated_input() {
        let lits = scan_tolerant(br#"{"file_path":"/x/client_x01/a.md","content":"unfinish"#);
        assert_eq!(lits.len(), 3);
    }

    #[test]
    fn splice_leaves_other_bytes_identical() {
        let src = br#"{ "n": 1.0, "big": 12345678901234567890, "e": 1e3, "s": "acme" }"#;
        let s = scan(src)
            .unwrap()
            .into_iter()
            .find(|l| &src[l.span.clone()] == br#""acme""#)
            .unwrap();
        let out = splice(src, vec![(s.span, br#""client_x01""#.to_vec())]);
        assert_eq!(
            out,
            br#"{ "n": 1.0, "big": 12345678901234567890, "e": 1e3, "s": "client_x01" }"#.to_vec()
        );
    }

    #[test]
    fn depth_is_guarded() {
        let deep = |n: usize| [vec![b'['; n], vec![b']'; n]].concat();
        assert!(scan(&deep(512)).is_ok());
        let e = scan(&deep(513)).unwrap_err();
        assert_eq!(e.reason, "too deep");
        assert!(scan_tolerant(&vec![b'['; 100_000]).is_empty());
    }

    #[test]
    fn strict_rejects_bad_numbers_and_escapes() {
        for bad in [
            &b"01"[..],
            b"1.",
            b"-",
            b"1e",
            br#""\x""#,
            br#""\u12""#,
            b"\"a\nb\"",
            b"[1,]",
            b"{\"a\" 1}",
        ] {
            assert!(scan(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            ".*".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::btree_map(".*", inner, 0..8)
                    .prop_map(|m: BTreeMap<String, Value>| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn follow<'a>(v: &'a Value, path: &[Seg]) -> &'a Value {
        path.iter().fold(v, |v, s| match s {
            Seg::Key(k) => &v[k.as_str()],
            Seg::Index(i) => &v[*i],
        })
    }

    fn count_strings(v: &Value) -> usize {
        match v {
            Value::String(_) => 1,
            Value::Array(a) => a.iter().map(count_strings).sum(),
            Value::Object(o) => o.values().map(count_strings).sum(),
            _ => 0,
        }
    }

    proptest! {
        #[test]
        fn scan_agrees_with_serde(v in arb_value()) {
            let bytes = serde_json::to_vec(&v).unwrap();
            let lits = scan(&bytes).unwrap();
            let values: Vec<_> = lits.iter().filter(|l| !l.is_key).collect();
            for l in &values {
                let d = decode(&bytes[l.span.clone()]).unwrap();
                prop_assert_eq!(follow(&v, &l.path).as_str(), Some(d.text.as_str()));
            }
            prop_assert_eq!(values.len(), count_strings(&v));
        }

        #[test]
        fn tolerant_never_panics_on_prefixes(v in arb_value(), cut in 0.0f64..=1.0) {
            let bytes = serde_json::to_vec(&v).unwrap();
            let n = (bytes.len() as f64 * cut) as usize;
            let _ = scan_tolerant(&bytes[..n]);
        }

        #[test]
        fn tolerant_never_panics_on_arbitrary_bytes(b in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = scan_tolerant(&b);
        }
    }
}
