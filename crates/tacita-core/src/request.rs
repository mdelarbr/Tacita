//! Request scrubbing policy: which string literals of a Claude Messages API
//! body may have business names replaced.

use crate::json::{self, Lit, ScanError, Seg};
use crate::pairs::Pairs;
use serde_json::Value;
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
    /// Scannable but rejected by serde_json (lone surrogate, nesting too deep).
    /// The message is serde's own text and never contains body content.
    #[error("request body is not valid JSON: {0}")]
    Invalid(String),
}

const TOOL_USE_TYPES: [&str; 3] = ["tool_use", "server_tool_use", "mcp_tool_use"];
const PROTECTED_VALUE_KEYS: [&str; 6] = ["type", "role", "id", "tool_use_id", "signature", "media_type"];

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

        if matches!(parent_type, Some("thinking" | "redacted_thinking")) {
            return false;
        }
        // User-data subtrees: everything inside is scrubbed, keys included.
        let user_data = (key == Some("input") && parent_type.is_some_and(|t| TOOL_USE_TYPES.contains(&t)))
            || (i == 2
                && key == Some("input_schema")
                && matches!(path[0], Seg::Key(ref k) if k == "tools")
                && matches!(path[1], Seg::Index(_)));
        if user_data {
            return !is_own_key;
        }
        if is_own_key {
            return false;
        }
        match key {
            Some("cache_control") => return false,
            Some("data") if i >= 1 && path[i - 1] == Seg::Key("source".into()) => return false,
            Some("model" | "metadata") if i == 0 => return false,
            Some("system") if i == 0 && child.is_string() => return false,
            Some("name") if i == 1 && path[0] == Seg::Key("tool_choice".into()) => return false,
            Some("name") if i == 2 && matches!(path[1], Seg::Index(_)) && path[0] == Seg::Key("tools".into()) => {
                return false;
            }
            Some("name") if last && parent_type.is_some_and(|t| TOOL_USE_TYPES.contains(&t)) => return false,
            Some(k) if last && PROTECTED_VALUE_KEYS.contains(&k) => return false,
            _ => {}
        }
        if i == 1 && matches!(seg, Seg::Index(0 | 1)) && path[0] == Seg::Key("system".into()) {
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
}
