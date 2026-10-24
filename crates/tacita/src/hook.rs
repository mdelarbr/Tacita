//! `tacita hook pre-tool-use`: a Claude Code PreToolUse hook that denies any
//! tool call still holding a Tacita stand-in or the Step 0 guard probe.
//! It fails closed: a call it cannot check is denied.

use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::config::Config;
use crate::paths;

const MAX_STDIN: u64 = 64 * 1024 * 1024;
const MAX_TOOL_NAME: usize = 64;

pub struct Guard {
    pub standins: Vec<String>,
    pub probe: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

fn cannot_check(error: &str) -> Decision {
    Decision::Deny(format!(
        "Tacita guard hook could not check this call ({error}), so it was denied."
    ))
}

fn tool_name(v: &Value) -> String {
    let Some(name) = v.get("tool_name").and_then(Value::as_str) else {
        return "unknown tool".into();
    };
    let mut end = name.len().min(MAX_TOOL_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// First needle found in any string value or key, at any depth.
fn find<'a>(v: &Value, needles: &'a [&'a str]) -> Option<&'a str> {
    match v {
        Value::String(s) => needles.iter().copied().find(|n| s.contains(n)),
        Value::Array(a) => a.iter().find_map(|x| find(x, needles)),
        Value::Object(m) => m.iter().find_map(|(k, x)| {
            needles
                .iter()
                .copied()
                .find(|n| k.contains(n))
                .or_else(|| find(x, needles))
        }),
        _ => None,
    }
}

pub fn decide(stdin: &str, guard: &Guard) -> Decision {
    let v: Value = match serde_json::from_str(stdin) {
        Ok(v) => v,
        Err(e) => {
            return cannot_check(&format!(
                "stdin is not JSON, line {} column {}",
                e.line(),
                e.column()
            ));
        }
    };
    let Some(input) = v.get("tool_input").filter(|i| i.is_object()) else {
        return cannot_check("stdin has no tool_input object");
    };
    if let Some(p) = guard.probe.as_deref().filter(|p| !p.is_empty())
        && find(input, &[p]).is_some()
    {
        return Decision::Deny(
            "Tacita guard probe: this call was denied on purpose (Step 0 test 4).".into(),
        );
    }
    let standins: Vec<&str> = guard
        .standins
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(s) = find(input, &standins) {
        return Decision::Deny(format!(
            "Tacita blocked this {} call: it still contains the Tacita stand-in `{s}`, which must never reach your files or commands. Write the call again with the real name from the conversation.",
            tool_name(&v)
        ));
    }
    Decision::Allow
}

/// Runs `f`; a panic becomes a deny.
fn guarded(f: impl FnOnce() -> Decision) -> Decision {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| cannot_check("internal error"))
}

fn read_stdin(r: impl Read) -> Result<String, String> {
    let mut buf = Vec::new();
    r.take(MAX_STDIN + 1)
        .read_to_end(&mut buf)
        .map_err(|_| "cannot read stdin".to_string())?;
    if buf.len() as u64 > MAX_STDIN {
        return Err("stdin is larger than 64 MiB".into());
    }
    String::from_utf8(buf).map_err(|_| "stdin is not UTF-8".to_string())
}

fn load_guard() -> Result<Guard, String> {
    // paths::home() would exit 1 (a non-blocking code) when nothing is set.
    let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if !set("TACITA_HOME") && !set("HOME") {
        return Err("neither TACITA_HOME nor HOME is set".into());
    }
    let cfg = Config::load(&paths::home()).map_err(|e| e.to_string())?;
    Ok(Guard {
        standins: cfg.step0.pairs.iter().map(|p| p.standin.clone()).collect(),
        probe: cfg.step0.guard_probe.clone(),
    })
}

/// Writes the outcome; returns the exit code. A deny that cannot be written
/// to stdout goes to stderr with exit 2, which Claude Code treats as a block.
fn emit(decision: &Decision, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Decision::Deny(reason) = decision else {
        return 0;
    };
    let body = serde_json::json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }});
    let written = writeln!(out, "{body}").and_then(|()| out.flush());
    if written.is_ok() {
        return 0;
    }
    let _ = writeln!(err, "{reason}");
    2
}

fn run_with(
    stdin: impl Read,
    guard: impl FnOnce() -> Result<Guard, String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let decision = guarded(|| match read_stdin(stdin) {
        Err(e) => cannot_check(&e),
        Ok(text) => match guard() {
            Err(e) => cannot_check(&e),
            Ok(g) => decide(&text, &g),
        },
    });
    emit(&decision, out, err)
}

/// Backstop: a panic anywhere in the hook process blocks the call (exit 2),
/// with a fixed message that holds no panic payload and no input.
fn install_panic_backstop() {
    std::panic::set_hook(Box::new(|_| {
        let Decision::Deny(reason) = cannot_check("internal error") else {
            unreachable!()
        };
        let _ = writeln!(std::io::stderr(), "{reason}");
        std::process::exit(2);
    }));
}

