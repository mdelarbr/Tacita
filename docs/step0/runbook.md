# Tacita Step 0 runbook

Five live tests, run by hand. Fill in `results.md` as you go. Spec: `docs/superpowers/specs/2026-10-24-tacita-design.md` (section 15 and the Max note, section 4).

Rules for the whole run:

- Every Claude Code session in Step 0 starts as `ANTHROPIC_BASE_URL=$(tacita status --base-url) claude`, inside the throwaway repo. This is the only setting Tacita uses. Never set `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper` or `CLAUDE_CODE_OAUTH_TOKEN`: each one switches the Max login off.
- Do not edit `~/.claude/settings.json`. The guard hook (test 4) is installed only by the throwaway repo's own `.claude/settings.json`.
- After every edit of `~/Library/Application Support/Tacita/config.toml`, stop `tacita daemon` with Ctrl-C and start it again.
- `tacita` commands that fail with a usage error (for example `tacita use openrouter` without `--model`) exit with code 2.
- Commands below are run from the repo root (`/Users/praexo/code/Tacita`) unless they say otherwise.

Reading the log. `requests.jsonl` has one JSON line per request. This helper prints the useful fields (run it from any directory):

```bash
LOG=~/Library/Application\ Support/Tacita/logs/requests.jsonl
jq -c '{n,route,status,provider,protection,model_out,scrubbed,scrubbed_system,restored,usage,tacita_error,upstream_error,rl:.ratelimit}' "$LOG" | tail -20
```

## 0. Preparation

1. Open https://claude.ai/settings/usage. Make sure extra usage is OFF. Note the plan usage percentage in `results.md`.
2. Record `claude --version` in `results.md`. The guard hook needs Claude Code v2.1.295 or later (for `"onFailure":"block"`). If yours is older, record that a hook crash or timeout would let the tool run.
3. Build and install `tacita` so it is on the PATH in every shell and split (it installs to `~/.cargo/bin/tacita`; this runbook uses the installed copy everywhere, because an `export PATH=...` would only reach one shell). `jq` is also needed (`command -v jq`; `brew install jq` if missing).
   ```bash
   cargo build --release
   cargo install --path crates/tacita --locked --force
   command -v tacita
   ```
   If the code changes, run the two cargo commands again and restart the daemon: a stale installed copy is what runs. After every build or install, macOS may ask whether `tacita` may read the Keychain item. Answer "Always Allow" for that build, and note in `results.md` whether it asked.
4. One check of the real Keychain path (uses the service `dev.tacita.test` and cleans up after itself):
   ```bash
   cargo test -p tacita --test proxy_openrouter keychain_round_trip -- --ignored
   ```
   It must pass. Record the result.
5. Create the config and store the OpenRouter key:
   ```bash
   tacita init
   tacita key set openrouter
   ```
   `tacita key set openrouter` prompts for the key with the input hidden and prints `Stored in the macOS Keychain (service dev.tacita, account openrouter).`
6. Make the throwaway repo (its path contains the fake client name; the script refuses a directory that already exists):
   ```bash
   docs/step0/make-throwaway-repo.sh ~/tmp/acmewidgets-test "$(command -v tacita)"
   ```
   The script needs an absolute path to an executable `tacita`; `command -v` gives `~/.cargo/bin/tacita` expanded. The hook in the repo calls that same copy.
7. In a terminal split, start the daemon. It prints `Tacita listening on http://127.0.0.1:47821/<secret>` and runs in the foreground:
   ```bash
   tacita daemon
   ```
   If it says `port 47821 is in use`, edit `port` in `config.toml` and start it again.
8. In a second split, follow the log:
   ```bash
   tail -f ~/Library/Application\ Support/Tacita/logs/requests.jsonl
   ```
9. In a third split, work in the throwaway repo: `cd ~/tmp/acmewidgets-test`. Check `tacita status`: it prints the lines `provider:`, `protection:`, `base URL:` and `daemon:`; `daemon:` must say `running`.

## Test 1. Max pass-through (`anthropic.protection = "off"`)

Setup: in `config.toml` the default is `[anthropic] protection = "off"`. Check with `tacita status` (`protection: off`, `provider: anthropic`). If you changed it, restart the daemon.

