# Tacita: using the Claude Max subscription through the proxy

Research synthesis, written 2026-10-24. Sources are numbered `[S#]`, and the table at the end gives each URL and date. Most official Claude Code doc pages carry no date. They were fetched on 2026-10-24 and may change without notice. Anthropic changed its rules on subscription use at least five times in 2026 (Jan, Feb, Apr, Jun, Oct), and a new Usage Policy takes effect on 2026-11-12. Re-check before relying on anything below, and record a "policy last checked" date in Tacita's docs.

## 0. Short answer

**No. Tacita does not need `claude setup-token` or any other Claude key.** Keep the normal `claude` / `/login` (claude.ai, Max) login and set **only** `ANTHROPIC_BASE_URL`. Claude Code then sends its own subscription token to Tacita, and Tacita forwards it unchanged. Max limits and billing still apply [S2][S4][S16][S17][S18].

**Correction to earlier research:** remove the advice to set `ANTHROPIC_AUTH_TOKEN=<local Tacita token>`. Any credential variable replaces the claude.ai login, so the subscription limits stop applying and Max is switched off [S1][S2][S4].

---

## 1. How subscription auth reaches a proxy today

### 1.1 Credential precedence (highest first) [S1]
1. Cloud-provider variables (`CLAUDE_CODE_USE_BEDROCK` / `VERTEX` / `FOUNDRY`)
2. `ANTHROPIC_AUTH_TOKEN`, sent as `Authorization: Bearer`
3. `ANTHROPIC_API_KEY`, sent as `x-api-key` (interactive use asks for approval once)
4. `apiKeyHelper`
5. `CLAUDE_CODE_OAUTH_TOKEN`, the token from `claude setup-token`
6. Anthropic profile / federation credentials
7. Subscription OAuth from `/login`, stored in the macOS Keychain

A signed-in "Claude apps gateway" session sits outside this list [S1].

Consequences:
- Setting anything from 2 to 5 overrides the Max login. With `ANTHROPIC_AUTH_TOKEN` the override is immediate, and `/status` then shows an "Auth token" row in place of "Login method" [S5].
- A leftover `CLAUDE_CODE_OAUTH_TOKEN` in the shell or in settings `env` also overrides `/login` in every new session [S1].

### 1.2 Base-URL-only mode (the supported recipe)
- Official text: "Setting only that variable, without a gateway credential, doesn't replace the subscription. Requests still route through the gateway, but a saved claude.ai login remains the active credential, so its usage limits and billing apply. Gateways that pass this traffic on to Anthropic must forward the OAuth capability in `anthropic-beta`." [S2]
- The gateways page says the same thing in reverse: setting `ANTHROPIC_AUTH_TOKEN` for your own gateway "turns off subscription login for that session. The exception is setting only `ANTHROPIC_BASE_URL`." [S4]
- Three commercial gateways document this exact pass-through and say they store nothing:
  - LiteLLM (v1.81.14+) [S16]
  - Tailscale Aperture, last validated 2026-09-28 [S17]
  - Databricks, updated 2026-09-11 [S18]

### 1.3 Exact headers on the wire (Claude Code -> Tacita)
| Header | Value / rule | Source |
|---|---|---|
| `Authorization` | `Bearer sk-ant-oat01-…`, the subscription access token. The refresh token (`sk-ant-ort01-…`) never goes to the proxy. | LiteLLM [S16]; token prefix is community-reported |
| `anthropic-beta` | Carries the "OAuth capability". Community sources give the value as `oauth-2025-04-20`, which is **not** in Anthropic's official beta list. Forward the whole header verbatim and never allowlist values. Officially, stripping it gives 401. #87420 measured no difference on direct curl calls. | [S3][S16][S25] |
| `anthropic-version` | Forward unchanged. | [S3] |
| `x-claude-code-session-id`, agent-id, parent-agent-id | For the gateway's own use. A natural key for per-session scrub dictionaries. | [S3] |
| Custom headers | `ANTHROPIC_CUSTOM_HEADERS="Name: Value"` (use `\n` between pairs in settings JSON). A custom header with a non-empty value replaces a built-in header of the same name, so never name it `Authorization` or `x-api-key`. | [S3][S4] |

Body field: `metadata.user_id` is a JSON string `{"device_id","account_uuid","session_id"}` [S32].

