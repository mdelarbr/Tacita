//! Request adapter for OpenRouter's Anthropic-compatible endpoint.

use serde_json::{Map, Value, json};

/// Caller preferences for provider routing.
#[derive(Debug, Default)]
pub struct Prefs {
    pub only: Vec<String>,
}

/// Why a body could not be adapted. Messages never quote body content.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    #[error("request body is not a valid JSON object: {0}")]
    Json(String),
    #[error("model {0} is not a claude-* model")]
    UnknownModel(String),
    #[error("message {0} is empty after removing thinking blocks")]
    EmptyMessage(usize),
    #[error("request already has a provider field")]
    ProviderFieldPresent,
}

/// Rewrites a scrubbed Messages body for OpenRouter: model slug, no thinking blocks, no `user_id`, privacy preferences.
pub fn adapt_request(body: &[u8], slug: &str, prefs: &Prefs) -> Result<Vec<u8>, AdaptError> {
    let mut root: Value =
        serde_json::from_slice(body).map_err(|e| AdaptError::Json(e.to_string()))?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| AdaptError::Json("not a JSON object".into()))?;
    match obj.get("model") {
        Some(Value::String(m)) if m.starts_with("claude-") => {}
        Some(Value::String(m)) => return Err(AdaptError::UnknownModel(m.clone())),
        Some(other) => return Err(AdaptError::UnknownModel(other.to_string())),
        None => return Err(AdaptError::UnknownModel(String::new())),
    }
    if obj.contains_key("provider") {
        return Err(AdaptError::ProviderFieldPresent);
    }
    obj.insert("model".into(), Value::String(slug.into()));
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for (i, message) in messages.iter_mut().enumerate() {
            if let Some(Value::Array(blocks)) = message.get_mut("content") {
                let before = blocks.len();
                blocks.retain(|b| {
                    !matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                });
                if blocks.is_empty() && before > 0 {
                    return Err(AdaptError::EmptyMessage(i));
                }
            }
        }
    }
    if let Some(Value::Object(meta)) = obj.get_mut("metadata") {
        meta.shift_remove("user_id");
    }
    let mut provider = Map::new();
    provider.insert("data_collection".into(), json!("deny"));
    provider.insert("zdr".into(), json!(true));
    provider.insert("allow_fallbacks".into(), json!(false));
    if !prefs.only.is_empty() {
        provider.insert("only".into(), json!(prefs.only));
    }
    obj.insert("provider".into(), Value::Object(provider));
    serde_json::to_vec(&root).map_err(|e| AdaptError::Json(e.to_string()))
}

