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
