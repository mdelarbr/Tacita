//! Request scrubbing policy: which string literals of a Claude Messages API
//! body may have business names replaced.

use crate::json::{self, Lit, ScanError, Seg};
use crate::pairs::Pairs;
use serde_json::Value;
use std::collections::HashSet;
use std::ops::Range;

#[derive(Debug)]
pub struct Scrubbed {
    pub body: Vec<u8>,
    pub replacements: usize,
    /// Replacements inside `system[2..]`.
    pub in_system: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error(transparent)]
    Json(#[from] ScanError),
    #[error("request body is not a JSON object")]
    NotAnObject,
    /// Some object repeats a key. The key is deliberately not named: keys
    /// inside user data can be business names.
    #[error("request body has an object with a repeated key")]
    DuplicateKey,
    /// Scannable but rejected by serde_json (lone surrogate, nesting too deep).
    /// The message is serde's own text and never contains body content.
    #[error("request body is not valid JSON: {0}")]
    Invalid(String),
}

const TOOL_USE_TYPES: [&str; 3] = ["tool_use", "server_tool_use", "mcp_tool_use"];
const PROTECTED_VALUE_KEYS: [&str; 9] = [
    "type",
    "role",
    "id",
    "tool_use_id",
    "signature",
    "media_type",
    "encrypted_content",
    "encrypted_index",
    "file_id",
];

fn is_key(seg: &Seg, name: &str) -> bool {
    matches!(seg, Seg::Key(k) if k == name)
}