/// Drops `oauth-*` items from an `anthropic-beta` value; `None` when nothing is left.
pub fn adapt_beta(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|i| !i.is_empty() && !i.starts_with("oauth-"))
        .collect();
    (!kept.is_empty()).then(|| kept.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &str, only: &[&str]) -> Result<Value, AdaptError> {
        let prefs = Prefs {
            only: only.iter().map(|s| s.to_string()).collect(),
        };
        adapt_request(body.as_bytes(), "qwen/qwen3-coder", &prefs)
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn adapts_body_for_openrouter() {
        let body = br#"{"model":"claude-opus-5-5","metadata":{"user_id":"u"},"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"hi"}]}]}"#;
        let v: Value = serde_json::from_slice(
            &adapt_request(
                body,
                "qwen/qwen3-coder",
                &Prefs {
                    only: vec!["groq".into()],
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v["model"], "qwen/qwen3-coder");
        assert!(v["metadata"].get("user_id").is_none());
        assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["provider"],
            json!({"data_collection":"deny","zdr":true,"allow_fallbacks":false,"only":["groq"]})
        );
    }

    #[test]
    fn unknown_model_fails_closed() {
        assert!(
            matches!(adapt_request(br#"{"model":"gpt-5","messages":[]}"#, "x/y", &Prefs { only: vec![] }), Err(AdaptError::UnknownModel(m)) if m == "\"gpt-5\"" || m == "gpt-5")
        );
    }

    #[test]
    fn missing_or_non_string_model_is_unknown() {
        assert!(
            matches!(run(r#"{"messages":[]}"#, &[]), Err(AdaptError::UnknownModel(m)) if m.is_empty())
        );
        assert!(
            matches!(run(r#"{"model":42}"#, &[]), Err(AdaptError::UnknownModel(m)) if m == "42")
        );
        assert!(
            matches!(run(r#"{"model":null}"#, &[]), Err(AdaptError::UnknownModel(m)) if m == "null")
        );
    }

    #[test]
    fn invalid_json_or_non_object_gives_json_error() {
        let secret = "ACME-secret";
        let e = run(&format!("{{\"model\":\"{secret}"), &[]).unwrap_err();
        assert!(matches!(e, AdaptError::Json(_)));
        assert!(!e.to_string().contains(secret));
        assert!(matches!(run("[1]", &[]), Err(AdaptError::Json(_))));
        assert!(matches!(run("\"claude-x\"", &[]), Err(AdaptError::Json(_))));
        let e = adapt_request(b"\xff\xfe", "x/y", &Prefs::default()).unwrap_err();
        assert!(matches!(e, AdaptError::Json(_)));
    }

    #[test]
    fn thinking_only_message_fails() {
        let body = r#"{"model":"claude-x","messages":[{"role":"user","content":"q"},{"role":"assistant","content":[{"type":"redacted_thinking","data":"d"}]}]}"#;
        assert!(matches!(run(body, &[]), Err(AdaptError::EmptyMessage(1))));
        let body = r#"{"model":"claude-x","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"t"}]}]}"#;
        assert!(matches!(run(body, &[]), Err(AdaptError::EmptyMessage(0))));
    }

    #[test]
    fn string_missing_and_already_empty_content_left_alone() {
        let body = r#"{"model":"claude-x","messages":[{"role":"user","content":""},{"role":"user"},{"role":"user","content":[]}]}"#;
        let v = run(body, &[]).unwrap();
        assert_eq!(v["messages"][0]["content"], "");
        assert!(v["messages"][1].get("content").is_none());
        assert_eq!(v["messages"][2]["content"], json!([]));
    }

    #[test]
    fn existing_provider_field_fails() {
        assert!(matches!(
            run(r#"{"model":"claude-x","provider":{}}"#, &[]),
            Err(AdaptError::ProviderFieldPresent)
        ));
        assert!(matches!(
            run(r#"{"model":"claude-x","provider":null}"#, &[]),
            Err(AdaptError::ProviderFieldPresent)
        ));
    }

    #[test]
    fn metadata_keeps_other_keys_and_stays_when_empty() {
        let v = run(
            r#"{"model":"claude-x","metadata":{"user_id":"u","k":1}}"#,
            &[],
        )
        .unwrap();
        assert_eq!(v["metadata"], json!({"k":1}));
        let v = run(r#"{"model":"claude-x","metadata":{"user_id":"u"}}"#, &[]).unwrap();
        assert_eq!(v["metadata"], json!({}));
        let v = run(r#"{"model":"claude-x"}"#, &[]).unwrap();
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn top_level_thinking_param_unchanged() {
        let v = run(
            r#"{"model":"claude-x","thinking":{"type":"enabled","budget_tokens":1024}}"#,
            &[],
        )
        .unwrap();
        assert_eq!(
            v["thinking"],
            json!({"type":"enabled","budget_tokens":1024})
        );
    }

    #[test]
    fn provider_is_last_with_ordered_keys_and_no_only_when_empty() {
        let out = adapt_request(
            br#"{"model":"claude-x","max_tokens":1.0,"stream":true}"#,
            "a/b",
            &Prefs::default(),
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            r#"{"model":"a/b","max_tokens":1.0,"stream":true,"provider":{"data_collection":"deny","zdr":true,"allow_fallbacks":false}}"#
        );
        let out = adapt_request(
            br#"{"model":"claude-x"}"#,
            "a/b",
            &Prefs {
                only: vec!["g".into(), "h".into()],
            },
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with(r#""allow_fallbacks":false,"only":["g","h"]}}"#)
        );
    }

    #[test]
    fn beta_header_loses_only_oauth() {
        assert_eq!(
            adapt_beta("oauth-2025-04-20, claude-code-20250219").as_deref(),
            Some("claude-code-20250219")
        );
        assert_eq!(adapt_beta("oauth-2025-04-20"), None);
        assert_eq!(adapt_beta("a,,b").as_deref(), Some("a,b"));
        assert_eq!(adapt_beta(""), None);
        assert_eq!(adapt_beta(" , "), None);
        assert_eq!(adapt_beta(" a , oauth-x ,b ").as_deref(), Some("a,b"));
    }
}
