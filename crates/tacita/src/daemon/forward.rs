//! Upstream request headers, the HTTP client, and response header filtering.

use axum::http::header::{ACCEPT_ENCODING, AUTHORIZATION, CONNECTION};
use axum::http::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use tacita_core::openrouter::adapt_beta;

use super::Timing;

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Copies every header except hop-by-hop ones (including any named in
/// `Connection`) and those `skip` names, keeping repeated values in order.
fn copy_end_to_end(src: &HeaderMap, skip: impl Fn(&str) -> bool) -> HeaderMap {
    let listed: Vec<String> = src
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in src {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || listed.iter().any(|l| l == n) || skip(n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Claude Code's headers for Anthropic, unchanged apart from hop-by-hop
/// headers, `host` and `content-length` (the client sets both). A protected
/// request asks for an uncompressed reply so it can be restored.
pub fn anthropic_headers(incoming: &HeaderMap, protected: bool) -> HeaderMap {
    let mut out = copy_end_to_end(incoming, |n| n == "host" || n == "content-length");
    if protected {
        out.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    out
}

/// OpenRouter's request headers, built from an allowlist: `content-type`,
/// `accept` and `anthropic-version` are copied, `anthropic-beta` loses its
/// `oauth-*` items, `authorization` is the OpenRouter key. Nothing else
/// from the incoming request goes. `None` when the key cannot be a header
/// value.
pub fn openrouter_headers(incoming: &HeaderMap, key: &str) -> Option<HeaderMap> {
    let mut auth = HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
    auth.set_sensitive(true);
    let mut out = HeaderMap::new();
    for name in ["content-type", "accept", "anthropic-version"] {
        for value in incoming.get_all(name) {
            out.append(name, value.clone());
        }
    }
    let beta: Vec<String> = incoming
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(adapt_beta)
        .collect();
    if !beta.is_empty()
        && let Ok(value) = HeaderValue::from_str(&beta.join(","))
    {
        out.insert("anthropic-beta", value);
    }
    out.insert(AUTHORIZATION, auth);
    out.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    Some(out)
}

/// The upstream's headers for the client. `content-length` goes when the
/// body is rewritten; CORS headers always go, since Tacita sends none.
pub fn response_headers(upstream: &HeaderMap, body_rewritten: bool) -> HeaderMap {
    copy_end_to_end(upstream, |n| {
        (body_rewritten && n == "content-length") || n.starts_with("access-control-")
    })
}

/// No redirects, no proxies, no total timeout (replies stream for minutes).
/// reqwest is built without decompression, so bodies arrive as sent.
pub fn client(t: &Timing) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(t.connect_timeout)
        .build()
        .expect("the HTTP client builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn pairs(h: &HeaderMap) -> Vec<(String, String)> {
        h.iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
            .collect()
    }

    const HOP: [(&str, &str); 9] = [
        ("connection", "keep-alive, x-private"),
        ("keep-alive", "timeout=5"),
        ("proxy-authenticate", "Basic"),
        ("proxy-authorization", "Basic abc"),
        ("te", "trailers"),
        ("trailer", "x-t"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "h2c"),
        ("x-private", "listed in connection"),
    ];

    #[test]
    fn anthropic_headers_copy_all_but_hop_by_hop_host_and_length() {
        let mut all = vec![
            ("host", "127.0.0.1:47821"),
            ("content-length", "12"),
            ("authorization", "Bearer sk-ant-oat01-T"),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("anthropic-beta", "claude-code-20250219"),
            ("x-claude-code-session-id", "s"),
            ("accept-encoding", "gzip, br"),
            ("sec-fetch-mode", "cors"),
        ];
        all.extend(HOP);
        let out = anthropic_headers(&headers(&all), false);
        assert_eq!(
            pairs(&out),
            vec![
                ("authorization".into(), "Bearer sk-ant-oat01-T".into()),
                ("anthropic-beta".into(), "oauth-2025-04-20".into()),
                ("anthropic-beta".into(), "claude-code-20250219".into()),
                ("x-claude-code-session-id".into(), "s".into()),
                ("accept-encoding".into(), "gzip, br".into()),
                ("sec-fetch-mode".into(), "cors".into()),
            ]
        );
    }

    #[test]
    fn protected_requests_ask_for_identity_encoding() {
        let h = headers(&[("accept-encoding", "gzip"), ("accept-encoding", "br")]);
        let out = anthropic_headers(&h, true);
        assert_eq!(
            pairs(&out),
            vec![("accept-encoding".into(), "identity".into())]
        );
        let out = anthropic_headers(&HeaderMap::new(), true);
        assert_eq!(out["accept-encoding"], "identity");
        assert_eq!(
            anthropic_headers(&h, false)
                .get_all("accept-encoding")
                .iter()
                .count(),
            2
        );
    }

    #[test]
    fn response_headers_drop_hop_by_hop_and_cors() {
        let mut all = vec![
            ("content-type", "text/event-stream"),
            ("content-length", "99"),
            ("content-encoding", "gzip"),
            ("retry-after", "30"),
            ("x-should-retry", "true"),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("request-id", "req_1"),
            ("access-control-allow-origin", "*"),
            ("Access-Control-Allow-Headers", "x"),
        ];
        all.extend(HOP);
        let h = headers(&all);
        let kept = |rewritten| {
            pairs(&response_headers(&h, rewritten))
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kept(false),
            [
                "content-type",
                "content-length",
                "content-encoding",
                "retry-after",
                "x-should-retry",
                "anthropic-ratelimit-unified-status",
                "request-id"
            ]
        );
        assert!(!kept(true).contains(&"content-length".to_string()));
        assert_eq!(kept(true).len(), 6);
        assert_eq!(response_headers(&h, false)["retry-after"], "30");
    }

    #[test]
    fn openrouter_headers_are_an_allowlist() {
        let h = headers(&[
            ("host", "127.0.0.1:1"),
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "oauth-2025-04-20,claude-code-20250219"),
            ("anthropic-beta", "interleaved-thinking-2025-05-14"),
            ("authorization", "Bearer sk-ant-oat01-T"),
            ("x-api-key", "sk-ant-api03-X"),
            ("cookie", "a=b"),
            ("x-claude-code-session-id", "s"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("accept-encoding", "gzip"),
        ]);
        let out = openrouter_headers(&h, "sk-or-v1-K").unwrap();
        assert_eq!(
            pairs(&out),
            vec![
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "text/event-stream".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
                (
                    "anthropic-beta".into(),
                    "claude-code-20250219,interleaved-thinking-2025-05-14".into()
                ),
                ("authorization".into(), "Bearer sk-or-v1-K".into()),
                ("accept-encoding".into(), "identity".into()),
            ]
        );
        let only_oauth = headers(&[("anthropic-beta", "oauth-2025-04-20")]);
        assert!(
            openrouter_headers(&only_oauth, "k")
                .unwrap()
                .get("anthropic-beta")
                .is_none()
        );
        assert!(openrouter_headers(&h, "bad\nkey").is_none());
    }

    #[test]
    fn client_builds() {
        let _ = client(&Timing::default());
    }
}
