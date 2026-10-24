//! Pure routing and local-access checks.

use axum::http::header::HOST;
use axum::http::{HeaderMap, Method, Uri};

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Messages { query: Option<String> },
    CountTokens,
    Hello,
    NotFound,
    Forbidden(&'static str),
}

/// Headers a browser sends to 127.0.0.1, with the reason given when one is
/// present. `sec-fetch-mode` is not here: Node's fetch adds
/// `sec-fetch-mode: cors` to every request, and a browser always sends
/// `Origin` or `Sec-Fetch-Site` as well.
const BROWSER_HEADERS: [(&str, &str); 5] = [
    ("origin", "browser requests are refused (Origin header)"),
    ("referer", "browser requests are refused (Referer header)"),
    (
        "sec-fetch-site",
        "browser requests are refused (Sec-Fetch-Site header)",
    ),
    (
        "sec-fetch-dest",
        "browser requests are refused (Sec-Fetch-Dest header)",
    ),
    (
        "sec-fetch-user",
        "browser requests are refused (Sec-Fetch-User header)",
    ),
];

const FOREIGN_HOST: &str = "the Host header is not 127.0.0.1 or localhost on Tacita's port";

/// Checks, in order: the Host header, browser headers, the secret path
/// prefix, then the method and path. The query never affects the route.
pub fn route(method: &Method, uri: &Uri, headers: &HeaderMap, secret: &str, port: u16) -> Route {
    if !host_is_local(headers, port) {
        return Route::Forbidden(FOREIGN_HOST);
    }
    if let Some((_, reason)) = BROWSER_HEADERS
        .iter()
        .find(|(name, _)| headers.contains_key(*name))
    {
        return Route::Forbidden(reason);
    }
    let Some(rest) = strip_secret(uri.path(), secret) else {
        return Route::NotFound;
    };
    if method == Method::POST && rest == "/v1/messages" {
        Route::Messages {
            query: uri.query().map(str::to_string),
        }
    } else if method == Method::POST && rest == "/v1/messages/count_tokens" {
        Route::CountTokens
    } else if (method == Method::HEAD || method == Method::GET) && rest == "/api/hello" {
        Route::Hello
    } else {
        Route::NotFound
    }
}

/// Exactly one Host header, equal to `127.0.0.1:{port}` or `localhost:{port}`.
fn host_is_local(headers: &HeaderMap, port: u16) -> bool {
    let mut all = headers.get_all(HOST).iter();
    let (Some(host), None) = (all.next(), all.next()) else {
        return false;
    };
    let Ok(host) = host.to_str() else {
        return false;
    };
    let Some((name, p)) = host.rsplit_once(':') else {
        return false;
    };
    matches!(name, "127.0.0.1" | "localhost") && p == port.to_string()
}

/// The rest of `path` after `/{secret}`, starting with `/`, when the first
/// segment is the secret.
pub fn strip_secret<'a>(path: &'a str, secret: &str) -> Option<&'a str> {
    let rest = path.strip_prefix('/')?;
    let (segment, tail) = rest.split_at(rest.find('/')?);
    constant_time_eq(segment.as_bytes(), secret.as_bytes()).then_some(tail)
}

