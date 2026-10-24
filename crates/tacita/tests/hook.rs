use std::io::Write;
use std::process::{Command, Output, Stdio};

fn hook(home: Option<&std::path::Path>, stdin: &str) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tacita"));
    c.args(["hook", "pre-tool-use"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match home {
        Some(h) => c.env("TACITA_HOME", h),
        None => c.env("TACITA_HOME", "").env_remove("HOME"),
    };
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn init() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tacita"))
        .arg("init")
        .env("TACITA_HOME", home.path())
        .output()
        .unwrap();
    assert!(o.status.success());
    home
}

fn assert_deny(o: &Output, needle: &str) {
    assert_eq!(o.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason.contains(needle), "{reason}");
}

#[test]
fn denies_stand_in_and_probe_allows_clean() {
    let home = init();
    let p = Some(home.path());
    assert_deny(
        &hook(
            p,
            r#"{"tool_name":"Bash","tool_input":{"command":"cat client_x01/README.md"}}"#,
        ),
        "`client_x01`",
    );
    assert_deny(
        &hook(
            p,
            r#"{"tool_name":"Bash","tool_input":{"command":"echo TACITA_GUARD_PROBE"}}"#,
        ),
        "guard probe",
    );
    let ok = hook(p, r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#);
    assert_eq!(ok.status.code(), Some(0));
    assert!(ok.stdout.is_empty());
}

#[test]
fn denies_garbage_stdin() {
    let home = init();
    assert_deny(&hook(Some(home.path()), "nope"), "could not check");
}

#[test]
fn denies_when_config_is_missing_or_home_unset() {
    let empty = tempfile::tempdir().unwrap();
    let clean = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    assert_deny(&hook(Some(empty.path()), clean), "could not check");
    assert_deny(&hook(None, clean), "neither TACITA_HOME nor HOME");
}

#[test]
fn denies_when_config_is_invalid() {
    let home = init();
    std::fs::write(home.path().join("config.toml"), "garbage = [").unwrap();
    let o = hook(Some(home.path()), r#"{"tool_name":"Bash","tool_input":{}}"#);
    assert_deny(&o, "could not check");
}

#[test]
fn other_commands_still_work_without_the_hook_path() {
    let home = init();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tacita"))
            .args(args)
            .env("TACITA_HOME", home.path())
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    let st = run(&["status"]);
    assert!(st.status.success() && String::from_utf8_lossy(&st.stdout).contains("daemon:"));
    assert!(!run(&["use", "openrouter"]).status.success());
}