Steps, in the throwaway repo:

1. `ANTHROPIC_BASE_URL=$(tacita status --base-url) claude`
2. Run `/status`. Check: the Max login row is shown, the base URL is the Tacita one, and there is no "Auth token" row and no "API key" row.
3. Ask: `read README.md and summarize it`. Then ask a follow-up question.
4. Probe: type `TACITA_BLOCK_PROBE`. Claude Code must show the error `Tacita: block probe: this request was blocked on purpose (Step 0)` and must not retry in a loop.
5. Claude Code keeps the failed message and would resend it with the next prompt. Run `/clear` (or start a new session), then check that the next prompt works.

Pass criteria (tick in `results.md`):

- Every `/v1/messages` line other than the block-probe line has `status` 200. The `route` is `messages`. Lines with `route` `count_tokens` and status 404 are expected (the message is `count_tokens is not forwarded`); so are lines with `route` `hello` (the `HEAD /api/hello` check).
- The `ratelimit` object is present on those lines and `anthropic-ratelimit-unified-status` is `allowed`.
- `anthropic-ratelimit-unified-5h-utilization` rises over the session.
- No overage: no `anthropic-ratelimit-unified-overage-*` value says extra usage was used, and the claude.ai usage page shows plan usage, not extra usage.
- Probe: exactly one line has `"tacita_error":"block probe"` and status 400 for the probe prompt, and Claude Code did not loop. Count:
  ```bash
  grep -c '"tacita_error":"block probe"' ~/Library/Application\ Support/Tacita/logs/requests.jsonl
  ```
  Compare with the count before the probe.
- After `/clear`, the next prompt gets a normal status 200.

## Test 2. Max with scrubbing (`anthropic.protection = "pairs"`)

Setup: in `config.toml` set `[anthropic] protection = "pairs"`. Restart the daemon. `tacita status` must say `protection: pairs`. The default `step0.pairs` are `acmewidgets` -> `client_x01`, `AcmeWidgets` -> `ClientX01`, `Jeanne Fictive` -> `person_x01`.

Steps: a new session in the throwaway repo (`ANTHROPIC_BASE_URL=$(tacita status --base-url) claude`), at least 3 turns.