### 1.4 What does NOT go through Tacita
- **Token exchange, refresh and revocation go directly to Anthropic**, not to `ANTHROPIC_BASE_URL`. The network-config page names `platform.claude.com` [S6]. Aperture's docs say `api.anthropic.com` [S17]. Either way, Tacita never refreshes and never needs to. Do not refresh in Tacita: two processes rotating the same refresh token race, and Claude Code then clears the login [S31].
- These calls still go **directly to `api.anthropic.com`**:
  - the WebFetch hostname preflight, which sends only the hostname and is not stopped by `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`; the only opt-out is `skipWebFetchPreflight: true` [S7]
  - the fast-mode check [S3][S5]
  - feature flags and telemetry [S6][S5]
- Surfaces that **ignore** `ANTHROPIC_BASE_URL` and so bypass Tacita:
  - the Claude Desktop app, cloud sessions and Slack [S5]
  - Remote Control, which is disabled whenever the base URL is not `api.anthropic.com` [S8]
  - In the VS Code extension, `claudeCode.environmentVariables` is the reliable place to set the variable [S5].
- Set the variable in the `env` block of `~/.claude/settings.json`, not as a shell export. The background-agent supervisor inherits only the environment of the first shell that started it [S6]. A settings-based base URL also **fails closed**: if Tacita is down, Claude Code gets "connection refused" instead of sending unscrubbed traffic (pattern from llm-redact-proxy [S20]).

### 1.5 `claude setup-token` facts
- Runs the same browser flow as `/login` and prints a **one-year** OAuth token. The token is **not saved** anywhere [S1].
- Used only through `CLAUDE_CODE_OAUTH_TOKEN`. Needs a Pro, Max, Team or Enterprise plan [S1].
- **Model requests only**: no Remote Control, no claude.ai connectors. `--bare` mode does not read it [S1][S8].
- Meant for "CI pipelines and scripts where browser login isn't available" [S1].
- Community reports, not official [S28]:
  - The token has only the `user:inference` scope.
  - It gets a 403 on `/api/oauth/usage` because it lacks `user:profile`.
  - It cannot read plan entitlements; Max users were shown a usage-credits prompt.
- **Conclusion:** interactive use on the laptop does not need it. Use it only for headless runs without Keychain access, and even then keep it in Claude Code's own environment and let Tacita pass it through.

---

## 2. Candidate designs

### Design A: pass-through of Claude Code's own login (RECOMMENDED)

**How it works**
- Claude Code keeps its `/login` token in the Keychain and sends it with every `/v1/messages` call to `http://127.0.0.1:<port>[/<secret-path>]`.
- Tacita scrubs the body, then forwards `Authorization`, every `anthropic-*` header and `x-claude-code-*` to `https://api.anthropic.com`.
- On the way back it restores names in the stream and passes response headers and error bodies through unchanged [S2][S3].
- Tacita never persists, logs, caches or reuses the bearer token, and never sends it to any other host or for Codex traffic. Also redact `sk-ant-` strings from Tacita's own logs [S9][S16].

