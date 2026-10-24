# Tacita Step 0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first working slice of Tacita, so the owner can run the five Step 0 live tests (spec §15) before anything else is built on assumptions. The slice is a local Rust proxy that:
- passes Claude Code's Max traffic through, either unchanged or scrubbed with a small fixed test dictionary of stand-ins;
- can route to OpenRouter;
- records a traffic corpus;
- ships a guard hook.

**Architecture:** A Cargo workspace with two crates.
- `tacita-core` is pure, synchronous logic with no I/O:
  - a byte-exact JSON string rewriter;
  - the Step 0 dictionary ("pairs");
  - the request scrub policy;
  - an incremental SSE parser;
  - the streaming restorer;
  - the OpenRouter request adapter.
- `tacita` is the binary:
  - config and state files, and the Keychain;
  - the axum daemon (`tacita daemon`, which is the spec's `tacitad`);
  - the metadata log and the corpus recorder;
  - the guard hook and the CLI.

The pairs dictionary is a deliberately small stand-in for the protection file. Plan 2 replaces it behind the same per-provider `Protection` switch, which is also where the spec's "credentials-only on Max" fallback will live.

**Tech Stack:** Rust stable (edition 2024), tokio, axum 0.8, reqwest 0.13 (native-tls, with no compression, no redirects and no proxies), serde_json (`preserve_order`, `arbitrary_precision`), aho-corasick, toml, clap, security-framework, rpassword. Tests use proptest and tempfile.

**Spec:** `docs/superpowers/specs/2026-10-24-tacita-design.md`. §15 drives this plan, and §8–§11 give the rules it must already respect. Research notes that executors also read:
- `docs/research/2026-10-24-claude-max-through-tacita.md` §3 (body-modification constraints) and §4 (the live test);
- `docs/research/2026-10-24-prior-art.md` §6.3 (rules R1–R22), §6.5 (tests T1–T26) and §7 (local-access rules).

**Scope:** This is plan 1 of a series. It builds only what Step 0 needs, but to production quality, because later plans sit on it. The later plans are written after Step 0's results; see "After this plan" at the end.

**Branch:** `step0`, cut from `design/tacita-v1`.

## Global Constraints

- **Language and platform:** Rust stable, edition 2024. Builds and runs on Intel x86_64 macOS 15.6.1, and must also work on Apple Silicon.
- **Claude Code settings:** only `ANTHROPIC_BASE_URL` is ever set. Never set `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper` or `CLAUDE_CODE_OAUTH_TOKEN`, because each one switches the Max login off. In this plan the variable is set per session on the command line; `~/.claude/settings.json` is not touched.
- **The Max bearer:** Claude Code's `Authorization` bearer and all `anthropic-*` / `x-claude-code-*` headers are forwarded unchanged to `https://api.anthropic.com` only.
  - Tacita never stores, logs or reuses that token, and never sends it to any other host.
  - Tacita redacts `sk-ant-` from its own logs.
  - The Anthropic upstream URL is a code constant, not a config value.
- **Never modified:** `system[0]`, `system[1]`, thinking and redacted-thinking blocks, signatures, tool names, `model`, `metadata`.
  - Toward Anthropic, never add, remove or reorder fields or blocks. Rewrite string values in place only.
  - "Do no Unicode or apostrophe normalization anywhere."
- **Upstream errors:** error bodies, `retry-after`, `x-should-retry` and every `anthropic-ratelimit-unified-*` header pass through unchanged.
- **Streaming:**
  - never buffer a whole streamed response;
  - forward `ping` events;
  - keep any held tool block bounded, because Claude Code's stall watchdog fires after about 300 s.
- **Paths:**
  - `POST /v1/messages/count_tokens` gets a 404 and is never forwarded;
  - `HEAD /api/hello` is answered locally;
  - every other path is refused and logged.
- **Fail closed, visibly:** any failure produces a visible error, never a silent pass-through. Tacita's own errors use the Anthropic error shape `{"type":"error","error":{"type":"invalid_request_error","message":"Tacita: …"}}`.
- **Local access:**
  - bind `127.0.0.1` only;
  - use a random secret path prefix;
  - check the Host header;
  - reject `Origin`, `Referer` and `Sec-Fetch-*`;
  - send no CORS headers.
- **Tacita's folder:** `~/Library/Application Support/Tacita/`, overridable with `TACITA_HOME` (used by tests). Directories are 0700 and files 0600.
- **Credentials:** Tacita never writes a credential value to disk. The OpenRouter key lives only in the macOS Keychain. The one deliberate exception is the opt-in Step 0 corpus recorder (Task 13).
- **OpenRouter requests:**
  - drop the Anthropic bearer and the OAuth beta value;
  - inject the OpenRouter key;
  - map `claude-*` model IDs to the chosen slug; unknown model IDs fail closed;
  - strip thinking blocks and drop `metadata.user_id`;
  - send the privacy preferences: no data collection, zero data retention (ZDR), no fallbacks, optional `only`.

  OpenRouter is never reached without protection.
- **Step 0 logging:** safe metadata only, namely header names, booleans, the first 60 characters of `system[1]`, the status, rate-limit headers and usage. Never bodies and never credential values. Bodies are written only by the opt-in corpus recorder, outside any git repo.
- **Dependencies:** small and readable. No AGPL code and no unlicensed code. reqwest is used without compression, redirects or proxies.
- **Never commit** the corpus, a config holding a real secret, or any key.

## Review Focus

1. **A long Write or Edit tool call streams for minutes while Tacita holds it.** Expected: Claude Code doesn't time out, because Tacita sends its own pings every 10 s. Past 480 s, Tacita ends the stream with a visible error. Pinned by Task 11: `pings_sent_while_holding`, `hold_limit_fails_closed`.
2. **The upstream stream breaks mid-response while text or a tool block is held.** Expected: the client gets the text that already arrived; a half-received tool call is never emitted; the daemon keeps serving. Pinned by Task 7 `finish_drops_held_tool_block` and Task 11 `stream_cut_mid_tool_drops_the_tool`.
3. **Several requests run at once** (subagents, background Haiku calls, two Claude Code windows). Expected: each stream is restored independently, with no mixing. Pinned by Task 11: `concurrent_sessions_restore_independently`.
4. **Odd JSON strings:** an escape inside a name (`Acme\u0057idgets`), lone surrogates from binary output, or real names containing quotes or backslashes. Expected: names are still found and replaced, every other byte stays identical, and the JSON stays valid. Pinned by Task 2: `match_spanning_an_escape_is_replaced_whole`, `lone_surrogate_survives_byte_exact`, `replace_escapes_real_values`.
5. **Claude Code calls paths Tacita doesn't know yet**, such as `?beta=true`, `/v1/models`, or a new endpoint after an update. Expected: the query string is forwarded intact, unknown paths are refused, and both show in the log so Step 0 reveals them. Pinned by Task 10: `forwards_auth_and_beta_headers_unchanged`, `wrong_secret_and_unknown_paths_are_refused`.

---

## File Structure

```
Cargo.toml                                  workspace + pinned dependency versions
rust-toolchain.toml                         stable + clippy + rustfmt
crates/tacita-core/src/lib.rs               module list
crates/tacita-core/src/json/mod.rs          re-exports
crates/tacita-core/src/json/string.rs       decode (with offset map), encode, replace inside one literal
crates/tacita-core/src/json/scan.rs         string-literal scanner with paths (strict + tolerant), splice
crates/tacita-core/src/pairs.rs             Step 0 dictionary: real <-> stand-in, validation, hold_from
crates/tacita-core/src/request.rs           request scrub policy (what may and may not be rewritten)
crates/tacita-core/src/sse.rs               incremental SSE parser, event builder
crates/tacita-core/src/restore.rs           streaming restorer + non-streaming restore
crates/tacita-core/src/errors.rs            Tacita error body / error event
crates/tacita-core/src/openrouter.rs        OpenRouter request + beta-header adaptation
crates/tacita-core/tests/fixtures/claude_code_request.json
crates/tacita/src/main.rs                   clap CLI dispatch
crates/tacita/src/lib.rs                    module list
crates/tacita/src/paths.rs                  TACITA_HOME, private dirs/files
crates/tacita/src/config.rs                 config.toml + state.toml
crates/tacita/src/secrets.rs                SecretStore trait, MemoryStore, KeychainStore
crates/tacita/src/daemon/mod.rs             DaemonOptions, Timing, serve(), request handler
crates/tacita/src/daemon/route.rs           pure routing + local-access checks
crates/tacita/src/daemon/forward.rs         upstream headers, client, response header filtering
crates/tacita/src/daemon/stream.rs          restoring response stream (pings, hold limit)
crates/tacita/src/daemon/metalog.rs         RequestMeta, observe(), redact(), MetaLog
crates/tacita/src/daemon/record.rs          opt-in corpus recorder
crates/tacita/src/hook.rs                   guard hook (PreToolUse)
crates/tacita/tests/support/mod.rs          FakeUpstream, start_tacita, sse()
crates/tacita/tests/{cli,proxy_passthrough,proxy_scrub,proxy_openrouter,proxy_record,hook}.rs
docs/step0/runbook.md, docs/step0/results.md, docs/step0/make-throwaway-repo.sh
```

---

### Task 1: Toolchain and workspace

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `crates/tacita-core/Cargo.toml`, `crates/tacita-core/src/lib.rs`, `crates/tacita/Cargo.toml`, `crates/tacita/src/lib.rs`, `crates/tacita/src/main.rs`
- Modify: `.gitignore` (add `/target`)

**Interfaces:**
- Produces: workspace dependency versions used by every later task.

- [ ] **Step 1: Check the linker.** Run: `xcode-select -p`. Expected: a path such as `/Library/Developer/CommandLineTools`. If it errors, run `xcode-select --install` and wait for it to finish.
- [ ] **Step 2: Install Rust.** Run: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable -c clippy,rustfmt && . "$HOME/.cargo/env" && rustc --version`. Expected: `rustc 1.<n>.<m> (…)`.
- [ ] **Step 3: Write the workspace.** Root `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/tacita-core", "crates/tacita"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"

[workspace.dependencies]
aho-corasick = "1.1.5"
anyhow = "1.0.104"
axum = "0.8.9"
bytes = "1.12.1"
clap = { version = "4.6.7", features = ["derive"] }
futures-util = "0.3.34"
proptest = "1.12.0"
reqwest = { version = "0.13.5", default-features = false, features = ["native-tls", "http2", "stream"] }
rpassword = "7.5.4"
security-framework = "3.7.0"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = { version = "1.0.154", features = ["preserve_order", "arbitrary_precision"] }
tempfile = "3.27.0"
thiserror = "2.0.21"
tokio = { version = "1.54.1", features = ["rt-multi-thread", "macros", "net", "time", "io-util", "io-std", "fs", "signal", "sync"] }
toml = "1.1.8"
```

  - `rust-toolchain.toml` sets `channel = "stable"` and `components = ["clippy", "rustfmt"]`.
  - `tacita-core` depends on aho-corasick, serde, serde_json and thiserror, with proptest as a dev-dependency.
  - `tacita` depends on tacita-core (by path), anyhow, axum, bytes, clap, futures-util, reqwest, rpassword, serde, serde_json, thiserror, tokio and toml, with tempfile as a dev-dependency. `security-framework` goes under `[target.'cfg(target_os = "macos")'.dependencies]`.
  - Both `lib.rs` files start empty, and `main.rs` is `fn main() {}`.
- [ ] **Step 4: Verify.** Run: `cargo build -q && cargo test -q 2>&1 | tail -2`. Expected: `test result: ok. 0 passed`.
- [ ] **Step 5: Commit.** `git add Cargo.toml Cargo.lock rust-toolchain.toml crates .gitignore && git commit -m "build: Rust workspace for tacita and tacita-core"`

---

### Task 2: JSON string literals (decode with an offset map, encode, replace)

**Files:**
- Create: `crates/tacita-core/src/json/mod.rs`, `crates/tacita-core/src/json/string.rs`
- Modify: `crates/tacita-core/src/lib.rs` (add `pub mod json;`)

**Interfaces:**
- Produces (`tacita_core::json`):
  - `pub struct Decoded { pub text: String, /* private offset map */ }`
  - `pub fn decode(lit: &[u8]) -> Result<Decoded, StrError>`. Here `lit` is one JSON string literal, quotes included.
  - `pub fn encode(s: &str) -> String`. Returns a literal with its quotes. It escapes `"`, `\` and U+0000–U+001F (`\b \f \n \r \t`, others as `\u00xx`). Everything else, including `/`, U+2028 and U+2029, is written as raw UTF-8.
  - `pub fn replace(lit: &[u8], decoded: &Decoded, edits: &[(Range<usize>, &str)]) -> Vec<u8>`. Each edit is a byte range in `decoded.text`; the edits are sorted, don't overlap, and sit on char boundaries. Each replacement is written escaped, and every raw byte outside the edited ranges is copied unchanged.
  - `pub enum StrError { NotAString, Unterminated, BadEscape(usize), RawControl(usize), BadUtf8 }`, derived with thiserror.

The offset map records the raw offset where the source of each decoded char starts, plus the closing quote's offset. So a decoded range `a..b` covers the raw bytes `map[a]..map[b]`. A surrogate-pair escape decodes to one char. A lone surrogate decodes to U+FFFD and its raw bytes are kept, because they sit outside any edit.

- [ ] **Step 1: Write the failing tests** in `string.rs` under `#[cfg(test)]`:

```rust
#[test]
fn decodes_escapes() {
    let d = decode(br#""a\u00e9\"b\uD83D\uDE00c""#).unwrap();
    assert_eq!(d.text, "aé\"b😀c");
}
#[test]
fn replace_keeps_untouched_escapes_byte_exact() {
    let lit = br#""x \u00e9 acmewidgets \/ y""#;
    let d = decode(lit).unwrap();
    let at = d.text.find("acmewidgets").unwrap();
    assert_eq!(replace(lit, &d, &[(at..at + 11, "client_x01")]), br#""x \u00e9 client_x01 \/ y""#.to_vec());
}
#[test]
fn replace_escapes_real_values() {
    let lit = br#""see client_x01""#;
    let d = decode(lit).unwrap();
    assert_eq!(replace(lit, &d, &[(4..14, "L'\"Oréal\\\n")]), "\"see L'\\\"Oréal\\\\\\n\"".as_bytes());
}
#[test]
fn match_spanning_an_escape_is_replaced_whole() {
    let lit = br#""Acme\u0057idgets""#;
    let d = decode(lit).unwrap();
    assert_eq!(d.text, "AcmeWidgets");
    assert_eq!(replace(lit, &d, &[(0..11, "ClientX01")]), br#""ClientX01""#.to_vec());
}
#[test]
fn lone_surrogate_survives_byte_exact() {
    let lit = br#""\uD800 acmewidgets""#;
    let d = decode(lit).unwrap();
    assert!(d.text.starts_with('\u{FFFD}'));
    let at = d.text.find("acmewidgets").unwrap();
    assert_eq!(replace(lit, &d, &[(at..at + 11, "client_x01")]), br#""\uD800 client_x01""#.to_vec());
}
#[test]
fn rejects_raw_control_and_bad_escape() {
    assert!(matches!(decode(b"\"a\nb\""), Err(StrError::RawControl(_))));
    assert!(matches!(decode(br#""\q""#), Err(StrError::BadEscape(_))));
}
proptest::proptest! {
    #[test]
    fn encode_round_trips(s in ".*") {
        let lit = encode(&s);
        proptest::prop_assert_eq!(serde_json::from_str::<String>(&lit).unwrap(), s.clone());
        proptest::prop_assert_eq!(decode(lit.as_bytes()).unwrap().text, s);
    }
}
```

- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib json::string -q 2>&1 | tail -3`. Expected: a compile error, unresolved `decode`.
- [ ] **Step 3: Implement** `decode`, `encode` and `replace` as specified above. Write a hand-written byte loop with no regex (R1).
- [ ] **Step 4: Run them and check they pass.** Same command. Expected: `test result: ok. 7 passed`.
- [ ] **Step 5: Commit.** `git commit -am "core: byte-exact JSON string literal codec"`. Add the new files first with `git add crates/tacita-core/src/json`.

---

### Task 3: JSON scanner with paths, and splice

**Files:**
- Create: `crates/tacita-core/src/json/scan.rs`
- Modify: `crates/tacita-core/src/json/mod.rs`

**Interfaces:**
- Consumes: `decode` from Task 2, used to fill `Seg::Key`.
- Produces (`tacita_core::json`):
  - `#[derive(Clone, Debug, PartialEq, Eq)] pub enum Seg { Key(String), Index(usize) }`
  - `#[derive(Clone, Debug)] pub struct Lit { pub span: Range<usize>, pub path: Vec<Seg>, pub is_key: bool }`
  - `pub fn scan(json: &[u8]) -> Result<Vec<Lit>, ScanError>`. Strict: the input must be exactly one RFC 8259 value plus whitespace.
  - `pub fn scan_tolerant(json: &[u8]) -> Vec<Lit>`. Returns every complete literal before the first error or the end of input. It never panics.
  - `pub fn splice(src: &[u8], edits: Vec<(Range<usize>, Vec<u8>)>) -> Vec<u8>`. Edits are sorted and don't overlap.
  - `#[derive(Debug, thiserror::Error)] #[error("invalid JSON at byte {offset}: {reason}")] pub struct ScanError { pub offset: usize, pub reason: &'static str }`

Path semantics:
- A value literal's `path` runs from the root to that value.
- A key literal's `path` is the path of the object that holds it, followed by `Seg::Key(<that key>)`, with `is_key = true`.

- [ ] **Step 1: Write the failing tests:**

```rust
fn k(s: &str) -> Seg { Seg::Key(s.into()) }
#[test]
fn paths_for_values_and_keys() {
    let j = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
    let lits = scan(j).unwrap();
    let text = lits.iter().find(|l| !l.is_key && &j[l.span.clone()] == br#""hi""#).unwrap();
    assert_eq!(text.path, vec![k("messages"), Seg::Index(0), k("content"), Seg::Index(0), k("text")]);
    let role = lits.iter().find(|l| l.is_key && &j[l.span.clone()] == br#""role""#).unwrap();
    assert_eq!(role.path, vec![k("messages"), Seg::Index(0), k("role")]);
}
#[test]
fn strict_rejects_trailing_garbage_and_truncation() {
    assert!(scan(br#"{"a":1} x"#).is_err());
    assert!(scan(br#"{"a":"b"#).is_err());
}
#[test]
fn tolerant_keeps_complete_literals_of_truncated_input() {
    let lits = scan_tolerant(br#"{"file_path":"/x/client_x01/a.md","content":"unfinish"#);
    assert_eq!(lits.len(), 3);
}
#[test]
fn splice_leaves_other_bytes_identical() {
    let src = br#"{ "n": 1.0, "big": 12345678901234567890, "e": 1e3, "s": "acme" }"#;
    let s = scan(src).unwrap().into_iter().find(|l| &src[l.span.clone()] == br#""acme""#).unwrap();
    let out = splice(src, vec![(s.span, br#""client_x01""#.to_vec())]);
    assert_eq!(out, br#"{ "n": 1.0, "big": 12345678901234567890, "e": 1e3, "s": "client_x01" }"#.to_vec());
}
```

  Add the property test `scan_agrees_with_serde`. It generates an arbitrary `serde_json::Value` with `prop_recursive(4, 64, 8, …)` over null, bool, i64 and `".*"` strings, arrays (`vec(inner, 0..8)`) and objects (`btree_map(".*", inner, 0..8)`). It serializes the value with `serde_json::to_vec`, then asserts two things:
  - every value literal's decoded text equals the string found by following its `path` in the value;
  - the number of value literals equals the number of strings in the value.

- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib json::scan -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement.** Write a recursive-descent scanner over bytes that keeps a path stack. String literals are delimited with the same escape rules as Task 2. Numbers, `true`, `false` and `null` are validated but not recorded.
- [ ] **Step 4: Run them and check they pass.** Same command. Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita-core && git commit -m "core: JSON literal scanner with paths and splice"`

---

### Task 4: The Step 0 dictionary (`Pairs`)

**Files:**
- Create: `crates/tacita-core/src/pairs.rs`
- Modify: `crates/tacita-core/src/lib.rs`

**Interfaces:**
- Produces (`tacita_core::pairs`):
  - `#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)] pub struct Pair { pub real: String, pub standin: String }`
  - `pub struct Pairs`, with these methods:
    - `new(pairs: Vec<Pair>) -> Result<Pairs, PairsError>`
    - `find_reals(&self, text: &str) -> Vec<(Range<usize>, &str)>`: each item is a range in `text` plus the matching stand-in.
    - `find_standins(&self, text: &str) -> Vec<(Range<usize>, &str)>`: each item is a range plus the matching real value.
    - `hold_from(&self, text: &str) -> usize`: the smallest `i` such that `text[i..]` is a non-empty proper prefix of a stand-in, or `text.len()` if there is none.
    - `scrub_str(&self, text: &str) -> (String, usize)` and `restore_str(&self, text: &str) -> (String, usize)`
    - `standins(&self) -> impl Iterator<Item = &str>`
  - `#[cfg(test)] pub(crate) fn test_pairs() -> Pairs`. It sits outside the tests module, so Tasks 5 and 7 can use it.
  - `#[derive(Debug, thiserror::Error)] pub enum PairsError { Empty, EmptyValue, BadStandin(String), DuplicateReal(String), DuplicateStandin(String), Nested(String, String), Overlap(String, String), Crossed(String, String), ReplacementChar(String) }`

`new` checks the following, each with its own error:
1. The list is not empty.
2. No value is empty.
3. Every stand-in matches `^[A-Za-z][A-Za-z0-9_.-]*$`.
4. Real values are unique, and so are stand-ins.
5. No stand-in is a substring of another stand-in (`Nested`).
6. No non-empty proper suffix of a stand-in equals a proper prefix of any stand-in, itself included (`Overlap`).
7. No real value is a substring of a stand-in, or the reverse (`Crossed`).
8. No real value contains U+FFFD.

Rules 5 and 6 make streaming restoration equal to whole-text restoration (Task 7's property test). Both directions match with aho-corasick `MatchKind::LeftmostLongest` on exact bytes. Plan 1 does no case folding.

- [ ] **Step 1: Write the failing tests:**

```rust
pub(crate) fn test_pairs() -> Pairs {
    Pairs::new(vec![
        Pair { real: "acmewidgets".into(), standin: "client_x01".into() },
        Pair { real: "AcmeWidgets".into(), standin: "ClientX01".into() },
        Pair { real: "Jeanne Fictive".into(), standin: "person_x01".into() },
    ]).unwrap()
}
#[test]
fn scrub_and_restore_round_trip() {
    let (s, n) = test_pairs().scrub_str("cd ~/code/acmewidgets && open AcmeWidgets.app # Jeanne Fictive");
    assert_eq!(s, "cd ~/code/client_x01 && open ClientX01.app # person_x01");
    assert_eq!(n, 3);
    assert_eq!(test_pairs().restore_str(&s).0, "cd ~/code/acmewidgets && open AcmeWidgets.app # Jeanne Fictive");
}
#[test]
fn hold_from_returns_longest_dangling_prefix() {
    let p = test_pairs();
    assert_eq!(p.hold_from("see the client_x0"), 8);
    assert_eq!(p.hold_from("see the c"), 8);
    assert_eq!(p.hold_from("see the client_x01"), 18);
    assert_eq!(p.hold_from("plain"), 5);
}
#[test]
fn rejects_ambiguous_sets() {
    let pr = |r: &str, s: &str| Pair { real: r.into(), standin: s.into() };
    assert!(matches!(Pairs::new(vec![]), Err(PairsError::Empty)));
    assert!(matches!(Pairs::new(vec![pr("a1", "x_x01"), pr("a2", "x_x01")]), Err(PairsError::DuplicateStandin(_))));
    assert!(matches!(Pairs::new(vec![pr("a1", "ab_x01"), pr("a2", "b_x01")]), Err(PairsError::Nested(..))));
    assert!(matches!(Pairs::new(vec![pr("a1", "ab_x01"), pr("a2", "x01_ab")]), Err(PairsError::Overlap(..))));
    assert!(matches!(Pairs::new(vec![pr("client", "client_x01")]), Err(PairsError::Crossed(..))));
    assert!(matches!(Pairs::new(vec![pr("a1", "client x01")]), Err(PairsError::BadStandin(_))));
}
```

- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib pairs -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement.** `hold_from` checks the suffixes that start on char boundaries against a `HashSet` of all proper prefixes of the stand-ins.
- [ ] **Step 4: Run them and check they pass.** Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita-core && git commit -m "core: Step 0 pairs dictionary with injectivity checks"`

---

### Task 5: Request scrubbing policy

**Files:**
- Create: `crates/tacita-core/src/request.rs`, `crates/tacita-core/tests/fixtures/claude_code_request.json`
- Modify: `crates/tacita-core/src/lib.rs`

**Interfaces:**
- Consumes: `scan`, `splice`, `decode`, `replace`, `Pairs::find_reals`.
- Produces (`tacita_core::request`):
  - `pub struct Scrubbed { pub body: Vec<u8>, pub replacements: usize, pub in_system: usize }`. `in_system` counts the replacements inside `system[2..]`. Step 0 uses it to prove that the cwd in the system prompt really was rewritten (spec §15.2).
  - `pub fn scrub_request(body: &[u8], pairs: &Pairs) -> Result<Scrubbed, RequestError>`
  - `#[derive(Debug, thiserror::Error)] pub enum RequestError { Json(#[from] ScanError), NotAnObject }`

**Policy:** a string literal is rewritten unless one of the rows below applies. Each row is checked against the body parsed with serde_json.

| Literal | Rule |
|---|---|
| any **key** | rewritten only inside a `tools[i].input_schema` subtree or the `input` subtree of a `tool_use` block; never elsewhere |
| `system` when it is a string | never |
| `system[0]` and `system[1]` subtrees | never |
| `model`; the `metadata` subtree | never |
| `tools[i].name`, `tool_choice.name` | never |
| the whole subtree of an object whose `type` is `thinking` or `redacted_thinking` | never |
| values of keys `type`, `role`, `id`, `tool_use_id`, `signature`, `media_type` | never, except inside `input` / `input_schema` subtrees, which hold user data |
| `name` of an object whose `type` is `tool_use`, `server_tool_use` or `mcp_tool_use` | never |
| `source.data` of any object | never |
| the `cache_control` subtree | never |

Everything else is scrubbed: message text, tool_result content, tool descriptions, `input_schema` contents, `system[2..]` and unknown fields. Only the matched name bytes change.

- [ ] **Step 1: Write the fixture** `claude_code_request.json`, exactly:

```json
{
  "model": "claude-opus-5-5",
  "max_tokens": 32000,
  "temperature": 1.0,
  "metadata": {"user_id": "{\"device_id\":\"acmewidgets-dev\",\"session_id\":\"s1\"}"},
  "system": [
    {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.300; cc_entrypoint=cli;"},
    {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}},
    {"type": "text", "text": "Working directory: /Users/dev/code/acmewidgets\nGit branch: acmewidgets-main", "cache_control": {"type": "ephemeral"}}
  ],
  "tools": [
    {"name": "mcp__acmewidgets__query", "description": "Query the acmewidgets database",
     "input_schema": {"type": "object", "properties": {"acmewidgets_id": {"type": "string", "description": "An acmewidgets ID"}}, "required": ["acmewidgets_id"]}}
  ],
  "messages": [
    {"role": "user", "content": [
      {"type": "text", "text": "Summarize acmewidgets/README.md"},
      {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "acmewidgets"}}
    ]},
    {"role": "assistant", "content": [
      {"type": "thinking", "thinking": "The user mentions acmewidgets.", "signature": "c2lnLWFjbWV3aWRnZXRz"},
      {"type": "tool_use", "id": "toolu_acmewidgets", "name": "mcp__acmewidgets__query", "input": {"acmewidgets_id": "acmewidgets-42", "type": "acmewidgets"}}
    ]},
    {"role": "user", "content": [
      {"type": "tool_result", "tool_use_id": "toolu_acmewidgets", "content": [{"type": "text", "text": "Row: acmewidgets, owner Jeanne Fictive"}]}
    ]}
  ]
}
```

- [ ] **Step 2: Write the failing tests.** They use `test_pairs()` from Task 4; make it `pub(crate)` under `#[cfg(test)]`.

```rust
const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/claude_code_request.json");
fn out() -> (serde_json::Value, String, usize) {
    let s = scrub_request(FIXTURE, &test_pairs()).unwrap();
    assert_eq!(s.in_system, 2);
    (serde_json::from_slice(&s.body).unwrap(), String::from_utf8(s.body).unwrap(), s.replacements)
}
#[test]
fn scrubs_user_content() {
    let (v, _, n) = out();
    assert_eq!(n, 12);
    assert_eq!(v.pointer("/system/2/text").unwrap(), "Working directory: /Users/dev/code/client_x01\nGit branch: client_x01-main");
    assert_eq!(v.pointer("/messages/0/content/0/text").unwrap(), "Summarize client_x01/README.md");
    assert!(v.pointer("/tools/0/input_schema/properties/client_x01_id").is_some());
    assert_eq!(v.pointer("/tools/0/input_schema/required/0").unwrap(), "client_x01_id");
    assert_eq!(v.pointer("/messages/1/content/1/input/client_x01_id").unwrap(), "client_x01-42");
    assert_eq!(v.pointer("/messages/1/content/1/input/type").unwrap(), "client_x01");
    assert_eq!(v.pointer("/messages/2/content/0/content/0/text").unwrap(), "Row: client_x01, owner person_x01");
}
#[test]
fn never_touches_protected_parts() {
    let (v, text, _) = out();
    let orig: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    for p in ["/system/0", "/system/1", "/model", "/metadata", "/tools/0/name", "/messages/1/content/0",
              "/messages/1/content/1/id", "/messages/1/content/1/name", "/messages/2/content/0/tool_use_id",
              "/messages/0/content/1/source/data"] {
        assert_eq!(v.pointer(p), orig.pointer(p), "{p}");
    }
    assert_eq!(text.matches("acmewidgets").count(), 7);
}
#[test]
fn deterministic_and_idempotent() {
    let a = scrub_request(FIXTURE, &test_pairs()).unwrap().body;
    assert_eq!(a, scrub_request(FIXTURE, &test_pairs()).unwrap().body);
    assert_eq!(scrub_request(&a, &test_pairs()).unwrap().body, a);
}
#[test]
fn invalid_json_is_an_error() {
    assert!(scrub_request(b"{\"messages\": [", &test_pairs()).is_err());
}
```

- [ ] **Step 3: Run them and check they fail.** Run: `cargo test -p tacita-core --lib request -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 4: Implement** `scrub_request`: `scan`, then filter by the policy, then `find_reals`, then `replace`, then `splice`. Validate with `serde_json::from_slice::<Value>`; a body that is not an object returns `NotAnObject`.
- [ ] **Step 5: Run them and check they pass.** Expected: `ok`.
- [ ] **Step 6: Commit.** `git add -A crates/tacita-core && git commit -m "core: request scrub policy that never touches protected parts"`

---

### Task 6: Incremental SSE parser

**Files:**
- Create: `crates/tacita-core/src/sse.rs`
- Modify: `crates/tacita-core/src/lib.rs`

**Interfaces:**
- Produces (`tacita_core::sse`):
  - `#[derive(Clone, Debug, PartialEq)] pub struct SseEvent { pub event: Option<String>, pub data: String, pub raw: Vec<u8> }`
  - `impl SseEvent { pub fn build(event: &str, data: &str) -> SseEvent }`. This sets `raw` to `"event: {event}\ndata: {data}\n\n"`, and `data` must contain no newline.
  - `#[derive(Default)] pub struct SseParser`, with:
    - `push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError>`
    - `finish(&mut self) -> Vec<u8>`, which returns the bytes of an unfinished event.
  - `#[derive(Debug, thiserror::Error)] pub enum SseError { Utf8 }`

Parsing rules follow WHATWG:
- Lines end with CRLF, LF or CR. A CR at the end of a chunk waits for the next byte.
- A blank line ends an event.
- `data` lines are joined with `\n`, and one leading space after `:` is removed.
- `:` lines are comments, and other fields are kept in `raw` only.
- A comment-only block is still returned, so its raw bytes are forwarded.
- `raw` holds the exact bytes from the event's first byte through its terminating blank line.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn parses_anthropic_events() {
    let mut p = SseParser::default();
    let evs = p.push(b"event: ping\ndata: {\"type\": \"ping\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].event.as_deref(), Some("ping"));
    assert_eq!(evs[0].data, "{\"type\": \"ping\"}");
    assert_eq!(evs[0].raw, b"event: ping\ndata: {\"type\": \"ping\"}\n\n".to_vec());
}
#[test]
fn handles_crlf_cr_and_multiline_data() {
    let mut p = SseParser::default();
    let evs = p.push(b"event: x\r\ndata: a\r\ndata: b\r\n\r\ndata: c\r\rdata: d").unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].data, "a\nb");
    assert_eq!(evs[1].data, "c");
}
#[test]
fn cr_at_chunk_end_is_not_a_blank_line() {
    let mut p = SseParser::default();
    assert!(p.push(b"data: a\r").unwrap().is_empty());
    let evs = p.push(b"\ndata: b\n\n").unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].data, "a\nb");
}
#[test]
fn rejects_invalid_utf8() {
    assert!(SseParser::default().push(b"data: \xff\n\n").is_err());
}
#[test]
fn finish_returns_unterminated_bytes() {
    let mut p = SseParser::default();
    p.push(b"data: partial").unwrap();
    assert_eq!(p.finish(), b"data: partial".to_vec());
}
```

  Add the property test `any_chunking_gives_same_events`. Its stream is three events, one with `é😀` in its data and CRLF line endings. Split the stream at up to 6 random byte positions, including inside multi-byte characters. The events must equal those from a single `push`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib sse -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement.** Work on bytes, and convert `data` to a `String` only when an event completes.
- [ ] **Step 4: Run them and check they pass.** Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita-core && git commit -m "core: incremental SSE parser"`

