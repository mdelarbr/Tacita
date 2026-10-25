//! Codec for one JSON string literal, with an offset map back to the raw bytes.

use std::borrow::Cow;
use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StrError {
    #[error("not a JSON string literal")]
    NotAString,
    #[error("unterminated string literal")]
    Unterminated,
    #[error("bad escape at byte {0}")]
    BadEscape(usize),
    #[error("raw control character at byte {0}")]
    RawControl(usize),
    #[error("invalid UTF-8")]
    BadUtf8,
}

/// A decoded string literal. `map[i]` is the raw offset where the source of
/// the char holding decoded byte `i` starts; the last entry is the closing
/// quote's offset.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    map: Vec<usize>,
}

fn hex4(b: &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let mut v = 0u32;
    for &c in &b[..4] {
        v = v * 16 + (c as char).to_digit(16)?;
    }
    Some(v)
}

/// Decodes one JSON string literal (quotes included).
pub fn decode(lit: &[u8]) -> Result<Decoded, StrError> {
    if lit.first() != Some(&b'"') {
        return Err(StrError::NotAString);
    }
    let src = std::str::from_utf8(lit).map_err(|_| StrError::BadUtf8)?;
    let mut text = String::new();
    let mut map = Vec::new();
    let mut push = |text: &mut String, c: char, start: usize| {
        text.push(c);
        for _ in 0..c.len_utf8() {
            map.push(start);
        }
    };
    let mut i = 1;
    loop {
        let Some(&b) = lit.get(i) else {
            return Err(StrError::Unterminated);
        };
        match b {
            b'"' => {
                if i + 1 != lit.len() {
                    return Err(StrError::NotAString);
                }
                map.push(i);
                return Ok(Decoded { text, map });
            }
            0..=0x1f => return Err(StrError::RawControl(i)),
            b'\\' => {
                let Some(&e) = lit.get(i + 1) else {
                    return Err(StrError::Unterminated);
                };
                let simple = match e {
                    b'"' => Some('"'),
                    b'\\' => Some('\\'),
                    b'/' => Some('/'),
                    b'b' => Some('\u{8}'),
                    b'f' => Some('\u{c}'),
                    b'n' => Some('\n'),
                    b'r' => Some('\r'),
                    b't' => Some('\t'),
                    _ => None,
                };
                if let Some(c) = simple {
                    push(&mut text, c, i);
                    i += 2;
                } else if e == b'u' {
                    let hi = hex4(&lit[i + 2..]).ok_or(StrError::BadEscape(i))?;
                    let mut len = 6;
                    let c = if (0xD800..0xDC00).contains(&hi)
                        && lit.get(i + 6) == Some(&b'\\')
                        && lit.get(i + 7) == Some(&b'u')
                    {
                        match hex4(&lit[(i + 8).min(lit.len())..]) {
                            Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                                len = 12;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            }
                            _ => None,
                        }
                    } else {
                        char::from_u32(hi)
                    };
                    push(&mut text, c.unwrap_or('\u{FFFD}'), i);
                    i += len;
                } else {
                    return Err(StrError::BadEscape(i));
                }
            }
            _ => {
                // Valid UTF-8 was checked above, so this slice starts on a char boundary.
                let c = src[i..].chars().next().ok_or(StrError::Unterminated)?;
                push(&mut text, c, i);
                i += c.len_utf8();
            }
        }
    }
}

/// Encodes `s` as a JSON string literal with quotes.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A copy of `json` in which every lone-surrogate escape (`\uD800` to
/// `\uDFFF`, not part of a high+low pair) is the six bytes `\uFFFD`, for
/// serde_json, which rejects lone surrogates. Both are six bytes, so every
/// offset still points at the same place in `json`. Borrowed when there is
/// none. A backslash outside a string is not valid JSON, so every one met
/// here starts an escape.
pub fn clean_lone_surrogates(json: &[u8]) -> Cow<'_, [u8]> {
    let hex_at = |i: usize| json.get(i..).and_then(hex4);
    let mut out: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < json.len() {
        if json[i] != b'\\' {
            i += 1;
            continue;
        }
        let unit = (json.get(i + 1) == Some(&b'u')).then(|| hex_at(i + 2)).flatten();
        let Some(unit) = unit.filter(|u| (0xD800..0xE000).contains(u)) else {
            // Any other escape, `\\` included: nothing after its first two
            // bytes can start an escape.
            i += 2;
            continue;
        };
        let paired = (0xD800..0xDC00).contains(&unit)
            && json.get(i + 6..i + 8) == Some(&b"\\u"[..])
            && hex_at(i + 8).is_some_and(|lo| (0xDC00..0xE000).contains(&lo));
        if paired {
            i += 12;
            continue;
        }
        out.get_or_insert_with(|| json.to_vec())[i..i + 6].copy_from_slice(b"\\uFFFD");
        i += 6;
    }
    out.map_or(Cow::Borrowed(json), Cow::Owned)
}