/// Lengths first, then every byte of the secret, with no early exit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "0123456789abcdef0123456789abcdef";
    const PORT: u16 = 47821;

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

    fn local() -> HeaderMap {
        headers(&[("host", "127.0.0.1:47821")])
    }

    fn r(method: Method, uri: &str, h: &HeaderMap) -> Route {
        route(&method, &uri.parse().unwrap(), h, S, PORT)
    }

    #[test]
    fn known_paths() {
        let h = local();
        assert_eq!(
            r(Method::POST, &format!("/{S}/v1/messages?beta=true"), &h),
            Route::Messages {
                query: Some("beta=true".into())
            }
        );
        assert_eq!(
            r(Method::POST, &format!("/{S}/v1/messages"), &h),
            Route::Messages { query: None }
        );
        assert_eq!(
            r(
                Method::POST,
                &format!("/{S}/v1/messages/count_tokens?beta=true"),
                &h
            ),
            Route::CountTokens
        );
        assert_eq!(
            r(Method::HEAD, &format!("/{S}/api/hello"), &h),
            Route::Hello
        );
        assert_eq!(r(Method::GET, &format!("/{S}/api/hello"), &h), Route::Hello);
    }

    #[test]
    fn unknown_paths_and_methods() {
        let h = local();
        for (m, p) in [
            (Method::GET, "/v1/messages"),
            (Method::POST, "/v1/messages/"),
            (Method::POST, "/v1/messages/batches"),
            (Method::GET, "/v1/models"),
            (Method::POST, "/api/hello"),
            (Method::POST, "//v1/messages"),
            (Method::POST, "/"),
        ] {
            assert_eq!(
                r(m.clone(), &format!("/{S}{p}"), &h),
                Route::NotFound,
                "{m} {p}"
            );
        }
    }

    #[test]
    fn wrong_or_missing_secret_is_not_found() {
        let h = local();
        let wrong = "f".repeat(32);
        for p in [
            format!("/{wrong}/v1/messages"),
            format!("/{}/v1/messages", &S[..31]),
            format!("/{S}0/v1/messages"),
            format!("/{S}"),
            "/v1/messages".to_string(),
            "/".to_string(),
        ] {
            assert_eq!(r(Method::POST, &p, &h), Route::NotFound, "{p}");
        }
    }

    #[test]
    fn strip_secret_cases() {
        assert_eq!(strip_secret(&format!("/{S}/v1/x"), S), Some("/v1/x"));
        assert_eq!(strip_secret(&format!("/{S}/"), S), Some("/"));
        assert_eq!(strip_secret(&format!("/{S}"), S), None);
        assert_eq!(strip_secret(&format!("{S}/v1"), S), None);
        assert_eq!(strip_secret("/abc/v1", S), None);
        assert_eq!(strip_secret("", S), None);
    }

    #[test]
    fn host_must_be_local_on_this_port() {
        let p = format!("/{S}/v1/messages");
        for host in ["127.0.0.1:47821", "localhost:47821"] {
            let h = headers(&[("host", host)]);
            assert_eq!(
                r(Method::POST, &p, &h),
                Route::Messages { query: None },
                "{host}"
            );
        }
        for host in [
            "evil.example:47821",
            "127.0.0.1:47822",
            "127.0.0.1",
            "localhost",
            "[::1]:47821",
            "0.0.0.0:47821",
            "127.0.0.1:47821.evil.example",
        ] {
            let h = headers(&[("host", host)]);
            assert!(
                matches!(r(Method::POST, &p, &h), Route::Forbidden(_)),
                "{host}"
            );
        }
        assert!(matches!(
            r(Method::POST, &p, &HeaderMap::new()),
            Route::Forbidden(_)
        ));
        let two = headers(&[("host", "127.0.0.1:47821"), ("host", "127.0.0.1:47821")]);
        assert!(matches!(r(Method::POST, &p, &two), Route::Forbidden(_)));
    }

    #[test]
    fn browser_headers_are_forbidden_but_sec_fetch_mode_alone_is_not() {
        let p = format!("/{S}/v1/messages");
        for name in [
            "origin",
            "referer",
            "sec-fetch-site",
            "sec-fetch-dest",
            "sec-fetch-user",
        ] {
            let h = headers(&[("host", "127.0.0.1:47821"), (name, "x")]);
            assert!(
                matches!(r(Method::POST, &p, &h), Route::Forbidden(_)),
                "{name}"
            );
        }
        let h = headers(&[("host", "127.0.0.1:47821"), ("sec-fetch-mode", "cors")]);
        assert_eq!(r(Method::POST, &p, &h), Route::Messages { query: None });
    }

    #[test]
    fn forbidden_is_decided_before_the_secret() {
        let h = headers(&[("host", "evil.example:47821")]);
        assert!(matches!(
            r(Method::POST, "/nope/v1/messages", &h),
            Route::Forbidden(_)
        ));
        let h = headers(&[("host", "127.0.0.1:47821"), ("origin", "https://x")]);
        assert!(matches!(r(Method::GET, "/", &h), Route::Forbidden(_)));
    }
}