---

### Task 7: Streaming restorer, non-streaming restore, Tacita errors

**Files:**
- Create: `crates/tacita-core/src/restore.rs`, `crates/tacita-core/src/errors.rs`
- Modify: `crates/tacita-core/src/lib.rs`

**Interfaces:**
- Consumes: `Pairs`, `SseEvent`, `scan`, `scan_tolerant`, `decode`, `encode`, `replace`, `splice`.
- Produces:
  - `tacita_core::errors::error_body(message: &str) -> Vec<u8>`. It returns `{"type":"error","error":{"type":"invalid_request_error","message":"Tacita: <message>"}}`.
  - `tacita_core::errors::error_event(message: &str) -> SseEvent`. It returns event `error` with the same data.
  - `pub struct Limits { pub max_tool_bytes: usize }`, where `Default` is 16 MiB.
  - `pub struct Restorer`, with:
    - `new(pairs: Arc<Pairs>, limits: Limits, fail_on_text: Option<String>) -> Restorer`
    - `push(&mut self, ev: SseEvent) -> Result<Vec<SseEvent>, RestoreError>`
    - `finish(&mut self) -> Vec<SseEvent>`
    - `is_holding(&self) -> bool`
    - `restored(&self) -> usize`
    - `dangling(&self) -> usize`
  - `#[derive(Debug, thiserror::Error)] pub enum RestoreError { BadEvent(String), ToolTooLarge, Probe }`
  - `pub fn restore_message(body: &[u8], pairs: &Pairs) -> Result<(Vec<u8>, usize), ScanError>`

