# Tacita

A local privacy proxy for AI coding tools. Tacita runs on your Mac, between Claude Code and the LLM provider. It replaces the business names you list with stand-ins before a request leaves the machine, and puts the real names back, byte for byte, in the streamed reply.

```
Claude Code ──► 127.0.0.1 Tacita ──► Anthropic (your Claude Max login, passed through)
                  │                └► OpenRouter (key from the macOS Keychain)
                  ├─ scrub:   acmewidgets → client_x01
                  └─ restore: client_x01  → acmewidgets
```

> [!WARNING]
> **Tacita is experimental and not yet a privacy tool you can rely on.** The current build is the Step 0 test harness. It hides only the exact spellings you list, it does not detect credentials, and on the Anthropic route it hides nothing until you turn scrubbing on. Do not send data you need to keep private through it yet. See [Gaps in the current build](#gaps-in-the-current-build).

## Why

Coding with an LLM sends it your internal code, company and project names, client names, people's names, and sometimes credentials that show up in files or command output.

You may be comfortable sending that to Anthropic, but not to an open-weight model hosted by a provider you don't know. Tacita is meant to let you switch provider without that worry: each provider sees `client_x01` instead of your client's name, while the code logic still goes through, so the model can do useful work. Real names are restored in the reply, so the edits the model makes land on disk correctly.

Two rules shape the design:

- **No change to how Claude Code feels.** Tacita works in the background.
- **Never fake protection.** When Tacita is broken or protecting less than usual, Claude Code must show it, and every failure must fail closed with a visible error. This is the v1 goal; the current build does not fully meet it yet (see below).

## Status

Tacita is built in steps. **Step 0** is a set of live tests that check the riskiest assumptions before the full design is built on them. Its code is complete; the live tests are in progress ([runbook](docs/step0/runbook.md), [results](docs/step0/results.md)).

| | Step 0 (built) | v1 (designed, not built) |
|---|---|---|
| What gets hidden | Exact spellings listed in `config.toml` | A protection file with spellings, case, accent and Unicode variants, plus proposed new findings |
| Anthropic route by default | Forwarded untouched (`protection = "off"`) | Full protection |
| Credentials | Not detected | Always blocked, with every provider |
| Final leak check before sending | No; the log only counts listed names left in what was sent | Yes, it masks or blocks |
| Reduced protection shown in Claude Code | No | Yes, in the status line |
| Providers | Anthropic (Claude Max), OpenRouter | Same |
| Restore in streamed replies | Yes, byte-exact | Yes |
| Guard hook against stand-ins reaching your tools | Yes, exact stand-ins only | Yes |
| Live panel (`tacita watch`) | No; a JSON log of metadata | Yes |
| Claude Code setup (`tacita setup claude`) | No; manual | Yes |
| Runs as | `tacita daemon`, in the foreground | A launchd user agent |

After v1, in order: the OpenAI Chat Completions format (OpenCode, chat apps), the OpenAI Responses format (Codex CLI), then business and strategy discussions. The [design](docs/superpowers/specs/2026-10-24-tacita-design.md) has the details.

### Gaps in the current build

- **Only what you list is hidden, in exactly that spelling.** That includes what Claude Code adds to every request on its own: your git `user.name`, your account email, your macOS username (in the working-directory path) and your global `~/.claude/CLAUDE.md`. The default pairs are placeholders, so nothing real is hidden until you replace them.
- **The Anthropic route forwards everything untouched by default.** Set `[anthropic] protection = "pairs"` to scrub it. The OpenRouter route always scrubs.
- **Some parts of a request are never rewritten**, on every provider: the first two system blocks (or a system prompt sent as a plain string), thinking blocks, tool names (including MCP server names in `mcp__<server>__<tool>`), `model`, `metadata`, object keys outside tool inputs and schemas, ids, types and signatures, base64 images and PDFs, and `cache_control` markers. A real name in any of these leaves as is.
- **On the Anthropic route, request headers and the query string go out as Claude Code sent them**, never scrubbed.
- **Nothing in Claude Code tells you protection is off or incomplete.** Check with `tacita status` and in the log.

## How it works today

1. Claude Code sends each request to `http://127.0.0.1:<port>/<secret>`. Tacita accepts it only on that random secret path, with a local `Host` header and no browser headers. It answers `HEAD` and `GET /api/hello` locally, returns 404 for `count_tokens` (its body carries the whole conversation; Claude Code then estimates the count itself), and refuses every other path.
2. **Scrub** (in `pairs` mode; `off` forwards the body untouched). Tacita walks the JSON body and replaces each listed spelling with its stand-in in string values: messages, tool results, tool inputs and schemas (keys included), and the system prompt from its third block on, which holds the working-directory path. The parts listed under [Gaps](#gaps-in-the-current-build) are left alone. Every other byte is forwarded unchanged.
3. **Forward** to the active provider (see [Providers](#providers)).
4. **Restore** the streamed reply as it arrives. Text is restored on the fly. Tool calls are held until they end, then restored inside the JSON strings of their input (keys and values), so file edits land with the real names. Thinking is never restored.
5. **Fail closed** on what Tacita cannot handle safely: a request body that is not valid JSON, a provider redirect, a reply it cannot restore (an unexpected event, or a compressed stream in `pairs` mode), or a `config.toml` changed since the daemon started. Each becomes an error shown in Claude Code, which is told not to retry it. Request fields Tacita does not recognise are not refused: their string values are scrubbed and forwarded.

A **guard hook** for Claude Code (`tacita hook pre-tool-use`) denies any tool call whose input contains one of the stand-ins listed in `config.toml`, matched exactly. It covers only projects where you install it, and a variant spelling of a stand-in (`CLIENT_X01`) is not caught. If the hook itself fails, it denies the call.

Tacita writes one line of metadata per request to `logs/requests.jsonl` (mode 0600): method and path after the secret, route, status, provider, protection, model, the Claude Code session id, header names, scrub and restore counts (including `reals_left`, the number of listed real values still in the bytes sent), usage, rate-limit headers, the first 60 characters of the second system block, up to 300 characters of any upstream error message, and timing. It never logs message bodies, the secret or a token.

## Requirements

- macOS (the OpenRouter key is stored in the macOS Keychain)
- Rust, stable toolchain ([rustup](https://rustup.rs))
- Claude Code; v2.1.295 or later for the guard hook to block a call when the hook times out or fails to run

## Install

```bash
git clone https://github.com/mdelarbr/Tacita.git
cd Tacita
cargo install --path crates/tacita --locked
```

This puts `tacita` in `~/.cargo/bin`. After each rebuild, macOS may ask whether `tacita` may read its Keychain item: answer "Always Allow".

## Quick start

```bash
tacita init      # create config.toml
```

Edit `config.toml` (see [Configuration](#configuration)): replace the placeholder pairs with your real spellings, and set `[anthropic] protection = "pairs"` if you want Anthropic traffic scrubbed. Then:

```bash
tacita status    # check the provider and protection
tacita daemon    # run the proxy, in its own terminal; Ctrl-C stops it
```

In another terminal, start Claude Code through Tacita:

```bash
ANTHROPIC_BASE_URL=$(tacita status --base-url) claude
```

`ANTHROPIC_BASE_URL` is the only setting Tacita needs. Keep Claude Code's normal `/login`. Do not set `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper` or `CLAUDE_CODE_OAUTH_TOKEN`: each one turns the Max login off.

To use OpenRouter instead:

```bash
tacita key set openrouter                     # prompts for the key, input hidden
tacita use openrouter --model <model-slug>    # an OpenRouter model slug
tacita use anthropic                          # switch back
```

To install the guard hook, add it to a project's `.claude/settings.json`, with the absolute path to `tacita`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "\"/Users/you/.cargo/bin/tacita\" hook pre-tool-use",
            "timeout": 10,
            "onFailure": "block"
          }
        ]
      }
    ]
  }
}
```

## Commands

| Command | What it does |
|---|---|
| `tacita init` | Create `config.toml` with a random secret. Never overwrites an existing one. |
| `tacita status` | Show the provider, protection, base URL and whether the daemon is running. `--base-url` prints only the URL. |
| `tacita daemon` | Run the proxy on 127.0.0.1 in the foreground. Stops on Ctrl-C, SIGTERM or SIGHUP. |
| `tacita use anthropic` | Send new requests to Anthropic. |
| `tacita use openrouter --model <slug>` | Send new requests to OpenRouter, with the given model. |
| `tacita key set openrouter` | Store the OpenRouter key in the macOS Keychain (service `dev.tacita`). |
| `tacita hook pre-tool-use` | The Claude Code guard hook; reads the hook JSON on stdin. |

## Configuration

Tacita keeps its files in `~/Library/Application Support/Tacita/`, or in `$TACITA_HOME` if set and not empty. Files are created with mode 0600 and folders with 0700; Tacita also sets 0700 on its home folder if it already exists. `tacita init` writes `config.toml`:

```toml
port = 47821
secret = "<random>"

[anthropic]
protection = "off"     # "off" or "pairs"

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

- `protection = "off"` forwards the body untouched; `"pairs"` hides every `real` spelling under its `standin`. Each spelling is matched exactly, so list every form you use.
- Stand-ins have the shape `kind_xNN` (`client_x01`, `ClientX01`), which is what makes restoring unambiguous.
- The three probes are for the Step 0 tests and are on by default: any request, reply or tool call containing one is blocked on purpose. Remove them when you are done testing.
- `record_dir` turns on the traffic recorder. **It writes every request and reply in full, unscrubbed** (real names and any credentials included), to files in that folder, mode 0600. Tacita refuses a folder inside a git repo. Turn it on only for the runbook tests, then delete the folder.
- Unknown keys are refused. After editing `config.toml`, restart `tacita daemon`: a daemon started on an older config refuses every request until then, and `tacita status` says so.

## Providers

### Anthropic (Claude Max)

Claude Code keeps its own login. Tacita sends the request only to `api.anthropic.com`, with every header Claude Code sent, unchanged (apart from hop-by-hop headers, `Host` and `Content-Length`), including the `Authorization` bearer, and with the query string as sent. Headers and the query are never scrubbed. Tacita never stores, logs or reuses the token, and never refreshes it. Upstream errors and rate-limit headers pass through, so Claude Code's usual limit messages still work.

### OpenRouter

The key comes from the macOS Keychain, never from a file. Only an allowlist of headers is sent, and no query string. Tacita drops Claude Code's Anthropic credentials, sets the model you chose, removes thinking blocks and user identifiers (`metadata.user_id`, `user`, `session_id`), and adds these provider preferences to every request:

```json
{ "provider": { "data_collection": "deny", "zdr": true, "allow_fallbacks": false } }
```

plus `only` when you list trusted providers. A request that carries its own routing fields (`models`, `route`, `plugins`, `transforms`, `preset`) or a model ID that is not a Claude model is refused. Set a credit limit on the key in OpenRouter's dashboard: Tacita has no spending cap.

## Claude Max and Anthropic's terms

Routing your own, unmodified Claude Code login through a local gateway is documented by Anthropic. **Rewriting the content is a grey area**: Anthropic's gateway guidance is to inspect without modifying, and rewritten traffic could be classed as third-party, which can mean errors or extra-usage billing. Step 0 test 2 checks exactly this.

To limit the risk, Tacita stays single-user and local, never touches the first two system blocks, and never forges Claude Code's fingerprints. **Keep extra usage off** (or capped low) at [claude.ai/settings/usage](https://claude.ai/settings/usage), so a misclassification shows up as an error rather than a bill. Anthropic's Usage Policy effective 2026-11-12 adds a clause against services that route requests through consumer subscriptions; Tacita is personal and local.

There is one low-confidence report of an account suspension after routing Claude Code to a non-Claude model through a local proxy. Use Tacita at your own risk. Policy last checked: 2026-10-24.

## Out of scope, by design

Even in v1, Tacita does not protect against:

- a malicious model that tricks the agent into sending your files elsewhere; Claude Code's permission prompts remain the guard;
- malware on your Mac, or the transcripts Claude Code keeps locally;
- what a model can infer from what remains: domain logic, writing style;
- deliberate sends: images and PDFs you share, and claude.ai connector calls, which go from Claude Code to Anthropic and on to the service you connected.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The workspace has two crates:

- `crates/tacita-core`: the logic that decides what leaves the machine: byte-exact JSON scanning, the request scrub, the SSE parser and the stream restorer, the OpenRouter adapter. No I/O.
- `crates/tacita`: the CLI, the config, the Keychain, the daemon (axum, reqwest) and the guard hook.

`keychain_round_trip` is ignored by default because it touches the real Keychain. Run it with `cargo test -p tacita --test proxy_openrouter keychain_round_trip -- --ignored`.

## Documentation

- [Design](docs/superpowers/specs/2026-10-24-tacita-design.md): the full v1 design, threat model and restoring rules
- [Step 0 runbook](docs/step0/runbook.md) and [results](docs/step0/results.md)
- [Prior art](docs/research/2026-10-24-prior-art.md) and [Claude Max through Tacita](docs/research/2026-10-24-claude-max-through-tacita.md)

## License

[Apache-2.0](LICENSE). Tacita is an independent project, not affiliated with Anthropic or OpenRouter.
