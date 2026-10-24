# Tacita Step 0 results

Fill in during the run (see `runbook.md`). Write PASS, FAIL or n/a, and a note for each line.

## Environment

| Item | Value |
| --- | --- |
| Date | |
| `claude --version` | |
| macOS version / CPU | |
| Plan usage % before | |
| Extra usage OFF confirmed | |
| Keychain round-trip test (`keychain_round_trip`) | |
| Keychain "Always Allow" prompt shown for this build? | |
| Claude Code at least v2.1.295 (`onFailure: block`) | |

## Test 1. Max pass-through (protection off)

| Date | Claude Code version | |
| --- | --- | --- |
| | | |

| Criterion | Result | Notes |
| --- | --- | --- |
| `/status`: Max login row, Tacita base URL, no "Auth token" / "API key" row | | |
| Every `/v1/messages` line has status 200 (count_tokens 404 expected) | | |
| `ratelimit` headers present, `unified-status` allowed | | |
| `5h-utilization` rises | | |
| No overage; claude.ai shows plan usage, not extra usage | | |
| `TACITA_BLOCK_PROBE`: error shown, exactly one `block probe` line, no retry loop | | |
| After `/clear` the next prompt works | | |

## Test 2. Max with scrubbing (protection pairs)

| Date | Claude Code version | |
| --- | --- | --- |
| | | |

First turn (`acmewidgets: read README.md and summarize it`), on its own line: 

| Criterion | Result | Notes |
| --- | --- | --- |
| Same as test 1 (200, allowed, 5h rises, no overage) | | |
| `scrubbed_system > 0` on the main model's requests | | |
| No 400 "Third-party apps now draw from your extra usage" | | |
| No 429 without `unified` headers | | |
| No 400 "bound to a different conversation" | | |
| `usage.cache_read > 0` from turn 2 | | |
| Reply shows `acmewidgets` | | |
| `grep -rn 'client_x01\|ClientX01\|person_x01' . --exclude-dir=.git` prints nothing | | |
| `TACITA_STREAM_PROBE`: `Tacita: stream probe` error shown | | |
| (a) Anthropic honours `accept-encoding: identity` on SSE (no 502 "compressed stream from upstream") | | |
| (b) Stall watchdog (~300 s) counts pings as activity during a long Write hold (say if the hold was too short to tell) | | |
| (c) `usage.cache_read > 0` from turn 2 | | |

If test 2 failed, fallback: credentials-only on Max. Noted: 

## Test 3. OpenRouter

| Date | Claude Code version | |
| --- | --- | --- |
| | | |

Pre-flight:

| Step | Result | Notes |
| --- | --- | --- |
| Own identifiers added to `step0.pairs` | | |
| One Anthropic prompt recorded; newest `-forwarded.json` has none of them | | |
| Session started with `ENABLE_CLAUDEAI_MCP_SERVERS=false` | | |
| Only harmless Bash commands used | | |

One row per slug tried:

| Slug | Read | Edit | Bash | Multi-turn | Thinking display | `--continue` across a switch | Provider / ZDR on activity page | "unexpected event from upstream" on every reply? |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| | | | | | | | | |

Fallback (Claude models via OpenRouter) tried: 

## Test 4. Guard hook

| Date | Claude Code version | |
| --- | --- | --- |
| | | |

| Criterion | Result | Notes |
| --- | --- | --- |
| Protection off during the test | | |
| Workspace trust dialog accepted | | |
| `TACITA_GUARD_PROBE` call denied with the guard-probe reason | | |
| `client_x01` call denied with the stand-in reason | | |
| `guard-probe.txt` does not exist | | |
| `standin-probe.txt` does not exist | | |
| Deny honoured in `--dangerously-skip-permissions` mode | | |
| What happened: turn ended with a warning line / model saw the reason and retried | | |
| With `"continueOnBlock": true`: difference | | |

## Test 5. Corpus

| Date | Claude Code version | |
| --- | --- | --- |
| | | |

| Item | Done | Notes |
| --- | --- | --- |
| `record_dir` outside any repo | | |
| File read | | |
| Edit | | |
| Write | | |
| Bash run | | |
| Grep | | |
| Subagent | | |
| WebSearch | | |
| Pasted image | | |
| `/compact` | | |
| Thinking turn | | |
| Secret grep (`sk-ant-`, `sk-or-`, `ghp_`, `AKIA`, `-----BEGIN`, `Bearer `) found nothing, or hits fixed / deleted | | |
| Corpus kept outside the repo, not committed | | |

## Decisions

- Max protection level (pairs / credentials-only / off): 
- OpenRouter models that passed: 
- Guard hook OK (and `continueOnBlock` needed?): 
- New paths or headers seen in the log: 
- Other findings: 
