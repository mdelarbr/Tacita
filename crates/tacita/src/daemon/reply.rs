//! The reply in a protected mode: a 2xx is restored or refused, never passed
//! on as it came; any other status is passed on verbatim.

use axum::body::Body;
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use tacita_core::restore::{Limits, Restorer, restore_message};

use super::{App, MetaHandle, bounded, forward, pass_through, short_reason, stream};

/// Largest non-streamed reply read for restoring.
const MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

enum Media {
    EventStream,
    Json,
}

/// The one `content-type`, by type/subtype before any parameter.
fn media_type(headers: &HeaderMap) -> Option<Media> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let essence = value.to_str().ok()?.split(';').next()?.trim();
    if essence.eq_ignore_ascii_case("text/event-stream") {
        Some(Media::EventStream)
    } else if essence.eq_ignore_ascii_case("application/json") {
        Some(Media::Json)
    } else {
        None
    }
}

fn is_identity(headers: &HeaderMap) -> bool {
    headers.get_all(CONTENT_ENCODING).iter().all(|v| {
        v.to_str()
            .is_ok_and(|e| e.trim().eq_ignore_ascii_case("identity"))
    })
}

/// The reply in a protected mode. A 2xx is restored or refused with a 502,
/// never passed on as it came; any other status is passed on verbatim.
pub(super) async fn protected_reply(
    app: &App,
    upstream: reqwest::Response,
    mut m: MetaHandle,
) -> Response {
    let status = upstream.status();
    if !status.is_success() {
        return pass_through(upstream, m);
    }
    let headers = upstream.headers().clone();
    m.meta.upstream_response(status, &headers);
    let identity = is_identity(&headers);
    let body = match media_type(&headers) {
        Some(Media::EventStream) if !identity => {
            return no_retry(
                m,
                "compressed stream from upstream; refusing to pass it on unrestored",
            );
        }
        Some(Media::EventStream) => {
            let restorer = Restorer::new(
                app.pairs.clone(),
                Limits::default(),
                app.config.step0.stream_probe.clone(),
            );
            stream::restoring_body(upstream.bytes_stream(), restorer, app.opts.timing, m)
        }
        Some(Media::Json) if !identity => return unrestorable(m, "it is compressed"),
        Some(Media::Json) => {
            let raw = match read_reply(upstream).await {
                Ok(raw) => raw,
                Err(ReadError::TooLarge) => return unrestorable(m, "it is larger than 64 MiB"),
                Err(ReadError::Broken(cause)) => {
                    // Transient: Claude Code may retry it.
                    let reason = format!("it broke off: {cause}");
                    let message = format!("could not restore the reply: {}", bounded(&reason));
                    return m.fail(StatusCode::BAD_GATEWAY, &message);
                }
            };
            match restore_message(&raw, &app.pairs) {
                Ok((restored, n)) => {
                    m.meta.restored = Some(n);
                    Body::from(restored)
                }
                Err(e) => return unrestorable(m, &e.to_string()),
            }
        }
        None => return unrestorable(m, "it is neither an event stream nor JSON"),
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = forward::response_headers(&headers, true);
    resp
}

/// A 502 with a fixed cause: retrying cannot help, so Claude Code is told
/// not to (it retries every 5xx otherwise).
fn no_retry(m: MetaHandle, message: &str) -> Response {
    let mut resp = m.fail(StatusCode::BAD_GATEWAY, message);
    resp.headers_mut()
        .insert("x-should-retry", HeaderValue::from_static("false"));
    resp
}

fn unrestorable(m: MetaHandle, reason: &str) -> Response {
    no_retry(
        m,
        &format!("could not restore the reply: {}", bounded(reason)),
    )
}

enum ReadError {
    TooLarge,
    /// The upstream broke off; the kind and root cause.
    Broken(String),
}

/// Reads a whole non-streamed reply, up to `MAX_REPLY_BYTES`.
async fn read_reply(upstream: reqwest::Response) -> Result<Vec<u8>, ReadError> {
    let mut stream = upstream.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ReadError::Broken(short_reason(&e)))?;
        if out.len() + chunk.len() > MAX_REPLY_BYTES {
            return Err(ReadError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
