use std::process::Output;

fn run_in(home: &std::path::Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tacita"))
        .args(args)
        .env("TACITA_HOME", home)
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn err(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

#[test]
fn init_status_use() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| run_in(home.path(), args);
    let init = String::from_utf8(run(&["init"]).stdout).unwrap();
    assert!(init.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:47821/"));
    let url = String::from_utf8(run(&["status", "--base-url"]).stdout).unwrap();
    assert!(
        url.trim().starts_with("http://127.0.0.1:47821/")
            && url.trim().len() == "http://127.0.0.1:47821/".len() + 32
    );
    let used = String::from_utf8(run(&["use", "openrouter", "--model", "x/y"]).stdout).unwrap();
    assert!(used.contains("provider:   openrouter (model x/y)"));
    assert!(!run(&["use", "openrouter"]).status.success());
}

#[test]
fn init_output_and_idempotence() {
    let home = tempfile::tempdir().unwrap();
    let first = out(&run_in(home.path(), &["init"]));
    let cfg = home.path().join("config.toml");
    assert!(
        first.starts_with(&format!("Created {}\n", cfg.display())),
        "{first}"
    );
    assert!(
        first.contains("For one Claude Code session:  ANTHROPIC_BASE_URL=http://127.0.0.1:47821/")
    );
    let before = std::fs::read(&cfg).unwrap();
    let second = out(&run_in(home.path(), &["init"]));
    assert!(
        second.starts_with(&format!("Already initialised: {}\n", cfg.display())),
        "{second}"
    );
    assert_eq!(std::fs::read(&cfg).unwrap(), before);
}

#[test]
fn init_refuses_invalid_existing_config() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "port = 1\n").unwrap();
    let o = run_in(home.path(), &["init"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(!out(&o).contains("Already initialised"));
}

#[test]
fn status_lines_and_use_anthropic() {
    let home = tempfile::tempdir().unwrap();
    run_in(home.path(), &["init"]);
    let s = out(&run_in(home.path(), &["status"]));
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], "provider:   anthropic");
    assert_eq!(lines[1], "protection: off");
    assert!(lines[2].starts_with("base URL:   http://127.0.0.1:47821/"));
    assert_eq!(lines[3], "daemon:     not running");
    assert_eq!(lines.len(), 4);

    let s = out(&run_in(
        home.path(),
        &["use", "openrouter", "--model", "x/y"],
    ));
    assert!(s.contains("protection: pairs"), "{s}");
    let s = out(&run_in(home.path(), &["use", "anthropic"]));
    assert!(s.contains("provider:   anthropic\n"), "{s}");
    let state = std::fs::read_to_string(home.path().join("state.toml")).unwrap();
    assert!(!state.contains("x/y"));
}

#[test]
fn bad_models_are_refused() {
    let home = tempfile::tempdir().unwrap();
    run_in(home.path(), &["init"]);
    for m in ["", "a b", "a\tb", "a\nb"] {
        let o = run_in(home.path(), &["use", "openrouter", "--model", m]);
        assert_eq!(o.status.code(), Some(1), "{m:?}");
        assert!(!home.path().join("state.toml").exists());
    }
}

#[test]
fn status_and_use_without_config() {
    let home = tempfile::tempdir().unwrap();
    for args in [&["status"][..], &["use", "anthropic"][..]] {
        let o = run_in(home.path(), args);
        assert_eq!(o.status.code(), Some(1));
        assert!(
            err(&o).contains("not initialised: run `tacita init`"),
            "{}",
            err(&o)
        );
        assert!(out(&o).is_empty());
    }
}

#[test]
fn status_detects_a_running_daemon_only_on_200() {
    use std::io::{Read, Write};
    let home = tempfile::tempdir().unwrap();
    run_in(home.path(), &["init"]);
    let cfg = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        home.path().join("config.toml"),
        cfg.replace("47821", &port.to_string()),
    )
    .unwrap();
    let secret = out(&run_in(home.path(), &["status", "--base-url"]));
    let secret = secret.trim().rsplit('/').next().unwrap().to_string();
    let server = std::thread::spawn(move || {
        for code in ["200 OK", "404 Not Found"] {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            s.write_all(
                format!("HTTP/1.1 {code}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            assert!(
                req.starts_with(&format!("HEAD /{secret}/api/hello ")),
                "{req}"
            );
        }
    });
    assert!(out(&run_in(home.path(), &["status"])).contains("daemon:     running"));
    assert!(out(&run_in(home.path(), &["status"])).contains("daemon:     not running"));
    server.join().unwrap();
}

fn run_with_proxies(home: &std::path::Path, proxy: &str, args: &[&str]) -> Output {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_tacita"));
    c.args(args).env("TACITA_HOME", home);
    for k in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "ALL_PROXY"] {
        c.env(k, proxy);
    }
    c.output().unwrap()
}

fn init_on_port(home: &std::path::Path, port: u16) {
    run_in(home, &["init"]);
    let cfg = std::fs::read_to_string(home.join("config.toml")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        cfg.replace("47821", &port.to_string()),
    )
    .unwrap();
}

fn spy() -> (std::net::TcpListener, u16) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let p = l.local_addr().unwrap().port();
    (l, p)
}