fn type_of(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

/// Decides whether the literal may be rewritten, by walking `root` along its path.
/// Anything unexpected in shape falls through to "scrub" (values) or "keep" (keys).
fn should_scrub(root: &Value, lit: &Lit) -> bool {
    let path = &lit.path;
    let n = path.len();
    let mut cur = root;
    for (i, seg) in path.iter().enumerate() {
        let child = match (cur, seg) {
            (Value::Object(m), Seg::Key(k)) => m.get(k),
            (Value::Array(a), Seg::Index(j)) => a.get(*j),
            _ => None,
        };
        let Some(child) = child else { return !lit.is_key };
        let last = i + 1 == n;
        let is_own_key = last && lit.is_key;
        let key = match seg {
            Seg::Key(k) => Some(k.as_str()),
            Seg::Index(_) => None,
        };
        let parent_type = type_of(cur);

        if i >= 1 && matches!(parent_type, Some("thinking" | "redacted_thinking")) {
            return false;
        }
        let tool_use_parent = i >= 1 && parent_type.is_some_and(|t| TOOL_USE_TYPES.contains(&t));
        // User-data subtrees: everything inside is scrubbed, keys included.
        let user_data = (key == Some("input") && tool_use_parent)
            || (i == 2
                && matches!(key, Some("input_schema" | "input_examples"))
                && is_key(&path[0], "tools")
                && matches!(path[1], Seg::Index(_)))
            || (i == 1 && is_key(&path[0], "output_format") && key == Some("schema"));
        if user_data {
            return !is_own_key;
        }
        if is_own_key {
            return false;
        }
        match key {
            Some("cache_control") => return false,
            // Only base64 payloads (images, PDFs) are opaque; text sources are scrubbed.
            Some("data") if i >= 1 && is_key(&path[i - 1], "source") && type_of(cur) == Some("base64") => {
                return false;
            }
            Some("model" | "metadata") if i == 0 => return false,
            Some("system") if i == 0 && child.is_string() => return false,
            Some("name") if i == 1 && is_key(&path[0], "tool_choice") => return false,
            Some("name") if i == 2 && matches!(path[1], Seg::Index(_)) && is_key(&path[0], "tools") => {
                return false;
            }
            Some("name") if last && tool_use_parent => return false,
            Some(k) if last && PROTECTED_VALUE_KEYS.contains(&k) => return false,
            _ => {}
        }
        if i == 1 && matches!(seg, Seg::Index(0 | 1)) && is_key(&path[0], "system") {
            return false;
        }
        cur = child;
    }
    true
}

fn in_system_tail(lit: &Lit) -> bool {
    matches!(lit.path.as_slice(), [Seg::Key(k), Seg::Index(j), ..] if k == "system" && *j >= 2)
}

pub fn scrub_request(body: &[u8], pairs: &Pairs) -> Result<Scrubbed, RequestError> {
    let lits = json::scan(body)?;
    let mut seen: HashSet<&[Seg]> = HashSet::new();
    for lit in lits.iter().filter(|l| l.is_key) {
        if !seen.insert(&lit.path) {
            return Err(RequestError::DuplicateKey);
        }
    }
    let root: Value = serde_json::from_slice(body).map_err(|e| RequestError::Invalid(e.to_string()))?;
    if !root.is_object() {
        return Err(RequestError::NotAnObject);
    }
    let mut edits: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
    let (mut replacements, mut in_system) = (0, 0);
    for lit in &lits {
        if !should_scrub(&root, lit) {
            continue;
        }
        let raw = &body[lit.span.clone()];
        let decoded = json::decode(raw).map_err(|e| RequestError::Invalid(e.to_string()))?;
        let found = pairs.find_reals(&decoded.text);
        if found.is_empty() {
            continue;
        }
        replacements += found.len();
        if in_system_tail(lit) {
            in_system += found.len();
        }
        edits.push((lit.span.clone(), json::replace(raw, &decoded, &found)));
    }
    edits.sort_by_key(|(r, _)| r.start);
    Ok(Scrubbed { body: json::splice(body, edits), replacements, in_system })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairs::test_pairs;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/claude_code_request.json");
    fn out() -> (serde_json::Value, String, usize) {
        let s = scrub_request(FIXTURE, &test_pairs()).unwrap();
        assert_eq!(s.in_system, 2);
        (serde_json::from_slice(&s.body).unwrap(), String::from_utf8(s.body).unwrap(), s.replacements)
    }
    #[test]
    fn scrubs_user_content() {
        let (v, _, n) = out();
        assert_eq!(n, 12);
        assert_eq!(v.pointer("/system/2/text").unwrap(), "Working directory: /Users/dev/code/client_x01\nGit branch: client_x01-main");
        assert_eq!(v.pointer("/messages/0/content/0/text").unwrap(), "Summarize client_x01/README.md");
        assert!(v.pointer("/tools/0/input_schema/properties/client_x01_id").is_some());
        assert_eq!(v.pointer("/tools/0/input_schema/required/0").unwrap(), "client_x01_id");
        assert_eq!(v.pointer("/messages/1/content/1/input/client_x01_id").unwrap(), "client_x01-42");
        assert_eq!(v.pointer("/messages/1/content/1/input/type").unwrap(), "client_x01");
        assert_eq!(v.pointer("/messages/2/content/0/content/0/text").unwrap(), "Row: client_x01, owner person_x01");
    }
    #[test]
    fn never_touches_protected_parts() {
        let (v, text, _) = out();
        let orig: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        for p in ["/system/0", "/system/1", "/model", "/metadata", "/tools/0/name", "/messages/1/content/0",
                  "/messages/1/content/1/id", "/messages/1/content/1/name", "/messages/2/content/0/tool_use_id",
                  "/messages/0/content/1/source/data"] {
            assert_eq!(v.pointer(p), orig.pointer(p), "{p}");
        }
        assert_eq!(text.matches("acmewidgets").count(), 7);
    }
    #[test]
    fn deterministic_and_idempotent() {
        let a = scrub_request(FIXTURE, &test_pairs()).unwrap().body;
        assert_eq!(a, scrub_request(FIXTURE, &test_pairs()).unwrap().body);
        assert_eq!(scrub_request(&a, &test_pairs()).unwrap().body, a);
    }
    #[test]
    fn invalid_json_is_an_error() {
        assert!(scrub_request(b"{\"messages\": [", &test_pairs()).is_err());
    }
    #[test]
    fn non_object_and_serde_rejects() {
        assert!(matches!(scrub_request(b"[1]", &test_pairs()), Err(RequestError::NotAnObject)));
        let e = scrub_request(br#"{"a":"\uD800 acmewidgets"}"#, &test_pairs()).unwrap_err();
        assert!(matches!(e, RequestError::Invalid(ref m) if !m.contains("acmewidgets")));
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let body = format!("{{\"a\":{deep}}}");
        assert!(matches!(scrub_request(body.as_bytes(), &test_pairs()), Err(RequestError::Invalid(_))));
    }
    #[test]
    fn odd_shapes_scrub_without_panic() {
        let b = br#"{"system":{"x":"acmewidgets"},"tools":"acmewidgets","messages":[1,"acmewidgets",{"type":5,"name":"acmewidgets"}]}"#;
        let s = scrub_request(b, &test_pairs()).unwrap();
        assert_eq!(s.replacements, 4);
    }
    const PROBE: &str = r#"{
  "model": "acmewidgets",
  "metadata": {"user_id": "acmewidgets", "x": {"y": "acmewidgets"}},
  "system": [
    {"type": "text", "text": "acmewidgets billing"},
    {"type": "text", "text": "acmewidgets prompt", "cache_control": {"type": "ephemeral", "note": "acmewidgets"}},
    {"type": "text", "text": "acmewidgets cwd"}
  ],
  "tool_choice": {"type": "tool", "name": "acmewidgets"},
  "tools": [
    {"name": "acmewidgets", "description": "acmewidgets", "cache_control": {"type": "ephemeral", "note": "acmewidgets"},
     "input_schema": {"acmewidgets": "acmewidgets"},
     "input_examples": [{"acmewidgets": "acmewidgets"}]}
  ],
  "output_format": {"type": "json_schema", "schema": {"acmewidgets": "acmewidgets"}},
  "messages": [
    {"role": "user", "content": [
      {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral", "note": "acmewidgets"}},
      {"type": "image", "source": {"type": "base64", "media_type": "acmewidgets", "data": "acmewidgets"}},
      {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "acmewidgets"}},
      {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "acmewidgets"}},
      {"type": "document", "source": {"type": "file", "file_id": "acmewidgets"}}
    ]},
    {"role": "assistant", "content": [
      {"type": "thinking", "thinking": "acmewidgets", "signature": "acmewidgets"},
      {"type": "redacted_thinking", "data": "acmewidgets"},
      {"type": "tool_use", "id": "acmewidgets", "name": "acmewidgets", "input": {"acmewidgets": "acmewidgets"}},
      {"type": "server_tool_use", "id": "acmewidgets", "name": "acmewidgets", "input": {"query": "acmewidgets"}},
      {"type": "web_search_tool_result", "tool_use_id": "acmewidgets", "content": [
        {"type": "web_search_result", "url": "https://acmewidgets.example", "title": "acmewidgets",
         "encrypted_content": "acmewidgets", "page_age": "1d"}]},
      {"type": "text", "text": "ok", "citations": [{"type": "web_search_result_location", "cited_text": "acmewidgets",
         "encrypted_index": "acmewidgets", "url": "u", "title": "t"}]},
      {"unknown_field": "acmewidgets"}
    ]},
    {"role": "user", "content": [
      {"type": "tool_result", "tool_use_id": "acmewidgets", "content": "acmewidgets"}
    ]}
  ]
}"#;

    #[test]
    fn every_protected_location_is_kept_and_every_user_location_scrubbed() {
        let s = scrub_request(PROBE.as_bytes(), &test_pairs()).unwrap();
        let orig: serde_json::Value = serde_json::from_str(PROBE).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&s.body).unwrap();
        let protected = [
            "/model", "/metadata", "/system/0", "/system/1", "/tool_choice", "/tools/0/name",
            "/tools/0/cache_control", "/output_format/type", "/messages/0/content/0/cache_control",
            "/messages/0/content/1", "/messages/0/content/2", "/messages/0/content/4",
            "/messages/1/content/0", "/messages/1/content/1",
            "/messages/1/content/2/id", "/messages/1/content/2/name",
            "/messages/1/content/3/id", "/messages/1/content/3/name",
            "/messages/1/content/4/tool_use_id", "/messages/1/content/4/content/0/encrypted_content",
            "/messages/1/content/5/citations/0/encrypted_index",
            "/messages/2/content/0/tool_use_id",
        ];
        for p in protected {
            assert_eq!(v.pointer(p), orig.pointer(p), "{p}");
        }
        let scrubbed = [
            "/system/2/text", "/tools/0/description", "/messages/0/content/3/source/data",
            "/messages/1/content/3/input/query", "/messages/1/content/4/content/0/url",
            "/messages/1/content/4/content/0/title", "/messages/1/content/5/citations/0/cited_text",
            "/messages/1/content/6/unknown_field", "/messages/2/content/0/content",
            "/tools/0/input_schema/client_x01", "/tools/0/input_examples/0/client_x01",
            "/output_format/schema/client_x01", "/messages/1/content/2/input/client_x01",
        ];
        for p in scrubbed {
            assert!(!v.pointer(p).unwrap().to_string().contains("acmewidgets"), "{p}");
        }
        // 9 single hits, plus a key and a value in each of the four user-data subtrees.
        assert_eq!(s.replacements, 9 + 8);
        assert_eq!(s.in_system, 1);
    }
    #[test]
    fn string_system_is_kept_whole() {
        let b = br#"{"system":"acmewidgets","messages":[]}"#;
        let s = scrub_request(b, &test_pairs()).unwrap();
        assert_eq!((s.body.as_slice(), s.replacements), (&b[..], 0));
    }
    #[test]
    fn duplicate_keys_are_rejected_without_naming_them() {
        for b in [
            r#"{"type":"text","text":"acmewidgets","type":"thinking"}"#,
            r#"{"messages":[{"role":"user","role":"x"}]}"#,
            r#"{"messages":[{"role":"user","content":[{"type":"tool_use","input":{"secretco":1,"secretco":2}}]}]}"#,
        ] {
            let e = scrub_request(b.as_bytes(), &test_pairs()).unwrap_err();
            assert!(matches!(e, RequestError::DuplicateKey), "{b}");
            assert!(!e.to_string().contains("secretco"));
        }
        // Same key in different objects is fine.
        assert!(scrub_request(br#"{"a":{"k":1},"b":{"k":2},"c":[{"k":1},{"k":1}]}"#, &test_pairs()).is_ok());
    }
    #[test]
    fn root_type_does_not_trigger_block_rules() {
        let b = br#"{"type":"thinking","note":"acmewidgets"}"#;
        assert_eq!(scrub_request(b, &test_pairs()).unwrap().replacements, 1);
    }
    fn scrub_str(b: &str) -> (String, usize) {
        let s = scrub_request(b.as_bytes(), &test_pairs()).unwrap();
        (String::from_utf8(s.body).unwrap(), s.replacements)
    }
    #[test]
    fn type_values_outside_user_data_are_kept() {
        let b = r#"{"messages":[{"role":"user","content":[{"type":"acmewidgets"}]}]}"#;
        assert_eq!(scrub_str(b), (b.to_string(), 0));
    }
    #[test]
    fn role_values_are_kept() {
        let b = r#"{"messages":[{"role":"acmewidgets","content":"hi"}]}"#;
        assert_eq!(scrub_str(b), (b.to_string(), 0));
    }
    #[test]
    fn signature_on_a_non_thinking_block_is_kept() {
        let b = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi","signature":"acmewidgets"}]}]}"#;
        assert_eq!(scrub_str(b), (b.to_string(), 0));
    }
    #[test]
    fn keys_outside_user_data_are_never_rewritten() {
        let b = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi","acmewidgets":"v"}]}]}"#;
        assert_eq!(scrub_str(b), (b.to_string(), 0));
    }
    #[test]
    fn root_object_is_not_a_tool_use_block() {
        let b = r#"{"type":"tool_use","input":{"acmewidgets":"acmewidgets"},"messages":[]}"#;
        let want = r#"{"type":"tool_use","input":{"acmewidgets":"client_x01"},"messages":[]}"#;
        assert_eq!(scrub_str(b), (want.to_string(), 1));
    }
}