**Local authentication** (needed mainly because Tacita will hold the OpenRouter key)
- Bind to 127.0.0.1 only.
- Put a random secret path prefix in `ANTHROPIC_BASE_URL`. Path prefixes work, as OpenRouter's `/api` base shows [S37].
- Optionally also send `X-Tacita-Token` through `ANTHROPIC_CUSTOM_HEADERS`; Tacita checks it and strips it.
- Do not rely on the custom header alone: some plugin or hook code that calls the API does not send custom headers (claude-code #99857, cited in research).

**One-time user steps**
1. Run `claude`, then `/login` with the Max account (already done).
2. Make sure `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper` and `CLAUDE_CODE_OAUTH_TOKEN` are not set anywhere: shell rc files, `~/.claude/settings.json`, project settings [S1].
3. In `~/.claude/settings.json`:
   - under `env`: `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>`, plus optional `ANTHROPIC_CUSTOM_HEADERS`
   - `"skipWebFetchPreflight": true` [S7]
   - `DISABLE_TELEMETRY=1`
   - optionally `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, which also turns off auto-update, feature flags and fast mode [S5]
   - `ENABLE_CLAUDEAI_MCP_SERVERS=false` if claude.ai connectors aren't used
4. Run `/status`. It must show an "Anthropic base URL" row pointing at Tacita and a "Login method" row naming the Claude Max account, with **no** "Auth token" or "API key" row [S5].
5. At claude.ai/settings/usage, set extra usage OFF or to a low cap, at least while testing. If Anthropic misclassifies the traffic or the plan runs out, you then get an error instead of quiet API-rate charges [S14][S21][S22].
6. For privacy, turn "help improve Claude" off at claude.ai/settings/data-privacy-controls. When it is on, consumer conversations can be used for training and kept for 5 years; when it is off, they are kept for 30 days [S7].

**What can break**
- Body rewriting can break the identity or attribution checks or the thinking-block binding. See section 3.
- Features you lose:
  - Remote Control [S8]
  - fast mode, if nonessential traffic is disabled [S5]
  - possibly the in-CLI usage meters. #87876 reports `rate_limits: null`, but its repro used `ANTHROPIC_AUTH_TOKEN`, so it does **not** test design A [S27]. The official doc says Claude Code reads the unified headers through a gateway [S3].
  - gateway model discovery under OAuth pass-through [S42]
- Server-managed settings are not fetched behind a custom base URL (research note citing the llm-gateway-connect page and CHANGELOG).
- Every Claude Code update can change headers, betas or the body shape, as happened to claude-code-router and CLIProxyAPI [S23][S39]. Run a regression test after each update.

**ToS risk: routing ALLOWED, rewriting GREY. Overall LOW-TO-MEDIUM.**
- *Explicitly allowed:*
  - an end user "signing in to the unmodified Claude Code binary with their own Claude subscription" [S9]
  - routing that login through a gateway with only `ANTHROPIC_BASE_URL` set, which is documented [S2]
- The legal page's bans ("route requests through Free, Pro, or Max plan credentials on behalf of their users", "collect, store, or intermediate Claude.ai credentials or session tokens") are addressed to "developers building products or services" [S9]. A personal single-user proxy is not clearly in scope. Still, a relay that carries the bearer token on every request could be read as "intermediating" it, so treat "pass-through is compliant" as an interpretation, not a fact.
- *Grey:* rewriting the prompt content. No Anthropic text allows or forbids it.
  - The gateway guide's technical advice is "inspect without modifying" [S3].
  - The preserved-thinking guide says a proxy's "own rewrites count as edits" [S12].
  - The practical risks are misclassification as a "third-party app" (400 or extra-usage billing) and automated abuse filters.
  - Precedents: false-positive bans in January 2026 [S36]; the official VS Code extension misclassified in April 2026 [S30].
- Keep Tacita **single-user and local**. The Consumer Terms forbid sharing account credentials and "bypassing any of our systems or protective measures" [S10]. The Usage Policy effective 2026-11-12 (not yet in force today) forbids "Resell, proxy, or otherwise provide access to Claude through unauthorized means, including services that route requests through consumer subscriptions or misrepresent the product or client being used" [S11].

**Detecting "Max quota exhausted" and switching to OpenRouter**
Tacita reads the response headers. Pattern from better-ccflare [S21] and TeamClaude [S22]:

| Signal | Meaning | Action |
|---|---|---|
| 429 with `anthropic-ratelimit-unified-status: rejected`, or any `anthropic-ratelimit-unified-*-status: rejected` (5h, 7d, per-model). better-ccflare saw a spent window that carried only `-5h-status: rejected`, so prefix-scan all headers. | Plan window spent | **Fail over to OpenRouter.** Record the reset time from the `-reset` headers. Keep the session on OpenRouter until then (sticky) to avoid flapping and repeated cache misses. |
| `-5h-utilization` / `-7d-utilization` at about 0.98 | Nearly spent | Optional early switch |
| 200 with `anthropic-ratelimit-unified-overage-in-use: true`, or an `-overage-status` showing extra usage in use | The plan is spent and extra usage is now billing, **or** the traffic was misclassified | Alert the user. Fail over only if the plan headers also say rejected. |
| 400 containing "Third-party apps now draw from your extra usage" | The classifier flagged Tacita's traffic as non-Claude-Code | **Stop and alert. Do not fail over silently.** Offer design C. |
| 429 **without** unified headers and without retry-after | A throttle, or the system-prompt identity check failing | Pass it through and do not switch. Log a warning, because Claude Code v2.1.199+ retries these automatically and can loop [S40][S25]. |
| 429 with a short `retry-after` and rate-limit headers | Short throttle | Pass it through. Claude Code retries; a retry-after above 60 s stops the retries [S3]. |
| 401 ("OAuth access token is invalid") | Token expired or revoked | Pass it through so the user re-runs `/login`. Never fail over on auth errors [S16]. |

Quota header names [S27][S3][S40]: `-status` (allowed / allowed_warning / rejected), `-reset`, `-representative-claim` (e.g. five_hour), `-5h-status`, `-5h-utilization`, `-5h-reset`, `-7d-*`, `-overage-status`, `-overage-disabled-reason`. Pass all of them through unchanged when not failing over, so Claude Code can show its own limit messages [S3].

**On failover, rebuild the request for OpenRouter** [S37][S12]:
- Set `Authorization: Bearer <OpenRouter key from Keychain>`. Never send the `sk-ant-oat` token to OpenRouter.
- Remove the OAuth beta and any betas OpenRouter rejects.
- Map model IDs to OpenRouter slugs (e.g. `anthropic/claude-opus-5`).
- Strip `thinking` / `redacted_thinking` blocks. Sonnet 5.5 and Haiku 5.5 thinking blocks are bound to the account that produced them, and every thinking block is bound to its conversation.
- Drop `metadata.user_id`.
- POST to `https://openrouter.ai/api/v1/messages`, with the same scrubbing applied.
- Expect a cold prompt cache.

**Policy on the fallback target**
- Anthropic "doesn't support routing Claude Code to non-Claude models through any gateway" [S3].
- One low-confidence report: an account was suspended within seconds after unmodified Claude Code was routed through a local proxy to GPT-5.6 (2026-08), then restored on appeal [S38].
- Lowest risk: fail over to **Claude models on OpenRouter**. If non-Claude models are wanted, run them in a separate Claude Code profile (`CLAUDE_CONFIG_DIR=~/.claude-tacita-openrouter`) with no claude.ai login.
- Lowest-*policy*-risk overflow overall: Anthropic "usage credits" (extra usage, billed at API rates up to a monthly cap). It keeps the same login path [S14].

### Design B: Tacita stores a setup-token in the macOS Keychain and injects it (DO NOT BUILD)

**How it works**
- The user runs `claude setup-token` once and puts the token in Keychain under Tacita.
- Claude Code must then authenticate to Tacita with something else, such as `ANTHROPIC_AUTH_TOKEN=<local>`, which switches Claude Code into non-subscription mode [S1][S4].
- Tacita replaces `Authorization` with the stored token and must add the OAuth beta itself.

**One-time user steps:** run `claude setup-token`, store the token in Keychain, set the local credential variable, and renew yearly.

**What breaks**
- Claude Code no longer knows it is on a subscription: no plan-limit handling, no entitlements, no usage meters [S2].
- The token has inference-only scope [S28].
- Tacita has to imitate Claude Code's request shape, including betas. claude-code-router broke by replacing `anthropic-beta` [S39].
- A one-year credential now sits inside Tacita.

**ToS risk: HIGH (close to forbidden)**
- It conflicts directly with "may not collect, store, or intermediate Claude.ai credentials or session tokens" [S9], and it matches the token-holding router pattern Anthropic targets.
- Precedent: #102967 (2026-10-17, one user report). A local credential proxy injected a setup-token for unmodified interactive Claude Code. The Max 20x account was revoked about 31 minutes after purchase for "suspicious signals". The appeal was rejected and no refund was given [S29].
- It gains nothing over design A.

**Quota detection:** the same unified headers, but Claude Code itself would not interpret them as plan limits.

### Design C: Anthropic Console API key (pay-as-you-go)

**How it works**
- Tacita holds a Console API key in Keychain, and Claude Code authenticates to Tacita with a local `ANTHROPIC_AUTH_TOKEN`. This is the earlier recipe, which is fine for **this** mode.
- Alternatively, Claude Code sends `ANTHROPIC_API_KEY` itself and Tacita passes it through.
- Max quota is not used.

**One-time user steps:** create a Console organization and key, buy credits, store the key in Keychain.

**What breaks / cost**
- Every token is billed.
- The API credits included with Max ($100/month on Max 5x, $200/month on Max 20x) explicitly **do not cover Claude Code**. A credits-only organization running Claude Code gets "Credit balance too low" [S13][S43].

**ToS risk: LOW, explicitly allowed.** The legal page does not restrict how customers manage their own API keys, provided usage is billed to the key owner and not resold [S9]. API keys are the sanctioned path for automation [S10].

**Quota detection:** not about Max. Watch for credit and spend-limit errors, and switch on those if wanted.

**Role:** the documented fallback if design A gets classified as third-party, and the default if Tacita is ever given to other people.

---

## 3. Constraints that body modification must respect

1. **Never touch `system[0]` (the attribution block).**
   - Claude Code puts it first; it holds the client version and "a fingerprint derived from the conversation".
   - `api.anthropic.com` strips it **by position**, and only if it arrives unchanged as the first block in its own entry.
   - Anthropic reads it for attribution. On auto-mode classifier requests it is "the only marker in the request body that identifies them as Claude Code traffic".
   - Since v2.1.181 the block is stable for the whole conversation behind a custom base URL, with no per-request token [S3].
   - Do not set `CLAUDE_CODE_ATTRIBUTION_HEADER=0` on the subscription path. It removes the marker, and before v2.1.229 it broke auto mode [S3].
2. **Never touch `system[1]`, the identity line `You are Claude Code, Anthropic's official CLI for Claude.`**
   - This rule is undocumented; it comes from community measurements [S25][S26]. On OAuth requests this line must be the first system block after the attribution block, either as its own array entry or as the start of a plain string.
   - Merging it, moving it to second place or leaving it out fails. Since about Aug 2026 the failure is a **429 `rate_limit_error` "Error" with no unified headers and no retry-after**. Earlier failures were a 400 "This credential is only authorized for use with Claude Code…" (until Feb 2026) and a 400 "Error" (Mar 2026).
   - As of 2026-10-20, haiku-5-5 and sonnet-5-5 are gated as well; only claude-haiku-4-5 is exempt [S26]. #103731 is a Claude Desktop probe bug that used a setup-token; it is cited only for this server-side measurement.
   - The scrub dictionary must never match words inside these two blocks, for example "Claude", "Anthropic" or a stand-in rule touching "official". Do no Unicode or apostrophe normalization anywhere.
3. **Keep the system array shape**: same count, order and block form, and every `cache_control` marker. Never turn `system` into a string and never flatten content arrays [S3].
   - **Editing later system blocks** (the env block with the cwd path, project names) is an **untested risk, not a safe default**. CLIProxyAPI regressions show the third-party classifier reacting to what is in the top-level `system` array: 400 "Third-party apps now draw from your extra usage" (#5934 2026-09-18, #6432 2026-10-07) [S23].
   - Exestence/redaction-proxy reports successful subscription requests while masking the OS username and project-directory names (Claude Code 2.1.287, 2026-10-14). It did not check overage headers, so it proves the requests got 200s, not that they were billed to the plan [S19].
   - Test this explicitly (section 4).
4. **Edit string values in place only.** Never remove or add fields, and never touch: `model`, `metadata`, `thinking` / `redacted_thinking` blocks and their signatures, `signature_delta`, tool names, tool definitions' structure, betas, or beta/body field pairs [S3][S12].
5. **Determinism (preserved thinking).**
   - For Fable 5.1 and Opus/Sonnet/Haiku 5.5, a thinking block is valid only if `system`, `tools` and every earlier message keep the same *values*. JSON key order and formatting don't matter. A change gives 400 "…bound to a different conversation"; a tampered signature gives 400 "Invalid signature" [S12].
   - Enforcement is **on by default only for accounts created on or after 2026-08-31**. Older accounts are checked only when the request opts in through `thinking.block_binding` [S12].
   - Claude Code recovers by stripping old thinking blocks and retrying, **but only if Tacita returns upstream error bodies unchanged** [S3]. A mismatch then costs reasoning quality and a cold cache (faster Max quota burn), not a dead session.
   - Rules:
     - Freeze the stand-in mapping per `x-claude-code-session-id`. Apply new dictionary terms only to new content, or start a new session.
     - Restoring and then re-scrubbing must give exactly the upstream bytes, for both assistant text and `tool_use` input. Cases that break this: the model writes a real name itself, case variants, or user text that already contains a stand-in such as `project_x01`.
     - Safest approach: cache the exact upstream assistant blocks (keyed by tool_use id, signature or a content hash) and put back those original bytes on replay, instead of re-scrubbing restored text.
   - On 5.5 / Fable models the thinking text is empty by default, so text and tool_use round-tripping is the main risk [S12].
   - Test aid: send the `thinking-binding-controls-2026-08-01` beta with `block_binding` unset. The API then lists prefix edits as `thinking_mismatch_allowed` in `input_transformations` without enforcing them. Use this in tests only, since adding a beta is itself a body change [S12].
6. **The fingerprint (unofficial).**
   - Reverse engineering (yage.ai, 2026-04-01) says the attribution fingerprint is the first 3 hex characters of SHA-256 over a salt, the characters at positions 4, 7 and 20 of the first user message, and the version [S33].
   - If the server recomputes it, scrubbing a name in the first ~20 characters of the first message could make it disagree with the content. Whether the server checks it is unknown.
   - **Do not tune the scrubber to keep those characters, and do not recompute or forge the fingerprint or `cch`.** That would be "bypassing protective measures" [S10] or "misrepresenting the client" [S11]. A gateway maintainer refused it as "forging" (mnfst/llm-gateway#2471). If scrubbing breaks the check, accept the result or use design C.
7. **`cch` body hash (unofficial).**
   - Reported to be an xxHash64 over the whole body, verified server-side when present [S34].
   - CLIProxyAPI's source says native Claude Code 2.1.220 to 2.1.234 sends the billing header **without** `cch` for any custom base URL [S24]. Together with the official v2.1.181 statement [S3], this means `cch` is probably absent on Tacita's path. This is not confirmed for current releases (around 2.1.30x).
8. **Headers.**
   - Forward `anthropic-*`, `Authorization` and `x-claude-code-*` verbatim, and merge headers, never replace them [S3][S39].
   - Never run the credential scrubber over headers.
   - Do not add vendor-like headers. Claude Code reportedly reports known gateway header prefixes to telemetry [S33].
9. **Streaming.**
   - Restore names across SSE events and byte-chunk boundaries using lookahead [S20][S19].
   - Never buffer whole responses. Claude Code's stall watchdog fires after 300 s (byte-level) behind a custom base URL.
   - Keep `ping` events, event order, `message_delta` / `message_stop` and `signature_delta` unchanged.
10. **Errors.** Return upstream 4xx/5xx bodies and `retry-after` / `x-should-retry` unchanged. Claude Code's recovery logic matches on the upstream error text [S3].

---

## 4. Recommended design and a minimal live test

### Recommendation
Use **design A**: Claude Code keeps its own Max login, `ANTHROPIC_BASE_URL` is the only setting, and Tacita passes the token through without storing it.
- Tacita leaves `system[0]` and `system[1]` byte-identical, scrubs string values deterministically with a mapping frozen per session, and passes all headers and errors through.
- It switches to OpenRouter (Claude models, with Tacita's own OpenRouter key from Keychain) only on a 429 with a unified `rejected` status.
- Keep extra usage OFF or capped low, so misclassification fails loudly.
- Keep design C ready as the documented fallback. Do not build design B.

### Minimal live test (the user runs it; the token is never printed)

Preparation:
- Turn extra usage OFF at claude.ai/settings/usage.
- Note the current plan usage percentage on that page.
- Make a throwaway directory whose path contains a fake client name, e.g. `~/tmp/acmewidgets-test`, and a file inside it that mentions "acmewidgets".

**Phase 1: pure pass-through (scrubbing off)**
1. Start Tacita in a debug mode that logs, for each request, **only**:
   - the header names
   - a boolean for `Authorization` starting with `Bearer sk-ant-oat`
   - a boolean for an `anthropic-beta` OAuth value being present
   - the first 60 characters of `system[1]`
   - the response status and every `anthropic-ratelimit-unified-*` header

   It must never log the Authorization value.
2. Start `claude` in the throwaway directory and run `/status`. Check that "Login method" shows Claude Max, that the Anthropic base URL is Tacita's, and that there is no "Auth token" or "API key" row.
3. Ask "read the file and summarize it" (forces a tool call), then ask a follow-up (second turn).
4. Pass criteria:
   - All responses are 200.
   - The unified headers are present, with status `allowed` / `allowed_warning`.
   - `5h-utilization` rises between turns.
   - No overage header shows extra usage in use.
   - The claude.ai usage page shows plan usage moved and no extra-usage charge.

**Phase 2: same steps with scrubbing on**
- The mapping includes `acmewidgets -> project_x01`, which touches the cwd path in the env system block, the first user message and the tool result.
- Pass criteria:
  - Same as phase 1.
  - No 400 "Third-party apps now draw from your extra usage".
  - No header-less 429.
  - No 400 "bound to a different conversation".
  - `cache_read_input_tokens` is non-zero on turn 2.
  - Claude's reply shows "acmewidgets" restored.

**Phase 3 (optional):** same as phase 2, plus a session long enough to trigger compaction, a subagent and a WebSearch. These are request types whose shape is not yet confirmed.

Re-run phases 1 and 2 after every Claude Code update.

---

## 5. Unverified or unclear claims (flagged)

- **Identity-line check** (`system[1]` must be "You are Claude Code…", or you get a header-less 429): community measurements only [S25][S26]. Undocumented, and it changed at least three times in 2026.
- **Attribution fingerprint from first-message characters 4, 7 and 20**, and whether the server checks it: reverse engineering only [S33]. The official doc says only "derived from the conversation" [S3].
- **`cch` absent behind a custom base URL**: from CLIProxyAPI source comments covering 2.1.220 to 2.1.234 [S24]. Not confirmed for current releases.
- **Whether editing Claude Code's own system text** (env block, cwd path) triggers the third-party classifier: unknown. Evidence conflicts: Exestence succeeded [S19]; CLIProxyAPI's layout changes failed [S23].
- **Current billing rule for "third-party" traffic.** These sources conflict:
  - From 2026-04-04, harnesses draw from extra usage [S35][S44][S46].
  - The Help Center (updated 2026-06-15 and 2026-10-07) says third-party apps "still draw from your subscription limits" [S15].
  - The 400 "Third-party apps now draw from your extra usage" was still seen in Oct 2026 [S23].
- **The "Cowork stays covered" and "2026-01-09 server-side block" details**: secondary sources only. GIGAZINE says "around January 2026" [S45]. VentureBeat's line that bans were reversed is the author's paraphrase [S36].
- **`oauth-2025-04-20` as the exact beta value**: community sources only [S16][S25]. Whether stripping it still causes 401 is disputed: the official doc says yes [S3], #87420 measured no difference [S25].
- **Usage meters hidden under design A**: untested. #87876 used the wrong auth path [S27].
- **setup-token scope `user:inference` only**: GitHub reports [S28], not official docs.
- **"Remote Control disabled since v2.1.196"**: the restriction is official [S8]; the version number is not verified.
- **Suspension after routing to GPT-5.6 through a local proxy** [S38] and **revocation with an injected setup-token** [S29]: single, low-confidence user reports with no stated cause.
- **The Usage Policy proxy clause** [S11]: whether it is new in the 2026-11-12 version is unknown. It is not in force on 2026-10-24.
- **How Anthropic treats a content-rewriting proxy used by the subscriber themselves**: no Anthropic statement found. If Tacita becomes a product, ask Anthropic for written confirmation.
- **Whether every auxiliary request type** (classifier, session title, compaction, WebSearch server tool) carries the identity block as `system[1]` through `ANTHROPIC_BASE_URL`: unknown. Log request shapes per `x-claude-code-request-class`.
- **Managed-settings fail-closed** (`allowedProviders: ["customEndpoint"]`, v2.1.285+): not verified together with a claude.ai login.
- **Access-token lifetime and refresh cadence for `/login` tokens**: not documented. Tacita just forwards a 401.

---

## Sources

| # | URL | Date |
|---|---|---|
| S1 | https://code.claude.com/docs/en/authentication | undated, fetched 2026-10-24 |
| S2 | https://code.claude.com/docs/en/llm-gateway | undated, fetched 2026-10-24 |
| S3 | https://code.claude.com/docs/en/llm-gateway-protocol | undated, fetched 2026-10-24 |
| S4 | https://code.claude.com/docs/en/gateways | undated, fetched 2026-10-24 |
| S5 | https://code.claude.com/docs/en/llm-gateway-connect | undated, fetched 2026-10-24 |
| S6 | https://code.claude.com/docs/en/network-config | undated, fetched 2026-10-24 |
| S7 | https://code.claude.com/docs/en/data-usage | undated, fetched 2026-10-24 |
| S8 | https://code.claude.com/docs/en/remote-control | undated, fetched 2026-10-24 |
| S9 | https://code.claude.com/docs/en/legal-and-compliance | undated, fetched 2026-10-24 (text since ~2026-02-19) |
| S10 | https://www.anthropic.com/legal/consumer-terms | effective 2025-10-08 |
| S11 | https://www.anthropic.com/legal/aup | effective 2026-11-12 |
| S12 | https://platform.claude.com/docs/en/build-with-claude/preserved-thinking | undated, fetched 2026-10-24 |
| S13 | https://platform.claude.com/docs/en/about-claude/api-credits-for-subscribers | undated, fetched 2026-10-24 |
| S14 | https://support.claude.com/en/articles/12429409-manage-usage-credits-for-paid-claude-plans | fetched 2026-10-24 |
| S15 | https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan | updates 2026-06-15, 2026-10-07 |
| S16 | https://docs.litellm.ai/docs/tutorials/claude_code_max_subscription | 2026 (Claude Code v2.1.296 screenshots) |
| S17 | https://tailscale.com/docs/aperture/how-to/use-passthrough-mode/claude-subscriptions | validated 2026-09-28 |
| S18 | https://docs.databricks.com/aws/en/ai-gateway/claude-max-anthropic-enterprise-support | updated 2026-09-11 |
| S19 | https://github.com/Exestence/redaction-proxy | README runs 2026-10-13/14/21 |
| S20 | https://github.com/CupOfGeo/llm-redact-proxy | pushed 2026-09-07 |
| S21 | https://github.com/tombii/better-ccflare | state 2026-10-24 |
| S22 | https://github.com/karpeleslab/teamclaude/blob/main/docs/routing.md | 2026-10-20 |
| S23 | https://github.com/router-for-me/CLIProxyAPI/issues/6432 (also #5934) | 2026-10-07 (2026-09-18) |
| S24 | https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/runtime/executor/claude_signing.go | covers CC 2.1.220–2.1.234 |
| S25 | https://github.com/anthropics/claude-code/issues/87420 | 2026-08-17 |
| S26 | https://github.com/anthropics/claude-code/issues/103731 | 2026-10-20 |
| S27 | https://github.com/anthropics/claude-code/issues/87876 | 2026-08-19 |
| S28 | https://github.com/anthropics/claude-code/issues/79360 (also #92529) | 2026-07-20 (2026-09-06) |
| S29 | https://github.com/anthropics/claude-code/issues/102967 | 2026-10-17 |
| S30 | https://github.com/anthropics/claude-code/issues/45016 | 2026-04-08 |
| S31 | https://github.com/anthropics/claude-code/issues/103325 | 2026-10-19 |
| S32 | https://github.com/anthropics/claude-code/issues/97831 | 2026-09-28 |
| S33 | https://yage.ai/share/claude-code-defense-in-depth-en-20260401.html | 2026-04-01 |
| S34 | https://a10k.co/b/reverse-engineering-claude-code-cch.html | research Feb 2026 |
| S35 | https://www.infoworld.com/article/4154435/anthropic-cuts-openclaw-access-from-claude-subscriptions-offers-credits-to-ease-transition.html | 2026-04-06 |
| S36 | https://venturebeat.com/technology/anthropic-cracks-down-on-unauthorized-claude-usage-by-third-party-harnesses | 2026-01-09 |
| S37 | https://openrouter.ai/docs/guides/guides/claude-code-integration | undated, fetched 2026-10-24 |
| S38 | https://en.kocpc.com.tw/?p=24341 | 2026-08-12 (low confidence) |
| S39 | https://github.com/musistudio/claude-code-router/issues/1528 | 2026-07-13 |
| S40 | https://code.claude.com/docs/en/errors | undated, fetched 2026-10-24 |
| S42 | https://github.com/anthropics/claude-code/issues/84583 | 2026-08-06 |
| S43 | https://support.claude.com/en/articles/17154008 | Oct 2026 |
| S44 | https://news.ycombinator.com/item?id=47633396 | 2026-04-03/04 |
| S45 | https://gigazine.net/gsc_news/en/20260220-anthropic-third-party-block | 2026-02-20 |
| S46 | https://techcrunch.com/2026/04/04/anthropic-says-claude-code-subscribers-will-need-to-pay-extra-for-openclaw-support/ | 2026-04-04 |