1. The first prompt starts with the fake name: `acmewidgets: read README.md and summarize it`. This touches characters 4, 7 and 20 of the first user message, which the attribution fingerprint reportedly uses (Max note, section 3.6). Record this first turn on its own line in `results.md`.
2. Then a follow-up question.
3. Then: `append a line "Reviewed by Jeanne Fictive for acmewidgets" to notes.md`.
4. Long tool-call hold (stall watchdog check): ask the model to Write one long file, for example `write a 2,000-line generated file big.txt`. Watch the log and the client. Record whether Claude Code aborts with a stall or timeout during the hold. If the hold was too short to tell, say so. (Claude Code's watchdog fires after about 300 s; Tacita sends pings every 10 s and holds a tool call for at most 480 s. The question is whether the pings count as activity.)
5. Probe: ask the model to reply with exactly `TACITA_STREAM_PROBE`. Claude Code must show the error `Tacita: stream probe: this reply was stopped on purpose (Step 0)`.

Pass criteria:

- The same as test 1 (status 200, `ratelimit` allowed, 5h-utilization rises, no overage).
- The main model's lines have `scrubbed_system` greater than 0. This proves the working directory in the system prompt was really rewritten.
- None of these appears: a 400 "Third-party apps now draw from your extra usage"; a 429 without `anthropic-ratelimit-unified-*` headers; a 400 "bound to a different conversation".
- `usage.cache_read` is greater than 0 from turn 2.
- The reply shows `acmewidgets` (restored), not `client_x01`.
- Nothing in the repo holds a stand-in. This must print nothing:
  ```bash
  grep -rn 'client_x01\|ClientX01\|person_x01' . --exclude-dir=.git
  ```
- The stream probe shows the `Tacita: stream probe` error and the line has `tacita_error` starting with `stream probe`.

Three live checks the code cannot prove. Record each in `results.md`:

- (a) Anthropic honours `accept-encoding: identity` on SSE. If every protected streamed reply comes back as a Tacita 502 with `compressed stream from upstream; refusing to pass it on unrestored` (in `tacita_error`), it does not.
- (b) The ~300 s stall watchdog counts Tacita's pings as activity (step 4 above).
- (c) `usage.cache_read > 0` from turn 2.

If test 2 fails: record it. The spec fallback is credentials-only on Max (Plan 2 makes it a config value).

## Test 3. OpenRouter

Claude Code adds your own context to every request: the git identity, the account email, the macOS username in the working directory, and the global `~/.claude/CLAUDE.md`. Plan 1 only scrubs what `step0.pairs` lists, so do this first, before anything goes to OpenRouter.

1. Add your real identifiers to `step0.pairs` in `config.toml`, each exact spelling, because Plan 1 does no case or accent folding. Add: the git `user.name`, the account email, the macOS username, the company domain, and the first name as written in your `~/.claude/CLAUDE.md`. Use stand-ins such as `person_x02`, `email_x01`, `user_x01`, `company_x01`. Example line: `{ real = "<exact spelling>", standin = "email_x01" },`. Keep the three default pairs.
2. Set `record_dir` to its own folder, separate from the test-5 corpus, absolute and outside any git repo: `record_dir = "/Users/<you>/tacita-precheck"` (the daemon refuses to start inside a git work tree). Set `[anthropic] protection = "pairs"`. Restart the daemon. In the throwaway repo, run ONE prompt on Anthropic, started with the same prefix as the test-3 session so the request shape matches:
   ```bash
   ENABLE_CLAUDEAI_MCP_SERVERS=false ANTHROPIC_BASE_URL=$(tacita status --base-url) claude
   ```
3. Check the WHOLE newest run, not one file (the newest file can be a small auxiliary request, such as a title request, without CLAUDE.md or git context). Give `grep -lF` every real spelling you put in `step0.pairs`, one `-e` each (`-F`: dots in an email or domain are not wildcards):
   ```bash
   D=$(ls -dt ~/tacita-precheck/run-* | head -1)
   grep -lF -e 'SPELLING1' -e 'SPELLING2' -e 'SPELLING3' "$D"/*-forwarded.json
   ```
   Go on only when this prints nothing. If it prints file names, add the missing spelling to `step0.pairs`, restart the daemon and repeat with a new prompt (each run is a new `run-<ms>` folder). Each daemon run writes into `<record_dir>/run-<start Unix ms>/`, and request numbers restart at 1 per run. Requests refused before their body is read (for example a 413) are not recorded.
   Then delete the pre-check folder, because its `-request.json` files hold your unscrubbed identifiers, and remove or comment out `record_dir` in `config.toml` (test 5 sets its own); restart the daemon:
   ```bash
   rm -rf ~/tacita-precheck
   ```
4. Start the test-3 session with connector tool names kept out of the request:
   ```bash
   ENABLE_CLAUDEAI_MCP_SERVERS=false ANTHROPIC_BASE_URL=$(tacita status --base-url) claude
   ```
5. Run only harmless Bash commands. No `env`, no reading files outside the throwaway repo. Plan 1 has no credential layer.

Steps:

1. In the second shell: `tacita use openrouter --model <slug>` (a usage error without `--model`, exit code 2). It prints the status block; check `provider: openrouter (model <slug>)`. The daemon reads `state.toml` for each request, so no restart is needed.
2. Start a new session (command in item 4 above). Do a Read, an Edit, a Bash run, and a multi-turn exchange. Check the thinking display.
3. Open the OpenRouter activity page. Check the provider used and that zero data retention (ZDR) applied.
4. Try `claude --continue` (with the same `ENABLE_CLAUDEAI_MCP_SERVERS=false ANTHROPIC_BASE_URL=...` prefix) across a switch of provider: `tacita use anthropic`, then `claude --continue`, and back.
5. Finish with `tacita use anthropic`.

Record each slug tried, with pass or fail per capability (Read, Edit, Bash, multi-turn, thinking, `--continue`).

If every OpenRouter reply ends with `Tacita: unexpected event from upstream`, record it and the slug. It likely means OpenRouter does not give its SSE events names matching their data `type`; the restorer fails closed on that by design.

Fallback: Claude models via OpenRouter.

## Test 4. Guard hook

Setup: set `[anthropic] protection = "off"` and restart `tacita daemon`. In `pairs` mode the restorer would turn `client_x01` back into the real name before the hook sees it. The hook is the one in the throwaway repo's `.claude/settings.json` (written by the script, with `"onFailure":"block"`). Do not touch `~/.claude/settings.json`.

Steps, in the throwaway repo:

1. `ANTHROPIC_BASE_URL=$(tacita status --base-url) claude --dangerously-skip-permissions`
2. Accept the workspace trust dialog: Claude Code holds back hooks until the folder is trusted.
3. Ask: `run exactly: echo TACITA_GUARD_PROBE > guard-probe.txt`
4. Ask: `run exactly: echo client_x01 > standin-probe.txt`

Pass criteria:

- Both calls are denied. The first with `Tacita guard probe: this call was denied on purpose (Step 0 test 4).` The second with:
  ```
  Tacita blocked this Bash call: it still contains the Tacita stand-in `client_x01`, which must never reach your files or commands. Write the call again with the real name from the conversation.
  ```
- Neither file exists: `ls guard-probe.txt standin-probe.txt` fails for both. The docs do not say whether a deny is honoured in bypass mode, so this test is the evidence.

Also record which of these happens:

- The turn ends and the reason shows as a warning line (the documented default).
- The model sees the reason and retries.

If the turn ends, add `"continueOnBlock": true` to the hook entry in the throwaway repo's `.claude/settings.json` (next to `"onFailure":"block"`), rerun both asks in a new session, and record the difference. The spec wants the model to see the reason.

The hook handles every failure it can see itself (it exits 0 with a deny, or exits 2). A crash by signal (out of memory, stack overflow) or a timeout is covered only by `"onFailure":"block"`, which needs Claude Code v2.1.295 or later. Record the version.

## Test 5. Record the corpus

Setup: set `step0.record_dir` to an absolute folder outside any git repo, for example `~/tacita-corpus` written as `/Users/<you>/tacita-corpus`. Restart the daemon. Each run writes `run-<start Unix ms>/` holding, per request number `NNNNNN` (six digits): `NNNNNN-request.json`, `NNNNNN-forwarded.json`, `NNNNNN-upstream.txt`, `NNNNNN-client.txt`, `NNNNNN-meta.json`. Use `[anthropic] protection = "pairs"` so the corpus shows scrubbed requests.

Steps: one session in the throwaway repo covering: a file read, an edit, a write, a Bash run, a grep, a subagent, a WebSearch, a pasted image, `/compact`, and a thinking turn. Use only harmless commands: no `env`, no files outside the throwaway repo.

Afterwards, search the corpus for secrets (the recorder keeps real request bodies, so this matters):

```bash
grep -rlE 'sk-ant-|sk-or-|ghp_|AKIA|-----BEGIN|Bearer ' ~/tacita-corpus
```

Also search it for your real identifiers from the test-3 pairs (`-F`, one `-e` per spelling). These files are the scrubbed copies, but `-request.json` holds the original, so hits there are expected: they are why the corpus must stay private. Hits in `-forwarded.json` or `-client.txt` are not expected:

```bash
grep -rlF -e 'SPELLING1' -e 'SPELLING2' ~/tacita-corpus
```

Fix or delete every hit before keeping the corpus. The corpus stays outside the repo and is never committed.

## Finish

Fill in the Decisions section of `results.md`: the Max protection level, the OpenRouter models that passed, whether the guard hook is OK, and any new paths or headers seen in the log:

```bash
jq -r '[.method,.path,.route,.status]|@tsv' ~/Library/Application\ Support/Tacita/logs/requests.jsonl | sort | uniq -c
jq -r '.header_names[]' ~/Library/Application\ Support/Tacita/logs/requests.jsonl | sort | uniq -c
```

Then `tacita use anthropic`, set `[anthropic] protection = "off"` if you want the default back, and stop the daemon with Ctrl-C (it prints `Tacita stopped`).