fn untouched(l: &std::net::TcpListener) -> bool {
    std::thread::sleep(std::time::Duration::from_millis(200));
    matches!(l.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

#[test]
fn status_never_uses_a_proxy() {
    use std::io::{Read, Write};
    let home = tempfile::tempdir().unwrap();
    let (proxy, proxy_port) = spy();
    let daemon = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    init_on_port(home.path(), daemon.local_addr().unwrap().port());
    let t = std::thread::spawn(move || {
        let (mut s, _) = daemon.accept().unwrap();
        let mut b = [0u8; 1024];
        let _ = s.read(&mut b).unwrap();
        s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .unwrap();
    });
    let o = run_with_proxies(
        home.path(),
        &format!("http://127.0.0.1:{proxy_port}"),
        &["status"],
    );
    assert!(out(&o).contains("daemon:     running"), "{}", out(&o));
    t.join().unwrap();
    assert!(untouched(&proxy), "proxy was contacted");
}

#[test]
fn status_does_not_follow_redirects() {
    use std::io::{Read, Write};
    let home = tempfile::tempdir().unwrap();
    let (target, target_port) = spy();
    let daemon = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    init_on_port(home.path(), daemon.local_addr().unwrap().port());
    let t = std::thread::spawn(move || {
        let (mut s, _) = daemon.accept().unwrap();
        let mut b = [0u8; 1024];
        let _ = s.read(&mut b).unwrap();
        let r = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{target_port}/x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        s.write_all(r.as_bytes()).unwrap();
    });
    let o = run_in(home.path(), &["status"]);
    assert!(out(&o).contains("daemon:     not running"), "{}", out(&o));
    t.join().unwrap();
    assert!(untouched(&target), "redirect was followed");
}

#[test]
fn daemon_refuses_a_port_in_use() {
    let home = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    init_on_port(home.path(), port);
    let o = run_in(home.path(), &["daemon"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o),
        format!("tacita: port {port} is in use (is another tacita daemon running?)\n")
    );
    assert_eq!(out(&o), "");
}

/// Kills the child if the test fails before it exits.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn daemon_listens_answers_hello_and_stops_on_ctrl_c() {
    use std::io::Read;
    let home = tempfile::tempdir().unwrap();
    init_on_port(home.path(), free_port());
    let base = out(&run_in(home.path(), &["status", "--base-url"]));
    let (mut d, mut stdout, banner) = spawn_daemon(home.path(), std::process::Stdio::piped());
    assert_eq!(banner, format!("Tacita listening on {base}"));
    assert!(out(&run_in(home.path(), &["status"])).contains("daemon:     running"));
    let status = interrupt(&mut d);
    assert!(status.success(), "{status:?}");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "Tacita stopped\n");
    let mut stderr = String::new();
    d.0.stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("/api/hello"), "{stderr}");
    assert!(
        !stderr.contains(base.trim().rsplit('/').next().unwrap()),
        "{stderr}"
    );
    let log = std::fs::read_to_string(home.path().join("logs/requests.jsonl")).unwrap();
    assert!(log.contains(r#""route":"hello""#), "{log}");
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Spawns `tacita daemon` and returns it with its stdout reader and the
/// first stdout line.
fn spawn_daemon(
    home: &std::path::Path,
    stderr: std::process::Stdio,
) -> (
    Daemon,
    std::io::BufReader<std::process::ChildStdout>,
    String,
) {
    use std::io::BufRead;
    let mut d = Daemon(
        std::process::Command::new(env!("CARGO_BIN_EXE_tacita"))
            .arg("daemon")
            .env("TACITA_HOME", home)
            .stdout(std::process::Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let mut stdout = std::io::BufReader::new(d.0.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    (d, stdout, line)
}

/// Sends SIGINT and waits up to 5 s for the exit.
fn interrupt(d: &mut Daemon) -> std::process::ExitStatus {
    use std::time::{Duration, Instant};
    let killed = std::process::Command::new("kill")
        .args(["-INT", &d.0.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = d.0.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "daemon did not stop on SIGINT");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn daemon_survives_a_closed_stderr() {
    // Rust ignores SIGPIPE, so a write to a closed stderr fails with EPIPE,
    // which `eprintln!` would turn into a panic inside a request.
    use std::io::Read;
    let home = tempfile::tempdir().unwrap();
    init_on_port(home.path(), free_port());
    let base = out(&run_in(home.path(), &["status", "--base-url"]));
    let (mut d, mut stdout, banner) = spawn_daemon(home.path(), std::process::Stdio::piped());
    drop(d.0.stderr.take());
    assert_eq!(banner, format!("Tacita listening on {base}"));
    for _ in 0..2 {
        assert!(out(&run_in(home.path(), &["status"])).contains("daemon:     running"));
    }
    assert!(interrupt(&mut d).success());
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "Tacita stopped\n");
    let log = std::fs::read_to_string(home.path().join("logs/requests.jsonl")).unwrap();
    assert_eq!(log.matches(r#""route":"hello""#).count(), 2, "{log}");
}

#[test]
fn daemon_prints_no_banner_when_startup_fails() {
    let home = tempfile::tempdir().unwrap();
    init_on_port(home.path(), free_port());
    // `logs` is a file, so the request log cannot be opened.
    std::fs::write(home.path().join("logs"), b"not a directory").unwrap();
    let o = run_in(home.path(), &["daemon"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(out(&o), "");
    assert!(
        err(&o).starts_with("tacita: cannot open logs/requests.jsonl: "),
        "{}",
        err(&o)
    );
}

#[test]
fn key_set_accepts_only_openrouter() {
    let home = tempfile::tempdir().unwrap();
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_tacita"))
        .env("TACITA_HOME", home.path())
        .args(["key", "set", "anthropic"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("input hidden"));
}