/// Replaces byte ranges of the decoded text, copying every other raw byte unchanged.
pub fn replace(lit: &[u8], decoded: &Decoded, edits: &[(Range<usize>, &str)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(lit.len());
    let mut cursor = 0;
    for (range, new) in edits {
        out.extend_from_slice(&lit[cursor..decoded.map[range.start]]);
        let enc = encode(new);
        out.extend_from_slice(&enc.as_bytes()[1..enc.len() - 1]);
        cursor = decoded.map[range.end];
    }
    out.extend_from_slice(&lit[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes() {
        let d = decode(br#""a\u00e9\"b\uD83D\uDE00c""#).unwrap();
        assert_eq!(d.text, "aé\"b😀c");
    }
    #[test]
    fn replace_keeps_untouched_escapes_byte_exact() {
        let lit = br#""x \u00e9 acmewidgets \/ y""#;
        let d = decode(lit).unwrap();
        let at = d.text.find("acmewidgets").unwrap();
        assert_eq!(replace(lit, &d, &[(at..at + 11, "client_x01")]), br#""x \u00e9 client_x01 \/ y""#.to_vec());
    }
    #[test]
    fn replace_escapes_real_values() {
        let lit = br#""see client_x01""#;
        let d = decode(lit).unwrap();
        assert_eq!(replace(lit, &d, &[(4..14, "L'\"Oréal\\\n")]), "\"see L'\\\"Oréal\\\\\\n\"".as_bytes());
    }
    #[test]
    fn match_spanning_an_escape_is_replaced_whole() {
        let lit = br#""Acme\u0057idgets""#;
        let d = decode(lit).unwrap();
        assert_eq!(d.text, "AcmeWidgets");
        assert_eq!(replace(lit, &d, &[(0..11, "ClientX01")]), br#""ClientX01""#.to_vec());
    }
    #[test]
    fn lone_surrogate_survives_byte_exact() {
        let lit = br#""\uD800 acmewidgets""#;
        let d = decode(lit).unwrap();
        assert!(d.text.starts_with('\u{FFFD}'));
        let at = d.text.find("acmewidgets").unwrap();
        assert_eq!(replace(lit, &d, &[(at..at + 11, "client_x01")]), br#""\uD800 client_x01""#.to_vec());
    }
    #[test]
    fn lone_surrogate_escapes_are_cleaned_in_place() {
        let cases: [(&[u8], &[u8]); 9] = [
            (br#""\uD800 x""#, br#""\uFFFD x""#),
            (br#""\udfff""#, br#""\uFFFD""#),
            (br#""a\uDC00b\uDBFF""#, br#""a\uFFFDb\uFFFD""#),
            // A high escape followed by something other than a low escape.
            (br#""\uD800\u0041""#, br#""\uFFFD\u0041""#),
            (br#""\uD800\n""#, br#""\uFFFD\n""#),
            // Two highs, then a low: the first is alone.
            (br#""\uD800\uD800\uDC00""#, br#""\uFFFD\uD800\uDC00""#),
            // A low before a high is not a pair.
            (br#""\uDC00\uD800""#, br#""\uFFFD\uFFFD""#),
            // An escaped backslash is not an escape of `u`.
            (br#"{"\\uD800":"\\\uD800"}"#, br#"{"\\uD800":"\\\uFFFD"}"#),
            (br#"["\uD83D\uDE00", "\ud83d\ude00", "\u00e9"]"#, br#"["\uD83D\uDE00", "\ud83d\ude00", "\u00e9"]"#),
        ];
        for (raw, want) in cases {
            let got = clean_lone_surrogates(raw);
            assert_eq!(
                (got.as_ref(), got.len()),
                (want, raw.len()),
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
        let clean = br#"{"a":"\uD83D\uDE00 \u00e9"}"#;
        assert!(matches!(clean_lone_surrogates(clean), Cow::Borrowed(_)));
        // The cleaned copy is what serde_json reads.
        let v: String = serde_json::from_slice(&clean_lone_surrogates(br#""\uD800x""#)).unwrap();
        assert_eq!(v, "\u{FFFD}x");
    }
    #[test]
    fn rejects_raw_control_and_bad_escape() {
        assert!(matches!(decode(b"\"a\nb\""), Err(StrError::RawControl(_))));
        assert!(matches!(decode(br#""\q""#), Err(StrError::BadEscape(_))));
    }
    #[test]
    fn decodes_one_mib_raw_literal() {
        let unit = "ab\u{e9}\u{4e2d}\u{1F600}";
        let body = unit.repeat((1 << 20) / unit.len());
        let lit = format!("\"{body}\"");
        let d = decode(lit.as_bytes()).unwrap();
        assert_eq!(d.text, body);
    }
    proptest::proptest! {
        #[test]
        fn encode_round_trips(s in "(?s).*") {
            let lit = encode(&s);
            proptest::prop_assert_eq!(serde_json::from_str::<String>(&lit).unwrap(), s.clone());
            proptest::prop_assert_eq!(decode(lit.as_bytes()).unwrap().text, s);
        }
    }
}
