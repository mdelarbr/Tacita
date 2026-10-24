//! Tacita's own errors, in the Anthropic error shape.

use crate::json::encode;
use crate::sse::SseEvent;

fn body(message: &str) -> String {
    let message = encode(&format!("Tacita: {message}"));
    format!(r#"{{"type":"error","error":{{"type":"invalid_request_error","message":{message}}}}}"#)
}

/// `{"type":"error","error":{"type":"invalid_request_error","message":"Tacita: <message>"}}`.
pub fn error_body(message: &str) -> Vec<u8> {
    body(message).into_bytes()
}

/// An SSE `error` event whose data is `error_body(message)`. The data is one
/// line, because `encode` escapes every newline.
pub fn error_event(message: &str) -> SseEvent {
    SseEvent::build("error", &body(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn error_body_shape() {
        let v: Value = serde_json::from_slice(&error_body("x")).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "Tacita: x");
    }

    #[test]
    fn error_body_escapes_message() {
        let msg = "bad \"quote\" \\ and\nnewline";
        let v: Value = serde_json::from_slice(&error_body(msg)).unwrap();
        assert_eq!(v["error"]["message"], format!("Tacita: {msg}"));
    }

    #[test]
    fn error_event_carries_the_body() {
        let e = error_event("line one\nline two");
        assert_eq!(e.event.as_deref(), Some("error"));
        assert_eq!(e.data.as_bytes(), error_body("line one\nline two"));
        assert_eq!(e.raw, SseEvent::build("error", &e.data).raw);
        assert!(!e.data.contains('\n'));
    }
}
