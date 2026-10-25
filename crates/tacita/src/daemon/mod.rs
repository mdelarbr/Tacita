//! The daemon: a local HTTP server that routes, forwards and logs.

pub mod forward;
pub mod metalog;
pub mod record;
mod reply;
pub mod route;
mod stream;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::serve::ListenerExt;
use bytes::Bytes;
use futures_util::StreamExt;
use tacita_core::errors::error_body;
use tacita_core::openrouter::{Prefs, adapt_request};
use tacita_core::pairs::Pairs;
use tacita_core::request::scrub_request;

use crate::config::{self, Config, Protection, Provider};
use crate::secrets::SecretStore;
use metalog::{MetaLog, Observer, RequestMeta};
use record::{Capture, Recorder};
use route::Route;

/// The real upstreams. Code constants, never config values.
const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";
const OPENROUTER_UPSTREAM: &str = "https://openrouter.ai/api";

/// Largest request body read.
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Longest reason put into a Tacita error or `tacita_error`.
const MAX_REASON_BYTES: usize = 200;

/// The hello header holding the daemon's `config_tag`.
pub const CONFIG_TAG_HEADER: &str = "x-tacita-config";

const BLOCK_PROBE: &str = "block probe: this request was blocked on purpose (Step 0)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub ping_every: Duration,
    pub hold_limit: Duration,
    pub connect_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            ping_every: Duration::from_secs(10),
            hold_limit: Duration::from_secs(480),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

pub struct DaemonOptions {
    pub home: PathBuf,
    pub anthropic_upstream: String,
    pub openrouter_upstream: String,
    pub secrets: Arc<dyn SecretStore>,
    pub timing: Timing,
}

impl DaemonOptions {
    #[cfg(target_os = "macos")]
    pub fn production(home: PathBuf) -> DaemonOptions {
        DaemonOptions {
            home,
            anthropic_upstream: ANTHROPIC_UPSTREAM.to_string(),
            openrouter_upstream: OPENROUTER_UPSTREAM.to_string(),
            secrets: Arc::new(crate::secrets::KeychainStore::new("dev.tacita")),
            timing: Timing::default(),
        }
    }
}

struct App {
    opts: DaemonOptions,
    config: Config,
    /// `config_tag` of the `config.toml` bytes `config` was loaded from.
    config_tag: String,
    /// The listener's port, for the Host check.
    port: u16,
    /// `host[:port]` of the Anthropic upstream, for error messages.
    anthropic_host: String,
    client: reqwest::Client,
    /// The Step 0 dictionary, built once from `config`.
    pairs: Arc<Pairs>,
    log: Arc<MetaLog>,
    recorder: Option<Arc<Recorder>>,
    next: AtomicU64,
}

/// Serves until the listener fails: `Server::new` then `Server::run`.
pub async fn serve(opts: DaemonOptions, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    Server::new(opts, listener)?.run().await
}

/// A daemon that has checked its listener, loaded `Config` and opened its
/// request log, so startup errors surface before anything is served.
pub struct Server {
    app: Arc<App>,
    listener: tokio::net::TcpListener,
}

impl Server {
    /// Loads `Config` once; it is not reloaded while serving. A request
    /// that finds `config.toml` changed since is refused.
    pub fn new(opts: DaemonOptions, listener: tokio::net::TcpListener) -> anyhow::Result<Server> {
        let addr = listener.local_addr()?;
        anyhow::ensure!(
            addr.ip() == Ipv4Addr::LOCALHOST,
            "Tacita listens on 127.0.0.1 only, not {}",
            addr.ip()
        );
        let (config, config_tag) = Config::load_tagged(&opts.home)?;
        let anthropic_host = host_label(&opts.anthropic_upstream)?;
        let pairs = config.pairs()?;
        let log = MetaLog::open(&opts.home).context("cannot open logs/requests.jsonl")?;
        let recorder = match &config.step0.record_dir {
            Some(dir) => Some(Arc::new(open_recorder(dir)?)),
            None => None,
        };
        let app = Arc::new(App {
            client: forward::client(&opts.timing),
            pairs,
            opts,
            config,
            config_tag,
            port: addr.port(),
            anthropic_host,
            log: Arc::new(log),
            recorder,
            next: AtomicU64::new(1),
        });
        Ok(Server { app, listener })
    }