pub fn run() -> i32 {
    install_panic_backstop();
    run_with(
        std::io::stdin().lock(),
        load_guard,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard {
            standins: vec!["client_x01".into(), "ClientX01".into()],
            probe: Some("TACITA_GUARD_PROBE".into()),
        }
    }

    fn deny(s: &str) -> String {
        match decide(s, &guard()) {
            Decision::Deny(r) => r,
            Decision::Allow => panic!("allowed: {s}"),
        }
    }

    #[test]
    fn allows_clean_calls() {
        for s in [
            r#"{"tool_name":"Bash","tool_input":{"command":"ls -la"}}"#,
            r#"{"tool_name":"Bash","tool_input":{}}"#,
            r#"{"tool_name":"Read","tool_input":{"n":1,"ok":true,"x":null,"a":["fine"]}}"#,
        ] {
            assert_eq!(decide(s, &guard()), Decision::Allow, "{s}");
        }
    }

    #[test]
    fn denies_stand_in_in_value() {
        let r = deny(r#"{"tool_name":"Bash","tool_input":{"command":"cat client_x01/README.md"}}"#);
        assert_eq!(
            r,
            "Tacita blocked this Bash call: it still contains the Tacita stand-in `client_x01`, which must never reach your files or commands. Write the call again with the real name from the conversation."
        );
    }

    #[test]
    fn denies_stand_in_in_key_nested_and_array() {
        assert!(deny(r#"{"tool_name":"X","tool_input":{"ClientX01":1}}"#).contains("`ClientX01`"));
        assert!(
            deny(r#"{"tool_name":"X","tool_input":{"a":{"b":[1,{"c":["x client_x01 y"]}]}}}"#)
                .contains("`client_x01`")
        );
        assert!(
            deny(r#"{"tool_name":"X","tool_input":{"a":[{"client_x01":0}]}}"#).contains("blocked")
        );
    }

    #[test]
    fn denies_probe_first() {
        let r = deny(r#"{"tool_name":"Bash","tool_input":{"c":"client_x01 TACITA_GUARD_PROBE"}}"#);
        assert_eq!(
            r,
            "Tacita guard probe: this call was denied on purpose (Step 0 test 4)."
        );
        assert!(
            deny(r#"{"tool_name":"Bash","tool_input":{"TACITA_GUARD_PROBE":1}}"#).contains("probe")
        );
    }

    #[test]
    fn denies_on_garbage_input() {
        for s in [
            "",
            "not json",
            "[]",
            r#"{"tool_name":"Bash"}"#,
            r#"{"tool_name":"Bash","tool_input":"client"}"#,
            r#"{"tool_name":"Bash","tool_input":null}"#,
        ] {
            let r = deny(s);
            assert!(
                r.starts_with("Tacita guard hook could not check this call ("),
                "{r}"
            );
            assert!(r.ends_with("), so it was denied."), "{r}");
            assert!(!r.contains("not json"), "{r}");
        }
    }

    #[test]
    fn tool_name_is_cut_or_defaulted() {
        let long = "é".repeat(100);
        let r = deny(&format!(
            r#"{{"tool_name":"{long}","tool_input":{{"a":"client_x01"}}}}"#
        ));
        let name = r
            .strip_prefix("Tacita blocked this ")
            .and_then(|r| r.split(" call:").next())
            .unwrap();
        assert!(name.len() <= 64 && name.chars().all(|c| c == 'é'), "{name}");
        assert!(
            deny(r#"{"tool_name":5,"tool_input":{"a":"client_x01"}}"#)
                .contains("this unknown tool call")
        );
    }

    #[test]
    fn reason_never_echoes_the_input() {
        let r = deny(r#"{"tool_name":"Bash","tool_input":{"command":"SECRETVALUE client_x01"}}"#);
        assert!(!r.contains("SECRETVALUE"));
    }

    #[test]
    fn oversized_stdin_is_denied() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                b.fill(b' ');
                Ok(b.len())
            }
        }
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let code = run_with(Endless, || Ok(guard()), &mut o, &mut e);
        assert_eq!(code, 0);
        assert!(String::from_utf8(o).unwrap().contains("larger than 64 MiB"));
    }

    #[test]
    fn panic_becomes_deny() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let code = run_with(
            "{}".as_bytes(),
            || -> Result<Guard, String> { std::panic::resume_unwind(Box::new("boom")) },
            &mut o,
            &mut e,
        );
        assert_eq!(code, 0);
        let s = String::from_utf8(o).unwrap();
        assert!(
            s.contains(r#""permissionDecision":"deny""#) && !s.contains("boom"),
            "{s}"
        );
    }

    #[test]
    fn stdout_failure_exits_2_with_reason_on_stderr() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut e = Vec::new();
        let code = run_with("nope".as_bytes(), || Ok(guard()), &mut Broken, &mut e);
        assert_eq!(code, 2);
        assert!(String::from_utf8(e).unwrap().contains("could not check"));
        // An allow writes nothing, so a broken stdout is irrelevant.
        let ok = r#"{"tool_name":"B","tool_input":{}}"#;
        assert_eq!(
            run_with(ok.as_bytes(), || Ok(guard()), &mut Broken, &mut Vec::new()),
            0
        );
    }

    #[test]
    fn output_is_valid_json_with_quotes_in_reason() {
        let mut o = Vec::new();
        let code = emit(
            &Decision::Deny("a \"q\" `b`".into()),
            &mut o,
            &mut Vec::new(),
        );
        assert_eq!(code, 0);
        let v: Value = serde_json::from_slice(&o).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "a \"q\" `b`"
        );
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    }
}