**Behaviour** (from research R7, R8, R11 and R14):

| Upstream event | Output |
|---|---|
| `content_block_start`, `content_block.type == "text"` | forwarded; opens a text state for that index |
| `text_delta` for a text index | `out = restore(carry + text)` up to `hold_from`; the rest becomes the new carry. Only the `delta.text` literal is replaced, with `encode(out)`. The event is dropped if `out` is empty |
| `content_block_start`, type `tool_use` | held; buffering starts. Another `content_block_start` while holding gives `BadEvent` |
| `input_json_delta` for the held index | `partial_json` is appended; past `max_tool_bytes` this gives `ToolTooLarge` |
| `content_block_stop` for the held index | emits the held start, then one `input_json_delta`, then the stop. The delta is the block's first delta event with its `partial_json` literal replaced. Its value is the buffer, with every complete key and value literal found by `scan_tolerant` restored. The delta is omitted if the buffer is empty |
| `content_block_stop` for a text index with a carry | emits a `text_delta` holding the carry (the block's last delta, with its text replaced), then the stop. Increments `dangling` |
| `message_delta`, `message_stop`, `error` | flush all carries first, then forward unchanged |
| everything else (`ping`, `message_start`, `thinking_delta`, `signature_delta`, `citations_delta`, other block types, unknown events) | forwarded raw. Thinking is never restored |

- `fail_on_text`: if a text block's restored text contains this string, the result is `RestoreError::Probe`. This is the Step 0 stream probe.
  - The match runs **across deltas**, because live streams split the probe over several deltas.
  - To do that, keep the last `probe.len() - 1` bytes of each text block's restored output, and search that tail plus the new output.
- `finish`: flushes text carries. A tool block that is still held is dropped and never emitted.
- `restore_message` restores two things only:
  - `content[i].text` where `content[i].type == "text"`;
  - the keys and values under `content[i].input` where `content[i].type == "tool_use"`.

- [ ] **Step 1: Write the failing tests.**
  - Helpers:
    - `ev(name, json!)` builds an event with `SseEvent::build(name, &data.to_string())`.
    - `text_stream(parts)` builds content_block_start(text, 0), one text_delta per part, content_block_stop(0) and message_stop.
    - `run(r, evs)` pushes every event, then calls `finish`.
    - `text_of(evs)` concatenates the `delta.text` of every text_delta.
    - `pairs()` returns `Arc::new(test_pairs())`.
  - Tests:

```rust
#[test]
fn restores_stand_in_split_across_deltas() {
    let out = run(&mut Restorer::new(pairs(), Limits::default(), None), text_stream(&["the cli", "ent_x0", "1 file"]));
    assert_eq!(text_of(&out), "the acmewidgets file");
}
#[test]
fn dangling_prefix_flushed_before_stop() {
    let mut r = Restorer::new(pairs(), Limits::default(), None);
    let out = run(&mut r, text_stream(&["the proj", "ect plan ClientX0"]));
    assert_eq!(text_of(&out), "the project plan ClientX0");
    let stop = out.iter().position(|e| e.event.as_deref() == Some("content_block_stop")).unwrap();
    assert!(out[..stop].iter().any(|e| e.data.contains("ClientX0")));
    assert_eq!(r.dangling(), 1);
}
#[test]
fn tool_use_buffered_and_restored_once() {
    let evs = vec![
        ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\": \"/Users/dev/code/cli"}})),
        ev("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"ent_x01/a.md\", \"n\": 1e3}"}})),
        ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
    ];
    let out = run(&mut Restorer::new(pairs(), Limits::default(), None), evs);
    assert_eq!(out.len(), 3);
    let d: serde_json::Value = serde_json::from_str(&out[1].data).unwrap();
    assert_eq!(d["delta"]["partial_json"], "{\"file_path\": \"/Users/dev/code/acmewidgets/a.md\", \"n\": 1e3}");
}
```

  Add these, each with the assertion named in its title:
  - `any_split_equals_full_restore` (proptest, T1). Text is built from up to 30 tokens chosen from `["client_x01","ClientX01","person_x01","clie","x0","1"," ","é","😀","a"]` and split at up to 6 random char boundaries; `text_of(run(..)) == test_pairs().restore_str(&full).0`.
  - `tool_input_real_value_is_escaped`: with a pair `{real: "L'Oréal \"SA\"", standin: "company_x01"}`, the tool input `{"q":"company_x01"}` restores to JSON that serde parses as `q == "L'Oréal \"SA\""`.
  - `ping_during_hold_passes_through_immediately`: after a held tool start, pushing a ping returns that ping, and `is_holding()` is true.
  - `thinking_is_never_restored`: a `thinking_delta` containing `client_x01` comes out with byte-identical `raw`.
  - `overlapping_blocks_fail_closed`: a second `content_block_start` while holding gives `Err(BadEvent)`.
  - `tool_too_large_fails`: with `max_tool_bytes: 10`, the result is `Err(ToolTooLarge)`.
  - `finish_drops_held_tool_block`: a tool start plus one delta, then `finish()`, yields no output event with `"tool_use"`.
  - `probe_split_across_deltas_fails`: `fail_on_text = Some("TACITA_STREAM_PROBE")` with `text_stream(&["TACITA_STR", "EAM_PRO", "BE"])` gives `Err(Probe)`.
  - `restore_message_non_streaming`: for `{"content":[{"type":"text","text":"client_x01"},{"type":"thinking","thinking":"client_x01"},{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/x/client_x01"}}]}`, the text and `file_path` are restored and the thinking text is unchanged.
  - `error_body_shape`: `error_body("x")` parses with `error.type == "invalid_request_error"` and `error.message == "Tacita: x"`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib restore -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement** as specified. Events are read with `serde_json::from_str::<Value>`, and changed events are rebuilt by splicing the one literal, so unknown fields survive.
- [ ] **Step 4: Run them and check they pass**, then run `cargo test -p tacita-core -q 2>&1 | tail -2`. Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita-core && git commit -m "core: streaming restorer with hold-back and tool buffering"`

---

### Task 8: OpenRouter request adapter

**Files:**
- Create: `crates/tacita-core/src/openrouter.rs`
- Modify: `crates/tacita-core/src/lib.rs`

**Interfaces:**
- Produces (`tacita_core::openrouter`):
  - `pub struct Prefs { pub only: Vec<String> }`
  - `pub fn adapt_request(body: &[u8], slug: &str, prefs: &Prefs) -> Result<Vec<u8>, AdaptError>`
  - `pub fn adapt_beta(value: &str) -> Option<String>`
  - `#[derive(Debug, thiserror::Error)] pub enum AdaptError { Json(String), UnknownModel(String), EmptyMessage(usize), ProviderFieldPresent }`

`adapt_request` rules:
1. A `model` starting with `claude-` becomes `slug`. Any other model gives `UnknownModel`.
2. Every `messages[i].content[j]` whose `type` is `thinking` or `redacted_thinking` is removed. If that empties a message, the result is `EmptyMessage(i)`.
3. `metadata.user_id` is removed.
4. `"provider": {"data_collection": "deny", "zdr": true, "allow_fallbacks": false}` is added, plus `"only": [...]` when the list is non-empty. If `provider` already exists, the result is `ProviderFieldPresent`.
5. The body is re-serialized with serde_json. That is allowed here, because this body goes to OpenRouter, not Anthropic.

`adapt_beta` splits on `,`, trims each item, drops items starting with `oauth-`, and joins the rest with `,`. It returns `None` if nothing is left.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn adapts_body_for_openrouter() {
    let body = br#"{"model":"claude-opus-5-5","metadata":{"user_id":"u"},"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"hi"}]}]}"#;
    let v: serde_json::Value = serde_json::from_slice(&adapt_request(body, "qwen/qwen3-coder", &Prefs { only: vec!["groq".into()] }).unwrap()).unwrap();
    assert_eq!(v["model"], "qwen/qwen3-coder");
    assert!(v["metadata"].get("user_id").is_none());
    assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(v["provider"], serde_json::json!({"data_collection":"deny","zdr":true,"allow_fallbacks":false,"only":["groq"]}));
}
#[test]
fn unknown_model_fails_closed() {
    assert!(matches!(adapt_request(br#"{"model":"gpt-5","messages":[]}"#, "x/y", &Prefs { only: vec![] }), Err(AdaptError::UnknownModel(_))));
}
#[test]
fn beta_header_loses_only_oauth() {
    assert_eq!(adapt_beta("oauth-2025-04-20, claude-code-20250219").as_deref(), Some("claude-code-20250219"));
    assert_eq!(adapt_beta("oauth-2025-04-20"), None);
}
```

  Also add `thinking_only_message_fails` (expects `EmptyMessage(0)`) and `existing_provider_field_fails`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita-core --lib openrouter -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement** with `serde_json::Value` (`preserve_order` keeps key order).
- [ ] **Step 4: Run them and check they pass.** Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita-core && git commit -m "core: OpenRouter request adapter"`

---

### Task 9: Config, state, and the `init` / `status` / `use` commands

**Files:**
- Create: `crates/tacita/src/paths.rs`, `crates/tacita/src/config.rs`, `crates/tacita/tests/cli.rs`
- Modify: `crates/tacita/src/lib.rs`, `crates/tacita/src/main.rs`

**Interfaces:**
- Consumes: `Pair`, `Pairs`, `PairsError`.
- Produces:
  - `paths::home() -> PathBuf`: `$TACITA_HOME` if set, else `~/Library/Application Support/Tacita`.
  - `paths::ensure_private_dir(dir: &Path) -> io::Result<()>`: creates the directory with mode 0700.
  - `paths::write_private(path: &Path, bytes: &[u8]) -> io::Result<()>`: writes a temporary 0600 file in the same directory, then renames it into place.
  - `config::Config { pub port: u16, pub secret: String, pub anthropic: AnthropicCfg, pub openrouter: OpenRouterCfg, #[serde(default)] pub step0: Step0Cfg }`
  - `AnthropicCfg { pub protection: Protection }`
  - `OpenRouterCfg { pub protection: Protection, #[serde(default)] pub only: Vec<String> }`
  - `#[serde(rename_all = "kebab-case")] pub enum Protection { Off, Pairs }`
  - `#[derive(Default)] Step0Cfg { pub pairs: Vec<Pair>, pub block_probe: Option<String>, pub stream_probe: Option<String>, pub guard_probe: Option<String>, pub record_dir: Option<PathBuf> }`
  - `Config::load(home: &Path) -> Result<Config, ConfigError>`
  - `Config::create_default(home: &Path) -> Result<Config, ConfigError>`
  - `Config::base_url(&self) -> String`: `http://127.0.0.1:{port}/{secret}`
  - `Config::pairs(&self) -> Result<Arc<Pairs>, ConfigError>`
  - `#[serde(rename_all = "kebab-case")] pub enum Provider { Anthropic, Openrouter }`
  - `pub struct State { pub provider: Provider, pub openrouter_model: Option<String> }`
  - `State::load(home) -> Result<State, ConfigError>`: a missing file means Anthropic.
  - `State::save(&self, home) -> Result<(), ConfigError>`: uses `write_private`.
  - `#[derive(Debug, thiserror::Error)] pub enum ConfigError { Io(…), Toml(…), BadSecret, BadPort, OpenRouterUnprotected, Pairs(PairsError), RecordDirNotAbsolute }`

**`load` validation:**
- `secret` is exactly 32 lowercase hex characters, and `port` is not 0.
- `openrouter.protection = "off"` gives `OpenRouterUnprotected`.
- If either provider uses `pairs`, `Pairs::new(step0.pairs)` must succeed.
- `record_dir` must be absolute.

**Files:** config is `<home>/config.toml` and state is `<home>/state.toml`. `create_default` writes the text below, with `{secret}` replaced by 16 bytes from `/dev/urandom` in lowercase hex:

```toml
port = 47821
secret = "{secret}"

[anthropic]
protection = "off"     # "off" = Step 0 test 1, "pairs" = test 2

[openrouter]
protection = "pairs"   # "off" is refused
only = []              # optional trusted OpenRouter provider slugs

[step0]
pairs = [
  { real = "acmewidgets", standin = "client_x01" },
  { real = "AcmeWidgets", standin = "ClientX01" },
  { real = "Jeanne Fictive", standin = "person_x01" },
]
block_probe = "TACITA_BLOCK_PROBE"
stream_probe = "TACITA_STREAM_PROBE"
guard_probe = "TACITA_GUARD_PROBE"
# record_dir = "/Users/you/tacita-corpus"   # Step 0 test 5; never inside a git repo
```

**CLI** (clap derive):
- `tacita init`
- `tacita status [--base-url]`
- `tacita use anthropic`
- `tacita use openrouter --model <slug>`

Exact output:
- `init` on a fresh home prints `Created <home>/config.toml` and then `For one Claude Code session:  ANTHROPIC_BASE_URL=<base_url> claude`. If the config already exists, it prints `Already initialised: <home>/config.toml` and the same second line.
- `status` prints four aligned lines:
  - `provider:   anthropic`, or `provider:   openrouter (model <slug>)`;
  - `protection: off|pairs` for the active provider;
  - `base URL:   <base_url>`;
  - `daemon:     running|not running`. The check is a 1 s `HEAD <base_url>/api/hello`.
- `status --base-url` prints only the URL.
- `use …` saves the state, then prints what `status` prints.

- [ ] **Step 1: Write the failing tests.**
  - Unit tests in `config.rs`: `default_config_loads`, `openrouter_off_is_refused`, `bad_secret_is_refused`, `invalid_pairs_are_refused`, `state_defaults_to_anthropic`, and `state_file_is_0600` (checked with `std::os::unix::fs::PermissionsExt`).
  - `tests/cli.rs` runs `env!("CARGO_BIN_EXE_tacita")` with `TACITA_HOME` set to a tempdir:

```rust
#[test]
fn init_status_use() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| std::process::Command::new(env!("CARGO_BIN_EXE_tacita")).args(args).env("TACITA_HOME", home.path()).output().unwrap();
    let init = String::from_utf8(run(&["init"]).stdout).unwrap();
    assert!(init.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:47821/"));
    let url = String::from_utf8(run(&["status", "--base-url"]).stdout).unwrap();
    assert!(url.trim().starts_with("http://127.0.0.1:47821/") && url.trim().len() == "http://127.0.0.1:47821/".len() + 32);
    let used = String::from_utf8(run(&["use", "openrouter", "--model", "x/y"]).stdout).unwrap();
    assert!(used.contains("provider:   openrouter (model x/y)"));
    assert!(!run(&["use", "openrouter"]).status.success());
}
```

- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita -q 2>&1 | tail -3`. Expected: a compile error, or a test failure.
- [ ] **Step 3: Implement** as specified. `main` is `#[tokio::main]` and dispatches the clap subcommands.
- [ ] **Step 4: Run them and check they pass.** Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "cli: config, state, init/status/use"`

---

### Task 10: Daemon pass-through (routing, forwarding, streaming, metadata log)

**Files:**
- Create: `crates/tacita/src/secrets.rs`, `crates/tacita/src/daemon/{mod.rs,route.rs,forward.rs,metalog.rs}`, `crates/tacita/tests/support/mod.rs`, `crates/tacita/tests/proxy_passthrough.rs`
- Modify: `crates/tacita/src/lib.rs`, `crates/tacita/src/main.rs` (add `tacita daemon`)

**Interfaces:**
- Consumes: `Config`, `State`, `paths`, `SseParser`, `errors::error_body`.
- Produces:
  - **Secrets (`secrets.rs`):**
    - `pub trait SecretStore: Send + Sync { fn get(&self, name: &str) -> anyhow::Result<Option<String>>; fn set(&self, name: &str, value: &str) -> anyhow::Result<()>; }`
    - `#[derive(Default)] pub struct MemoryStore`
    - `pub struct KeychainStore { service: String }` and `KeychainStore::new(service: &str)`. The account is `name`, using `security_framework::passwords::{get_generic_password, set_generic_password}`. A "not found" error maps to `Ok(None)`. Production code uses `KeychainStore::new("dev.tacita")`.
  - **Daemon types (`daemon/mod.rs`):**
    - `pub struct Timing { pub ping_every: Duration, pub hold_limit: Duration, pub connect_timeout: Duration }`. `Default` is 10 s, 480 s and 10 s.
    - `pub struct DaemonOptions { pub home: PathBuf, pub anthropic_upstream: String, pub openrouter_upstream: String, pub secrets: Arc<dyn SecretStore>, pub timing: Timing }`
    - `DaemonOptions::production(home)`: `https://api.anthropic.com`, `https://openrouter.ai/api`, `KeychainStore`, default timing.
    - `pub async fn serve(opts: DaemonOptions, listener: tokio::net::TcpListener) -> anyhow::Result<()>`. It loads `Config` once, and `State` on every request. It uses the listener's port for the Host check.
  - **Routing (`route.rs`):**
    - `pub enum Route { Messages { query: Option<String> }, CountTokens, Hello, NotFound, Forbidden(&'static str) }`
    - `pub fn route(method: &Method, uri: &Uri, headers: &HeaderMap, secret: &str, port: u16) -> Route`
    - Order of checks:
      1. If `Host` is not `127.0.0.1:{port}` or `localhost:{port}`, the route is `Forbidden`.
      2. If any `origin`, `referer`, `sec-fetch-site`, `sec-fetch-dest` or `sec-fetch-user` header is present, the route is `Forbidden`.
         - `sec-fetch-mode` alone is allowed: Node's fetch adds `sec-fetch-mode: cors` to every request.
         - A browser always sends `Origin` or `Sec-Fetch-Site` to 127.0.0.1, so it is still refused.
         - This narrows research §7's "reject `Sec-Fetch-*`" (my call).
      3. If the path doesn't start with `/{secret}/` (constant-time compare), the route is `NotFound`.
      4. Then `POST /v1/messages` is `Messages`, `POST /v1/messages/count_tokens` is `CountTokens`, `HEAD` or `GET /api/hello` is `Hello`, and anything else is `NotFound`. The query string is ignored for matching and kept for forwarding.
  - **Forwarding (`forward.rs`):**
    - `pub fn anthropic_headers(incoming: &HeaderMap, protected: bool) -> HeaderMap`. It copies everything except hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host` and `content-length`. When `protected` is true, `accept-encoding` is set to `identity`.
    - `pub fn response_headers(upstream: &HeaderMap, body_rewritten: bool) -> HeaderMap`. It drops hop-by-hop headers, and drops `content-length` when the body is rewritten.
    - `pub fn client(t: &Timing) -> reqwest::Client`: `redirect(Policy::none())`, `no_proxy()`, `connect_timeout`, and no total timeout.
  - **Metadata log (`metalog.rs`):**
    - `#[derive(Serialize, Default)] pub struct RequestMeta { ts, n, method, path, query, route, provider, protection, session, model_in, model_out, header_names, oauth_bearer, oauth_beta, system1_head, scrubbed, scrubbed_system, restored, dangling, status, ratelimit, retry_after, request_id, usage, stop_reason, upstream_error, tacita_error, ms }`
      - `path` has the secret removed, or is `"(wrong secret)"`.
      - `header_names` is sorted and lowercase.
      - `oauth_bearer` is true if `authorization` starts with `Bearer sk-ant-oat`.
      - `oauth_beta` is true if `anthropic-beta` has an item starting with `oauth-`.
      - `system1_head` is the first 60 chars of `system[1].text`.
      - `ratelimit` is a `BTreeMap` of every `anthropic-ratelimit-unified-*` header.
      - `usage` is `{input, cache_creation, cache_read, output}`.
      - `upstream_error` is `"<error.type>: <error.message>"`.
    - `pub fn observe(meta: &mut RequestMeta, ev: &SseEvent)`. It reads `message_start.message.usage`, `message_delta.usage.output_tokens`, `message_delta.delta.stop_reason` and `error.error`.
    - `pub fn redact(line: &str) -> String`. It rewrites `sk-ant-` and `sk-or-` followed by `[A-Za-z0-9_-]*` to `sk-ant-[redacted]` and `sk-or-[redacted]`.
    - `pub struct MetaLog`, with `MetaLog::open(home)`, which uses `<home>/logs/requests.jsonl` with mode 0600, and `append(&self, meta: &RequestMeta)`. `append` writes `redact(serde_json line)` and prints a one-line summary on stderr: `n`, path, provider, status, ms.
- **Test support (`tests/support/mod.rs`):**
  - `Received { method, path_and_query, headers: HeaderMap, body: Vec<u8> }`
  - `Reply::sse(body)`, `Reply::status(code, headers, body)`, `Reply::chunks(status, headers, Vec<(delay_ms, Vec<u8>)>)`, and `Reply::Echo`. `Echo` answers with an SSE text stream of the request's last `messages[-1].content[0].text`, in 3-character deltas.
  - `FakeUpstream::start(replies) -> FakeUpstream { url, received: Arc<Mutex<Vec<Received>>> }`. Replies are used in order, and the last one repeats.
  - `start_tacita(config_toml, state_toml: Option<&str>, anthropic: &FakeUpstream, openrouter: &FakeUpstream, secrets: Arc<MemoryStore>, timing: Timing) -> Tacita { base, home, port }`
  - `sse(&[(&str, Value)]) -> Vec<u8>`
  - `pub const SECRET: &str = "0123456789abcdef0123456789abcdef"`
  - `pub const CONFIG_OFF: &str` and `pub const CONFIG_PAIRS: &str`. Each is the full default config text with `SECRET`, with `[anthropic] protection` set to `"off"` or `"pairs"`. Tasks 11–13 use them from here.
  - `pub async fn wait_for_lines(path: &Path, n: usize) -> Vec<String>`. It polls every 20 ms, for up to 2 s, until the file has `n` lines. Every test that reads `requests.jsonl` uses it, because the meta line is written after the client has the body.
  - The file starts with `#![allow(dead_code)]`, because each test crate uses only some of the helpers.
- **Messages in this task:** handled for `provider = anthropic` with `protection = off`. Steps:
  1. Read the body, up to 64 MiB; past that, return 413 with `error_body("request larger than 64 MiB")`.
  2. Forward it to `{anthropic_upstream}/v1/messages{?query}`.
  3. Stream the response bytes through unchanged, and read metadata on the side:
     - When the response is `text/event-stream`, a side `SseParser` feeds `observe`.
     - When the status is ≥ 400 and the body is not SSE, tee up to 64 KiB of it. When the stream ends, parse `error.type` and `error.message` into `upstream_error`.
  4. Write the meta line when the stream ends or is dropped.
  5. If the upstream is unreachable, return 502 with `error_body("cannot reach <host>: <reason>")`.
- **Modes not built yet:** any other provider or protection combination returns 400 with `error_body("this mode is not built yet")` until Tasks 11–12 replace that.
- **Other routes:**
  - `CountTokens` gets a 404 with `error_body("count_tokens is not forwarded")`.
  - `Hello` gets an empty 200.
  - `NotFound` gets a 404, and `Forbidden` a 403. Each is logged with `route` set to `refused` or `forbidden`.
- `tacita daemon` binds `127.0.0.1:{config.port}`, prints `Tacita listening on <base_url>`, and stops on Ctrl-C.

- [ ] **Step 1: Write the failing tests** in `proxy_passthrough.rs`. `CONFIG_OFF` is the default config with `SECRET`. `MIN_BODY` is `{"model":"claude-opus-5-5","max_tokens":10,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}`.

```rust
#[tokio::test]
async fn forwards_auth_and_beta_headers_unchanged() {
    let up = FakeUpstream::start(vec![Reply::sse(sse(&[("message_stop", json!({"type":"message_stop"}))]))]).await;
    let or = FakeUpstream::start(vec![]).await;
    let t = start_tacita(CONFIG_OFF, None, &up, &or, Default::default(), Timing::default()).await;
    let resp = reqwest::Client::new().post(format!("{}/v1/messages?beta=true", t.base))
        .header("authorization", "Bearer sk-ant-oat01-TESTTOKEN")
        .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219")
        .header("x-claude-code-session-id", "sess-1")
        .body(MIN_BODY).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let got = &up.received.lock().unwrap()[0];
    assert_eq!(got.path_and_query, "/v1/messages?beta=true");
    assert_eq!(got.headers["authorization"], "Bearer sk-ant-oat01-TESTTOKEN");
    assert_eq!(got.headers["anthropic-beta"], "oauth-2025-04-20,claude-code-20250219");
    assert_eq!(got.headers["x-claude-code-session-id"], "sess-1");
    assert_eq!(got.body, MIN_BODY.as_bytes());
    assert!(or.received.lock().unwrap().is_empty());
}
```

  Add these, each asserting what its name says:
  - `streams_without_buffering`: the upstream sends its 2nd chunk 400 ms after the 1st; the client reads the 1st chunk less than 300 ms after sending.
  - `count_tokens_is_404_and_not_forwarded`
  - `hello_is_answered_locally`
  - `wrong_secret_and_unknown_paths_are_refused`: a wrong secret and `GET {base}/v1/models` both get a 404; the upstream receives 0 requests; `requests.jsonl` has 2 lines with `"route":"refused"`.
  - `foreign_host_and_browser_headers_are_forbidden`: `Host: evil.example:<port>`, `Origin: https://x` and `Sec-Fetch-Site: cross-site` each get a 403.
  - `sec_fetch_mode_alone_is_allowed`: a request carrying only `sec-fetch-mode: cors` is forwarded normally.
  - `redirects_are_not_followed`: an upstream 307 to `http://127.0.0.1:1/x` reaches the client as 307, and the upstream received exactly 1 request. This test's client is `reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()`.
  - `upstream_errors_pass_verbatim`: a 429 with `anthropic-ratelimit-unified-status: rejected`, `retry-after: 30` and body `{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}` reaches the client identical. The meta line has that header in `ratelimit` and `"upstream_error":"rate_limit_error: Error"`.
  - `oversized_body_is_refused`: 413, with a message starting with `Tacita: `.
  - `upstream_unreachable_is_a_visible_error`: with the upstream at `http://127.0.0.1:1`, the result is 502 with a `Tacita: ` message.
  - `metalog_records_safe_fields_only`: the meta line has `"oauth_bearer":true`, `"oauth_beta":true` and `system1_head == "You are Claude Code, Anthropic's official CLI for Claude."`. It has `usage.cache_read == 7`, taken from the SSE `message_start`. `requests.jsonl` does not contain `TESTTOKEN`.
  - `redact_hides_tokens` (unit test in `metalog.rs`): `redact("x sk-ant-oat01-AbC_9 y")` equals `"x sk-ant-[redacted] y"`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita --test proxy_passthrough -q 2>&1 | tail -3`. Expected: a compile error.
- [ ] **Step 3: Implement** as specified. The axum `Router` has a single `fallback` handler that calls `route()`. Response bodies use `axum::body::Body::from_stream`.
- [ ] **Step 4: Run them and check they pass**, then run `cargo test -q 2>&1 | tail -2`. Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "daemon: local pass-through proxy with safe metadata log"`

---

### Task 11: Protection in the daemon (scrub, restore, probes, holds)

**Files:**
- Create: `crates/tacita/src/daemon/stream.rs`, `crates/tacita/tests/proxy_scrub.rs`
- Modify: `crates/tacita/src/daemon/mod.rs`

**Interfaces:**
- Consumes: `scrub_request`, `Restorer`, `restore_message`, `error_body`, `error_event`, `observe`, `Timing`.
- Produces: `pub fn restoring_body(upstream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static, restorer: Restorer, timing: Timing, meta: MetaHandle) -> axum::body::Body`. `MetaHandle` is the builder's type for finishing the meta line from the stream.

**Behaviour:**
- **Block probe.** If the request body bytes contain `step0.block_probe`, return 400 with `error_body("block probe: this request was blocked on purpose (Step 0)")`. This applies in every protection mode, and nothing is forwarded. It writes a meta line with `route = "messages"` and `tacita_error = "block probe"`.
- **`protection = pairs`:**
  - `scrub_request`; on error, return 400 with `error_body("could not read this request: <err>")`;
  - forward with `anthropic_headers(.., true)`;
  - record `scrubbed` and `scrubbed_system` (from `in_system`) in the meta line.
- **Response** (protected modes only):
  - **2xx `text/event-stream`:**
    - If `content-encoding` is present and not `identity`, return 502 with `error_body("compressed stream from upstream; refusing to pass it on unrestored")`.
    - Otherwise run `SseParser` → `Restorer`, with `fail_on_text = step0.stream_probe`, then emit each event's `raw`.
  - **2xx `application/json`:** read the whole body (up to 64 MiB) and pass it through `restore_message`.
  - **Any other 2xx** is never passed on unrestored. That covers a response that is compressed, larger than 64 MiB, not parseable by `restore_message`, or neither SSE nor JSON. It gets 502 with `error_body("could not restore the reply: <reason>")`, and `tacita_error` is set.
  - **Non-2xx:** pass through verbatim, without restoring.
- **Stream loop** (`stream.rs`):
  - While `restorer.is_holding()` and nothing has been sent for `ping_every`, emit `event: ping\ndata: {"type": "ping"}\n\n`.
  - If a hold lasts longer than `hold_limit`, emit `error_event("a tool call took longer than <n> s to arrive and was not passed on")` and end the stream.
    - Each wait is `timeout(min(ping_every, hold_deadline - now), upstream.next())`.
    - The hold age is checked before each upstream item is pushed into the restorer.
  - A parser or restorer error emits `error_event(<reason>)` and ends the stream. A probe gives `"stream probe: this reply was stopped on purpose (Step 0)"`.
  - If the upstream stream errors or ends:
    - Emit `restorer.finish()`.
    - If neither `message_stop` nor an upstream `error` event was seen, also emit `error_event("the reply from upstream was cut off; any tool call in progress was not passed on")` and set `tacita_error`.
    - Then end the stream. The partial event bytes from `parser.finish()` are dropped.
  - Every emitted event goes through `observe`. When the stream ends, the meta line gets `restored`, `dangling` and `tacita_error`.

- [ ] **Step 1: Write the failing tests** in `proxy_scrub.rs`. `CONFIG_PAIRS` is the default config with `[anthropic] protection = "pairs"`.
  - `scrubbed_request_reaches_upstream`: the request is the Task 5 fixture. The upstream body has no `acmewidgets` in `/messages/0/content/0/text`, and `system[1]` is equal to the original. The `accept-encoding` header is `identity`.
  - `echo_round_trip_restores_names`: the user text `hello acmewidgets and Jeanne Fictive` reaches the upstream as `hello client_x01 and person_x01`. The client's concatenated `text_delta` text is `hello acmewidgets and Jeanne Fictive`.
  - `tool_use_restored_in_one_delta`: the upstream streams the Task 7 tool block. The client receives exactly one `input_json_delta`, whose `file_path` contains `/acmewidgets/`.
  - `pings_sent_while_holding`: timing is `{ping_every: 50 ms, hold_limit: 2 s}`. The upstream pauses 300 ms after the tool start. The client sees at least 2 `event: ping` events before the `tool_use` start.
  - `hold_limit_fails_closed`: timing is `{ping_every: 50 ms, hold_limit: 200 ms}`. With a 600 ms upstream pause inside the tool block, the client stream has `event: error` with `Tacita:`, no `"type":"tool_use"` start, and the meta line's `tacita_error` is set.
  - `stream_cut_mid_tool_drops_the_tool`: the upstream sends the text `see client_x0`, then a tool start and one partial delta, then closes. The client text is `see client_x0`, no `tool_use` start reached the client, and the stream ends with `event: error` containing `cut off`.
  - `unreadable_json_reply_fails_closed`: a 2xx `application/json` body that is not valid JSON gives 502 with a `Tacita: ` message.
  - `compressed_stream_fails_closed`: an upstream `content-encoding: gzip` gives 502 with a `Tacita: ` message.
  - `block_probe_blocks_before_forwarding`: with `CONFIG_OFF`, the result is 400 with `Tacita: block probe`, and the upstream received 0 requests.
  - `stream_probe_ends_stream_with_error`: an `Echo` of `TACITA_STREAM_PROBE` (so the probe is split over 3-character deltas) produces `event: error` with `stream probe`.
  - `concurrent_sessions_restore_independently`: 8 concurrent `Echo` requests with the text `n{i} acmewidgets` each get back exactly `n{i} acmewidgets`.
  - `non_streaming_json_restored`: an upstream `application/json` message with `client_x01` in its text reaches the client restored.
  - `error_bodies_not_restored`: an upstream 400 body containing `client_x01` reaches the client byte-identical.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita --test proxy_scrub -q 2>&1 | tail -3`. Expected: failures or a compile error.
- [ ] **Step 3: Implement** as specified.
- [ ] **Step 4: Run them and check they pass**, then run the full suite: `cargo test -q 2>&1 | tail -2`. Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "daemon: scrub requests and restore streamed replies"`

---

### Task 12: OpenRouter route and `tacita key set openrouter`

**Files:**
- Create: `crates/tacita/tests/proxy_openrouter.rs`
- Modify: `crates/tacita/src/daemon/{mod.rs,forward.rs}`, `crates/tacita/src/main.rs`

**Interfaces:**
- Consumes: `adapt_request`, `adapt_beta`, `Prefs`, `SecretStore`, `State`.
- Produces: `pub fn openrouter_headers(incoming: &HeaderMap, key: &str) -> HeaderMap`. It is built from an allowlist:
  - only `content-type`, `accept` and `anthropic-version` are copied;
  - `anthropic-beta` is passed through `adapt_beta`;
  - `authorization` is set to `Bearer <key>`;
  - `accept-encoding` is set to `identity`.

  Nothing else is copied. In particular, no `x-claude-code-*` header goes to OpenRouter (my call: allowlist rather than denylist).

**When `state.provider = openrouter`:**
1. No `openrouter_model` → 400 with `error_body("no OpenRouter model: run `tacita use openrouter --model <slug>`")`.
2. `secrets.get("openrouter")` is `None` → 400 with `error_body("no OpenRouter key: run `tacita key set openrouter`")`. A Keychain error → 400 with `error_body("cannot read the OpenRouter key from the Keychain: <err>")`.
3. `scrub_request` → `adapt_request(slug, Prefs { only: config.openrouter.only })`. An `AdaptError` → 400 with `error_body(<err>)`.
4. `POST {openrouter_upstream}/v1/messages` with no query string. The response is handled exactly as in Task 11.

**`tacita key set openrouter`:**
- It prompts with `rpassword::prompt_password("OpenRouter key (input hidden): ")`.
- The value is trimmed and must start with `sk-or-`.
- It is stored with `KeychainStore::new("dev.tacita").set("openrouter", key)`.
- It prints `Stored in the macOS Keychain (service dev.tacita, account openrouter).`
- The key is never echoed or logged.

- [ ] **Step 1: Write the failing tests.** The state is `provider = "openrouter"`, `openrouter_model = "qwen/qwen3-coder"`, and the `MemoryStore` holds `openrouter = sk-or-v1-TEST`.
  - `openrouter_gets_its_key_never_the_anthropic_bearer`:
    - the upstream `authorization` is `Bearer sk-or-v1-TEST`;
    - no upstream header value or body contains `sk-ant`;
    - there is no `x-claude-code-*` header;
    - `anthropic-beta` equals `claude-code-20250219` (sent: `oauth-2025-04-20,claude-code-20250219`);
    - the path is `/v1/messages`.
  - `openrouter_body_adapted_and_scrubbed`: the request is the Task 5 fixture. Expected:
    - `model == "qwen/qwen3-coder"`, `provider.zdr == true`, no thinking blocks, no `metadata.user_id`;
    - `/system/2/text`, `/messages/0/content/0/text` and the tool_result text contain no `acmewidgets`;
    - the body has exactly 5 `acmewidgets` left, all in fields that are never rewritten: the two tool names, the tool_use id, the tool_use_id, and the image data.
  - `anthropic_upstream_untouched_in_openrouter_mode`
  - `missing_key_is_a_visible_error`, `missing_model_is_a_visible_error`, `unknown_model_is_a_visible_error`: each gets 400 with a `Tacita: ` message, and the upstream received 0 requests.
  - `names_restored_from_openrouter_stream`: an `Echo` round trip.
  - `keychain_round_trip`: `#[ignore]`, run by hand. It uses `KeychainStore::new("dev.tacita.test")`, and cleans up afterwards with `security_framework::passwords::delete_generic_password`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita --test proxy_openrouter -q 2>&1 | tail -3`
- [ ] **Step 3: Implement** as specified.
- [ ] **Step 4: Run them and check they pass**, plus the full suite. Expected: `ok`.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "daemon: OpenRouter route with Keychain key"`

---

### Task 13: Corpus recorder (Step 0 test 5)

**Files:**
- Create: `crates/tacita/src/daemon/record.rs`, `crates/tacita/tests/proxy_record.rs`
- Modify: `crates/tacita/src/daemon/{mod.rs,stream.rs}`, `crates/tacita/src/config.rs` (add `ConfigError::RecordDirInRepo`)

**Interfaces:**
- Produces:
  - `pub struct Recorder`
  - `Recorder::new(dir: &Path) -> io::Result<Recorder>`: creates the directory with mode 0700.
  - `pub fn save(&self, n: u64, parts: RecordParts)`
  - `pub struct RecordParts { pub headers: BTreeMap<String, String>, pub status: u16, pub request: Vec<u8>, pub forwarded: Vec<u8>, pub upstream: Vec<u8>, pub client: Vec<u8>, pub truncated: bool }`

**What it records:**
- When `step0.record_dir` is set, each Messages request writes five files, all mode 0600: `{n:06}-request.json`, `-forwarded.json`, `-upstream.txt`, `-client.txt` and `-meta.json` (headers, status, truncated).
- Header values are kept only for `anthropic-version`, `anthropic-beta`, `content-type`, `accept`, `user-agent`, `x-claude-code-session-id`, `x-claude-code-agent-id` and `x-claude-code-parent-agent-id`. Every other value is recorded as `"<redacted>"`.
- The upstream and client bytes are captured while streaming. Capture stops after 64 MiB each, with `truncated = true`. The files are written when the stream ends.
- `Config::load` refuses a `record_dir` that is inside a git work tree, meaning any ancestor that contains `.git` (`RecordDirInRepo`).
- **The one deliberate exception to "Tacita never writes a credential value to disk".** The recorder writes raw bodies, and Plan 1 has no credential layer.
  - It is opt-in, for Step 0 only.
  - It runs on a throwaway repo and stays outside any git repo.
  - The runbook makes the owner grep the corpus for secrets before keeping it.
  - Later plans run recording through the credential layer.

- [ ] **Step 1: Write the failing tests.**
  - `records_five_files_without_credentials`:
    - after one request carrying the bearer `sk-ant-oat01-TESTTOKEN`, the 5 files exist with mode 0600 (poll for up to 2 s, because they are written when the stream ends);
    - no file contains `TESTTOKEN`;
    - `meta.json` has `headers.authorization == "<redacted>"`.
  - `record_dir_inside_git_repo_is_refused`: a tempdir with `.git/` and `record_dir = <tempdir>/corpus` gives `Err(RecordDirInRepo)`.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita --test proxy_record -q 2>&1 | tail -3`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them and check they pass**, plus the full suite.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "daemon: opt-in corpus recorder outside git repos"`

---

### Task 14: Guard hook (`tacita hook pre-tool-use`)

**Files:**
- Create: `crates/tacita/src/hook.rs`, `crates/tacita/tests/hook.rs`
- Modify: `crates/tacita/src/lib.rs`, `crates/tacita/src/main.rs`

**Interfaces:**
- Consumes: `Config::load`, `Step0Cfg`.
- Produces:
  - `pub struct Guard { pub standins: Vec<String>, pub probe: Option<String> }`
  - `pub enum Decision { Allow, Deny(String) }`
  - `pub fn decide(stdin: &str, guard: &Guard) -> Decision`
  - `pub fn run() -> i32`: reads stdin, builds the `Guard` from `Config::load(paths::home())` (stand-ins from `step0.pairs`, plus `guard_probe`), prints the output, and returns the exit code.

**Rules:**
- Parse stdin as JSON and walk every string value **and key** of `tool_input`.
- A stand-in found as a substring → `Deny("Tacita blocked this <tool_name> call: it still contains the Tacita stand-in `<s>`, which must never reach your files or commands. Write the call again with the real name from the conversation.")`.
- The probe → `Deny("Tacita guard probe: this call was denied on purpose (Step 0 test 4).")`.
- Any failure (stdin is not JSON, or the config can't be read) → `Deny("Tacita guard hook could not check this call (<error>), so it was denied.")`. The hook fails closed.

**Output:**
- **Deny:** print `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<reason>"}}` on stdout and exit with 0.
- **Allow:** print nothing and exit with 0.

- [ ] **Step 1: Write the failing tests.**
  - Unit tests:
    - `allows_clean_calls`;
    - `denies_stand_in_in_value` (`{"tool_name":"Bash","tool_input":{"command":"cat client_x01/README.md"}}`);
    - `denies_stand_in_in_key`;
    - `denies_probe`;
    - `denies_on_garbage_input`.
  - `tests/hook.rs` runs the binary with `TACITA_HOME` set to a home made by `tacita init`. It pipes the JSON in and asserts that stdout parses with `hookSpecificOutput.permissionDecision == "deny"` and the exit code is 0. It also asserts that a missing `TACITA_HOME` config still gives a deny.
- [ ] **Step 2: Run them and check they fail.** Run: `cargo test -p tacita hook -q 2>&1 | tail -3`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them and check they pass**, plus the full suite.
- [ ] **Step 5: Commit.** `git add -A crates/tacita && git commit -m "hook: fail-closed PreToolUse guard"`

---

### Task 15: Step 0 runbook, results template, throwaway repo, dry run

**Files:**
- Create: `docs/step0/runbook.md`, `docs/step0/results.md`, `docs/step0/make-throwaway-repo.sh`

**Interfaces:**
- Consumes: every CLI command above.

**`make-throwaway-repo.sh <dir> <absolute path to tacita>`** (the default `<dir>` is `~/tmp/acmewidgets-test`, a path that contains the fake client name):
1. Runs `git init -q`.
2. Writes `README.md` as `# AcmeWidgets portal\n\nInternal tool for acmewidgets. Contact: Jeanne Fictive.\n` and `notes.md` as `Meeting notes for acmewidgets.\n`.
3. Writes `.claude/settings.json` with the guard hook: `{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"<tacita> hook pre-tool-use","timeout":10,"onFailure":"block"}]}]}}`.
   - Per code.claude.com/docs/en/hooks.md, a hook that crashes or times out lets the tool call through unless `onFailure` is set to `"block"`, which requires Claude Code v2.1.295 or later.
   - The hook also denies on its own internal errors (Task 14), so the protection doesn't rely on that setting alone.
4. Runs `git config user.name step0 && git config user.email step0@example.invalid` inside the repo, so Claude Code's git context shows the fake identity, then makes one commit.

The script refuses to run if `<dir>` already exists.

**`runbook.md`** gives exact commands and pass criteria, copied from spec §15 and the Max note §4:
- **Preparation:**
  - extra usage OFF at claude.ai/settings/usage, and note the plan usage %;
  - record `claude --version`;
  - `cargo build --release`, then `tacita init` and `tacita key set openrouter`;
  - run the throwaway-repo script;
  - in a split, run `tacita daemon`, plus `tail -f ~/Library/Application\ Support/Tacita/logs/requests.jsonl`.
- **How to launch Claude Code:** every Claude Code session in Step 0 is started as `ANTHROPIC_BASE_URL=$(tacita status --base-url) claude` inside the throwaway repo. Global settings stay untouched. After editing `config.toml`, restart `tacita daemon`.
- **Test 1, Max pass-through** (`anthropic.protection = "off"`):
  - Steps: run `/status` and check for the Max login row, the Tacita base URL, and no "Auth token" or "API key" row. Ask "read README.md and summarize it", then a follow-up.
  - Pass criteria:
    - every `/v1/messages` line other than the probe has status 200 (404 `refused` lines for count_tokens are expected);
    - the `ratelimit` headers are present and allowed;
    - `5h-utilization` rises;
    - there is no overage;
    - the claude.ai usage page shows plan usage, not extra usage.
  - Then type `TACITA_BLOCK_PROBE`. Claude Code must show the `Tacita: block probe…` error without retrying in a loop: the main model's request gets exactly one `tacita_error` line.
  - Claude Code keeps the failed message and would resend it. So run `/clear`, or start a new session, then check that the next prompt works.
- **Test 2, Max with scrubbing** (`anthropic.protection = "pairs"`):
  - Steps: a new session, at least 3 turns.
    - The **first prompt starts with the fake name**: `acmewidgets: read README.md and summarize it`. This touches characters 4, 7 and 20 of the first user message, which the attribution fingerprint reportedly uses (Max note §3.6). Record that first turn on its own line in the results.
    - Then the follow-up, plus "append a line `Reviewed by Jeanne Fictive for acmewidgets` to notes.md".
  - Pass criteria:
    - the same as test 1;
    - the main model's requests have `scrubbed_system > 0`, which proves the cwd in the system prompt was really rewritten;
    - no 400 "Third-party apps now draw from your extra usage";
    - no 429 without unified headers;
    - no 400 "bound to a different conversation";
    - `usage.cache_read > 0` from turn 2;
    - the reply shows `acmewidgets`;
    - `grep -rn 'client_x01\|ClientX01\|person_x01' . --exclude-dir=.git` prints nothing.
  - Then ask the model to reply with exactly `TACITA_STREAM_PROBE`. Claude Code shows the `Tacita: stream probe` error.
  - If test 2 fails: record it. The spec fallback is credentials-only on Max (Plan 2 makes it a config value).
- **Test 3, OpenRouter:**
  - **Before anything goes to OpenRouter:** Claude Code adds the owner's own context to every request: git identity, account email, macOS username in the cwd, and the global `~/.claude/CLAUDE.md`. Plan 1 only scrubs what `step0.pairs` lists, so:
    1. Add the owner's real identifiers to `step0.pairs`, each exact spelling, because Plan 1 does no case or accent folding. That means the git `user.name`, the account email, the macOS username, the company domain and the first name as written in CLAUDE.md, with stand-ins such as `person_x02`, `email_x01`, `user_x01` and `company_x01`.
    2. Set `record_dir`, restart the daemon, and run one prompt on **Anthropic** (`pairs` mode).
    3. Grep the newest `-forwarded.json` for those identifiers. Go on only when nothing is found.
    4. Start the test-3 session with `ENABLE_CLAUDEAI_MCP_SERVERS=false`, so connector tool names stay out of the request.
    5. Run only harmless Bash commands: no `env`, no reading files outside the throwaway repo. Plan 1 has no credential layer.
  - Steps: `tacita use openrouter --model <slug>`, then a new session. Do a Read, an Edit, a Bash run and a multi-turn exchange, and check the thinking display. Check the OpenRouter activity page for the provider used and ZDR. Try `claude --continue` across a switch. Finish with `tacita use anthropic`.
  - Record each slug tried, with pass or fail per capability.
  - Fallback: Claude models via OpenRouter.
- **Test 4, guard hook:**
  - Setup: set `[anthropic] protection = "off"` and restart `tacita daemon`. In `pairs` mode, Plan 1's restorer would turn `client_x01` back into the real name before the hook sees it.
  - Steps: run `claude --dangerously-skip-permissions`. Accept the workspace trust dialog, because Claude Code holds back hooks until the folder is trusted. Ask it to "run exactly: echo TACITA_GUARD_PROBE > guard-probe.txt", then "run exactly: echo client_x01 > standin-probe.txt".
  - Pass criteria: both calls are denied with Tacita's reason, and neither file exists. The docs don't say whether a deny is honoured in bypass mode, so this test is the evidence.
  - Also record which of these happens:
    - the turn ends and the reason shows as a warning line, which is the documented default;
    - the model sees the reason and retries.

    If the turn ends, rerun with `"continueOnBlock": true` added to the hook entry and record the difference. The spec wants the model to see the reason.
- **Test 5, record the corpus:**
  - Setup: set `step0.record_dir` to a folder outside any repo, for example `~/tacita-corpus`.
  - Steps: run one session covering a file read, an edit, a write, a Bash run, a grep, a subagent, a WebSearch, a pasted image, `/compact` and a thinking turn. Use only harmless commands: no `env`, no files outside the throwaway repo.
  - Afterwards, search the corpus for secrets: `grep -rlE 'sk-ant-|sk-or-|ghp_|AKIA|-----BEGIN|Bearer ' ~/tacita-corpus`. Fix or delete every hit before keeping it.
  - The corpus stays outside the repo and is never committed.

**`results.md`:**
- one table per test, with date, Claude Code version, pass or fail per criterion, and notes;
- a closing **Decisions** section: Max protection level, OpenRouter models that passed, guard hook OK, and new paths or headers seen in the log.

- [ ] **Step 1: Write** the script, the runbook and the results template as specified.
- [ ] **Step 2: Dry-run the script.** Run: `bash docs/step0/make-throwaway-repo.sh "$TMPDIR/acmewidgets-dry" "$PWD/target/release/tacita" && git -C "$TMPDIR/acmewidgets-dry" log --oneline | wc -l`. Expected: `1`. Then run `rm -rf "$TMPDIR/acmewidgets-dry"`.
- [ ] **Step 3: Dry-run the daemon.**
  - Run it as one command, so the temporary home and the background process stay in one shell:

```bash
cargo build --release -q && export TACITA_HOME=$(mktemp -d) && T=./target/release/tacita && $T init >/dev/null && \
($T daemon & pid=$!; sleep 1; U=$($T status --base-url); curl -sI "$U/api/hello" | head -1; \
 curl -s -o /dev/null -w '%{http_code}\n' -X POST "$U/v1/messages/count_tokens"; kill $pid); unset TACITA_HOME
```

  - Expected: `HTTP/1.1 200 OK`, then `404`.
  - If port 47821 is busy, edit `port` in `$TACITA_HOME/config.toml` before starting the daemon.
- [ ] **Step 4: Run the full suite and clippy.** Run: `cargo test -q 2>&1 | tail -2 && cargo clippy -q --all-targets -- -D warnings 2>&1 | tail -3`. Expected: `ok`, and no clippy output.
- [ ] **Step 5: Commit.** `git add docs/step0 && git commit -m "docs: Step 0 runbook, results template, throwaway repo script"`

---

## After this plan

The owner runs `docs/step0/runbook.md` and fills in `docs/step0/results.md`. The next plans are written from those results, roughly in this order. Each one replaces or extends pieces built here and keeps their interfaces:

1. **Protection file and matching engine** (§5, §6, §13 layer 1):
   - entries with spellings, case styles and match modes;
   - normalization;
   - injectivity checks;
   - seeding.

   It replaces `Pairs` behind `Protection`, and adds `full` and `credentials-only` per provider.
2. **Credentials** (§7, §13 layer 2): vendored gitleaks rules, context rules, local secret values, Keychain-keyed fingerprints, and same-file restore.
3. **Full pipeline** (§8, §10):
   - a snapshot per `x-claude-code-session-id`;
   - the replay cache;
   - the final check (the second lock);
   - late finds;
   - session state on disk;
   - style-change restoring.
4. **`tacita watch` and protection-file commands** (§12).
5. **`tacita setup claude`, `doctor`, the status line, `pause`/`resume`, launchd, and the full guard hook** (§1, §11).
6. **`tacita check`, structured detectors, and the 1M-token benchmark** (§8 performance, §12, §13 layer 3). Then the optional ML detector (§13 layer 4).