    /// Serves until the listener fails. `State` is read on every request.
    pub async fn run(self) -> anyhow::Result<()> {
        let router = Router::new().fallback(handle).with_state(self.app);
        let listener = self.listener.tap_io(|tcp| {
            // Small SSE writes go out at once.
            let _ = tcp.set_nodelay(true);
        });
        axum::serve(listener, router).await?;
        Ok(())
    }
}

/// Creates this run's corpus directory.
/// The created directory is checked again, resolved, because the config
/// check cannot see a repo made since, nor every way a path can lead into
/// one; inside a repo the daemon refuses to start.
fn open_recorder(dir: &std::path::Path) -> anyhow::Result<Recorder> {
    let rec = Recorder::for_run(dir)
        .with_context(|| format!("cannot create step0.record_dir {}", dir.display()))?;
    let real = rec
        .dir()
        .canonicalize()
        .unwrap_or_else(|_| rec.dir().to_path_buf());
    if config::in_git_repo(&real) {
        let _ = std::fs::remove_dir(rec.dir());
        return Err(config::ConfigError::RecordDirInRepo.into());
    }
    Ok(rec)
}

fn host_label(upstream: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(upstream).context("invalid upstream URL")?;
    let host = url.host_str().context("upstream URL has no host")?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// Holds a request's meta line and writes it exactly once, when dropped:
/// after the response, after the body stream, or when the client leaves.
struct MetaHandle {
    meta: RequestMeta,
    log: Arc<MetaLog>,
    start: Instant,
    /// The corpus side copy; `None` unless `step0.record_dir` is set.
    cap: Option<Capture>,
}

impl MetaHandle {
    /// A Tacita error response; the line is written now.
    fn fail(mut self, status: StatusCode, message: &str) -> Response {
        self.meta.status = status.as_u16();
        self.meta.tacita_error = Some(message.to_string());
        let body = Bytes::from(error_body(message));
        self.cap_client(&body);
        error_response(status, body)
    }

    fn cap_forwarded(&mut self, bytes: &Bytes) {
        if let Some(c) = &mut self.cap {
            c.forwarded(bytes.clone());
        }
    }

    fn cap_upstream(&mut self, bytes: &Bytes) {
        if let Some(c) = &mut self.cap {
            c.upstream(bytes);
        }
    }

    fn cap_client(&mut self, bytes: &Bytes) {
        if let Some(c) = &mut self.cap {
            c.client(bytes);
        }
    }
}

impl Drop for MetaHandle {
    fn drop(&mut self) {
        self.meta.ms = self.start.elapsed().as_millis() as u64;
        self.log.append(&self.meta);
        if let Some(c) = self.cap.take() {
            c.finish(self.meta.n, self.meta.status);
        }
    }
}

fn error_response(status: StatusCode, body: Bytes) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();
    let secret = &app.config.secret;
    let n = app.next.fetch_add(1, Ordering::Relaxed);
    let mut m = MetaHandle {
        meta: RequestMeta::begin(n, &parts.method, &parts.uri, &parts.headers, secret),
        log: app.log.clone(),
        start,
        cap: None,
    };
    match route::route(&parts.method, &parts.uri, &parts.headers, secret, app.port) {
        Route::Forbidden(reason) => {
            m.meta.route = "forbidden";
            m.fail(StatusCode::FORBIDDEN, reason)
        }
        Route::NotFound => {
            m.meta.route = "refused";
            m.fail(
                StatusCode::NOT_FOUND,
                "this path is not handled; only POST /v1/messages is forwarded",
            )
        }
        Route::CountTokens => {
            m.meta.route = "count_tokens";
            m.fail(StatusCode::NOT_FOUND, "count_tokens is not forwarded")
        }
        Route::Hello => {
            m.meta.route = "hello";
            m.meta.status = 200;
            let mut resp = Response::new(Body::empty());
            if let Ok(tag) = HeaderValue::from_str(&app.config_tag) {
                resp.headers_mut().insert(CONFIG_TAG_HEADER, tag);
            }
            resp
        }
        Route::Messages { query } => {
            m.meta.route = "messages";
            messages(&app, m, &parts.headers, body, query).await
        }
    }
}

async fn messages(
    app: &App,
    mut m: MetaHandle,
    headers: &HeaderMap,
    body: Body,
    query: Option<String>,
) -> Response {
    let body = match read_body(body).await {
        Ok(b) => Bytes::from(b),
        Err(BodyError::TooLarge) => {
            return m.fail(StatusCode::PAYLOAD_TOO_LARGE, "request larger than 64 MiB");
        }
        Err(BodyError::Broken) => {
            return m.fail(StatusCode::BAD_REQUEST, "could not read the request body");
        }
    };
    m.meta.peek_request(&body);
    m.cap = app
        .recorder
        .clone()
        .map(|r| Capture::new(r, headers, body.clone()));
    // `tacita status` reads the file; a daemon running an older one must
    // not act on settings the owner no longer sees.
    if config::current_config_tag(&app.opts.home).ok().as_ref() != Some(&app.config_tag) {
        let mut resp = m.fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config.toml changed since the daemon started; restart `tacita daemon`",
        );
        resp.headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
        return resp;
    }
    if let Some(probe) = app.config.step0.block_probe.as_deref()
        && contains(&body, probe.as_bytes())
    {
        m.meta.status = StatusCode::BAD_REQUEST.as_u16();
        m.meta.tacita_error = Some("block probe".to_string());
        let reply = Bytes::from(error_body(BLOCK_PROBE));
        m.cap_client(&reply);
        return error_response(StatusCode::BAD_REQUEST, reply);
    }
    let state = match config::State::load(&app.opts.home) {
        Ok(s) => s,
        Err(e) => {
            let mut resp = m.fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("cannot read state.toml: {e}"),
            );
            resp.headers_mut()
                .insert("x-should-retry", HeaderValue::from_static("false"));
            return resp;
        }
    };
    let (provider, protection) = match state.provider {
        Provider::Anthropic => ("anthropic", app.config.anthropic.protection),
        Provider::Openrouter => ("openrouter", app.config.openrouter.protection),
    };
    m.meta.provider = Some(provider);
    m.meta.protection = Some(match protection {
        Protection::Off => "off",
        Protection::Pairs => "pairs",
    });
    match state.provider {
        Provider::Openrouter => return openrouter(app, m, headers, body, &state).await,
        Provider::Anthropic => {}
    }
    m.meta.model_out = m.meta.model_in.clone();
    let protected = protection != Protection::Off;
    let body = if protected {
        match scrub(app, &mut m, &body) {
            Ok(b) => Bytes::from(b),
            Err(resp) => return *resp,
        }
    } else {
        body
    };
    // Only the fixed path and the incoming query reach the upstream URL.
    let mut url = format!(
        "{}/v1/messages",
        app.opts.anthropic_upstream.trim_end_matches('/')
    );
    if let Some(q) = &query {
        url.push('?');
        url.push_str(q);
    }
    if protected {
        m.meta.reals_left = Some(reals_left(&app.pairs, &body));
    }
    m.cap_forwarded(&body);
    let sent = app
        .client
        .post(url)
        .headers(forward::anthropic_headers(headers, protected))
        .body(body)
        .send()
        .await;
    match sent {
        Ok(upstream) if upstream.status().is_redirection() => reply::redirect_refused(m),
        Ok(upstream) if protected => reply::protected_reply(app, upstream, m).await,
        Ok(upstream) => pass_through(upstream, m),
        Err(e) => {
            let message = format!("cannot reach {}: {}", app.anthropic_host, short_reason(&e));
            m.fail(StatusCode::BAD_GATEWAY, &message)
        }
    }
}

/// Scrubs the body and records the counts; `Err` is the 400 to return.
fn scrub(app: &App, m: &mut MetaHandle, body: &[u8]) -> Result<Vec<u8>, Box<Response>> {
    match scrub_request(body, &app.pairs) {
        Ok(s) => {
            m.meta.scrubbed = Some(s.replacements);
            m.meta.scrubbed_system = Some(s.in_system);
            Ok(s.body)
        }
        Err(e) => {
            let message = format!("could not read this request: {}", bounded(&e.to_string()));
            m.meta.status = StatusCode::BAD_REQUEST.as_u16();
            m.meta.tacita_error = Some(message.clone());
            let reply = Bytes::from(error_body(&message));
            m.cap_client(&reply);
            Err(Box::new(error_response(StatusCode::BAD_REQUEST, reply)))
        }
    }
}

/// Occurrences of real values in `body`, the bytes going upstream: what
/// the scrub left, in fields it never rewrites or forms it cannot see.
fn reals_left(pairs: &Pairs, body: &[u8]) -> usize {
    pairs.find_reals(&String::from_utf8_lossy(body)).len()
}

/// The OpenRouter route: model, key, scrub, adapt, then the same protected
/// reply as the Anthropic path. Nothing is forwarded on any error, and only
/// `openrouter_headers` and the adapted body reach OpenRouter.
async fn openrouter(
    app: &App,
    mut m: MetaHandle,
    headers: &HeaderMap,
    body: Bytes,
    state: &config::State,
) -> Response {
    let Some(slug) = state.openrouter_model.as_deref() else {
        return m.fail(
            StatusCode::BAD_REQUEST,
            "no OpenRouter model: run `tacita use openrouter --model <slug>`",
        );
    };
    // The Keychain may show a dialog; keep it off the async workers.
    let secrets = app.opts.secrets.clone();
    let key = match tokio::task::spawn_blocking(move || secrets.get("openrouter")).await {
        Ok(Ok(Some(k))) => k,
        Ok(Ok(None)) => {
            return m.fail(
                StatusCode::BAD_REQUEST,
                "no OpenRouter key: run `tacita key set openrouter`",
            );
        }
        Ok(Err(e)) => {
            let message = format!(
                "cannot read the OpenRouter key from the Keychain: {}",
                bounded(&e.to_string())
            );
            return m.fail(StatusCode::BAD_REQUEST, &message);
        }
        Err(_) => {
            return m.fail(
                StatusCode::BAD_REQUEST,
                "cannot read the OpenRouter key from the Keychain: the read did not finish",
            );
        }
    };
    let Some(out_headers) = forward::openrouter_headers(headers, &key) else {
        return m.fail(
            StatusCode::BAD_REQUEST,
            "the stored OpenRouter key is not usable; run `tacita key set openrouter` again",
        );
    };
    let scrubbed = match scrub(app, &mut m, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let prefs = Prefs {
        only: app.config.openrouter.only.clone(),
    };
    let body = match adapt_request(&scrubbed, slug, &prefs) {
        Ok(b) => Bytes::from(b),
        Err(e) => return m.fail(StatusCode::BAD_REQUEST, bounded(&e.to_string())),
    };
    m.meta.model_out = Some(slug.to_string());
    m.meta.reals_left = Some(reals_left(&app.pairs, &body));
    m.cap_forwarded(&body);
    let url = format!(
        "{}/v1/messages",
        app.opts.openrouter_upstream.trim_end_matches('/')
    );
    match app
        .client
        .post(url)
        .headers(out_headers)
        .body(body)
        .send()
        .await
    {
        Ok(upstream) if upstream.status().is_redirection() => reply::redirect_refused(m),
        Ok(upstream) => reply::protected_reply(app, upstream, m).await,
        Err(e) => {
            let message = format!("cannot reach OpenRouter: {}", short_reason(&e));
            m.fail(StatusCode::BAD_GATEWAY, &message)
        }
    }
}

enum BodyError {
    TooLarge,
    Broken,
}

/// Reads up to `MAX_REQUEST_BYTES`. Past that, a bounded remainder is read
/// and dropped, so the client sees the 413 rather than a reset.
async fn read_body(body: Body) -> Result<Vec<u8>, BodyError> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyError::Broken)?;
        if out.len() + chunk.len() > MAX_REQUEST_BYTES {
            drop(out);
            let mut drained = 0;
            while drained < MAX_REQUEST_BYTES {
                match stream.next().await {
                    Some(Ok(c)) => drained += c.len(),
                    _ => break,
                }
            }
            return Err(BodyError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The error kind and its root cause, never the URL (reqwest's own Display
/// includes it).
fn short_reason(e: &reqwest::Error) -> String {
    let kind = if e.is_connect() {
        "connect"
    } else if e.is_timeout() {
        "timeout"
    } else {
        "other"
    };
    let mut root = std::error::Error::source(e);
    while let Some(next) = root.and_then(std::error::Error::source) {
        root = Some(next);
    }
    match root {
        Some(cause) => format!("{kind}: {cause}"),
        None => kind.to_string(),
    }
}

/// The first `MAX_REASON_BYTES` of `reason`, cut at a char boundary.
fn bounded(reason: &str) -> &str {
    let mut end = reason.len().min(MAX_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

/// An empty needle matches nothing.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Status, headers and body bytes go to the client unchanged and unbuffered;
/// the observer reads them on the side.
fn pass_through(upstream: reqwest::Response, mut m: MetaHandle) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    m.meta.upstream_response(status, &headers);
    let mut side = Side {
        observer: Observer::new(status, &headers),
        m,
    };
    let body = upstream.bytes_stream().map(move |item| {
        match &item {
            Ok(bytes) => {
                side.observer.feed(&mut side.m.meta, bytes);
                side.m.cap_upstream(bytes);
                side.m.cap_client(bytes);
            }
            Err(e) => {
                side.m.meta.tacita_error =
                    Some(format!("the upstream reply broke off: {}", short_reason(e)));
            }
        }
        item
    });
    let mut resp = Response::new(Body::from_stream(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = forward::response_headers(&headers, false);
    resp
}

/// Lives inside the body stream; dropping it ends observation, then the
/// `MetaHandle` field writes the line.
struct Side {
    observer: Observer,
    m: MetaHandle,
}

impl Drop for Side {
    fn drop(&mut self) {
        self.observer.end(&mut self.m.meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_that_appears_after_the_config_check_stops_the_daemon() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("corpus");
        // The config check passed; then the folder became a repo.
        std::fs::create_dir(t.path().join(".git")).unwrap();
        let e = open_recorder(&base).err().expect("must refuse");
        assert!(matches!(
            e.downcast_ref::<config::ConfigError>(),
            Some(config::ConfigError::RecordDirInRepo)
        ));
        assert!(
            std::fs::read_dir(&base).unwrap().next().is_none(),
            "the run directory is removed again"
        );
    }

    #[test]
    fn bounded_cuts_at_a_char_boundary() {
        assert_eq!(bounded("short"), "short");
        let long = "é".repeat(150); // 300 bytes
        let cut = bounded(&long);
        assert_eq!(cut.len(), 200);
        assert_eq!(cut, "é".repeat(100));
        let odd = format!("a{long}");
        assert_eq!(bounded(&odd).len(), 199);
    }

    #[test]
    fn contains_finds_the_probe_and_never_an_empty_one() {
        assert!(contains(b"x TACITA_BLOCK_PROBE y", b"TACITA_BLOCK_PROBE"));
        assert!(!contains(b"x TACITA_BLOCK_PROB", b"TACITA_BLOCK_PROBE"));
        assert!(!contains(b"anything", b""));
        assert!(!contains(b"", b"x"));
    }

    #[test]
    fn default_timing() {
        let t = Timing::default();
        assert_eq!(t.ping_every, Duration::from_secs(10));
        assert_eq!(t.hold_limit, Duration::from_secs(480));
        assert_eq!(t.connect_timeout, Duration::from_secs(10));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn production_upstreams_are_constants() {
        let o = DaemonOptions::production(PathBuf::from("/h"));
        assert_eq!(o.home, PathBuf::from("/h"));
        assert_eq!(o.anthropic_upstream, "https://api.anthropic.com");
        assert_eq!(o.openrouter_upstream, "https://openrouter.ai/api");
        assert_eq!(o.timing, Timing::default());
    }

    #[tokio::test]
    async fn serve_refuses_a_non_loopback_listener() {
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let opts = DaemonOptions {
            home: home.path().to_path_buf(),
            anthropic_upstream: "http://127.0.0.1:1".into(),
            openrouter_upstream: "http://127.0.0.1:1".into(),
            secrets: Arc::new(crate::secrets::MemoryStore::default()),
            timing: Timing::default(),
        };
        let e = serve(opts, listener).await.unwrap_err().to_string();
        assert!(e.contains("127.0.0.1 only"), "{e}");
    }
}
