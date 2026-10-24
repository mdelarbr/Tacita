# Tacita — design

Date: 2026-10-24 · Status: draft for owner review · License: Apache-2.0 (already in the repo)

Supporting research (sources, recipes, full test list):
- `docs/research/2026-10-24-prior-art.md`, the landscape, reuse map, detection, restoring rules (R1–R22) and tests (T1–T26), plus the leak checklist
- `docs/research/2026-10-24-claude-max-through-tacita.md`, how the Claude Max login works through a local proxy, the policy position and the live test

Where this document and the research disagree, this document wins.

Labels used below:
- **[owner]**: decided by the owner in conversation.
- **[designer]**: the designer's call, to be checked by the owner.
- **[builder]**: deliberately left to whoever builds it.

---

## 1. Why this exists

The owner codes with Claude Code on a Claude Max subscription. Coding with an LLM means sending it the project's internal code, company and project names, client names, people's names, and sometimes credentials that show up in files or command output.

The owner is comfortable sending that to Anthropic. They are not comfortable sending it to an open-weight model hosted by an unknown provider, for example through OpenRouter. So today, when the Max quota runs out, **they stop working** rather than switch provider.

Tacita is a privacy layer that runs on the owner's Mac, between their AI tools and every LLM provider. Each provider only ever sees scrubbed content:
- credentials are never sent;
- identifying business information is replaced by stable stand-ins (`boarddata` → `project_x01`);
- the code logic itself still goes through, so the model can do useful work.

On the way back, Tacita puts the real names back exactly, so the edits the model makes land on disk correctly.

The first use is coding (Claude Code). Later uses are other coding tools, then personal business and strategy discussions with any LLM.

The owner wants a project they **control and understand**. Tacita borrows the best ideas from existing projects but does not depend on opaque products.

**Guiding principle [owner]:** Tacita must not change the user experience. It works silently in the background. The owner looks at it only when they want to, through `tacita watch`.

**…but never fake protection [owner]:** when Tacita is broken or protecting less than usual, that must be **explicit**. The owner must never feel protected while not being protected.
- **Any failure fails closed with a visible error in Claude Code**, never a silent pass-through. This covers the daemon being down, the protection file being unreadable, the Keychain being locked, a configured detector being down, and a request shape Tacita doesn't understand.
- **Every reduced-protection mode is visible inside Claude Code itself**, not only in `tacita watch`. This covers `tacita pause` and the Step 0 credentials-only fallback on Max. **[designer]** How:
  - `tacita setup claude` adds a short Tacita segment to Claude Code's status line (`Tacita ✓ protected`, `Tacita ⚠ PAUSED: names not protected`, `Tacita ⚠ Max: credentials only`). If the owner already has a status line, the segment is appended to it rather than replacing it.
  - A macOS notification fires when such a mode starts.
  - The `watch` header turns red.
- **[designer]** If the status line can't be installed, setup says so, and `tacita doctor` reports "reduced-protection modes are not visible in Claude Code".

## 2. What it is, in one picture

```
Claude Code ──(Anthropic Messages, streamed)──► 127.0.0.1  tacitad (always on)
                                                  1. scrub every string value (protection file + credential rules)
                                                  2. final check: anything protected left? → mask or block, never send
                                                  3. forward in the SAME format ──► Anthropic (Max login, passed through)
                                                                               └──► OpenRouter (key from Keychain)
                                                  4. restore real names in the streamed reply (byte-exact)
tacita watch ◄── local socket ── live events, decisions on new findings
~/…/Tacita/protection file  (the single source of truth, on this machine only)
```

## 3. Goals, non-goals, anti-goals

### Goals for v1
1. **[owner]** Claude Code runs on the owner's **Max subscription through Tacita** with full protection.
2. **[owner]** Claude Code runs on **OpenRouter through Tacita** with the same protection. Switching is a **manual command** the owner runs when Max is exhausted.
3. **[owner]** A **protection file** on the machine holds every protected concept. It evolves over time: Tacita proposes new findings, and the owner confirms, allows or renames them easily.
4. **[owner]** **Credentials never leave the machine**, whichever provider is in use.
5. **[owner]** **No UX change** in Claude Code by default. New findings are hidden silently, with no prompts during coding.
6. **[owner]** A **terminal side panel** (`tacita watch`) shows what was hidden or blocked on each request and lets the owner answer questions about new findings with one key. Everything is terminal-based.
7. **[designer, from the owner's anti-goal]** Proof: a command that shows every entry in the protection file is caught, so the owner can trust it.

### Later, in order **[owner agreed to the order]**
- **v1.1:** the OpenAI **Chat Completions** door. It covers OpenCode with API keys or OpenRouter, chat apps (Open WebUI, LibreChat, Jan, Zed…) and "Ollama with OpenRouter" set-ups.
- **v1.2:** the OpenAI **Responses** door, for Codex CLI.
- **v2:** business and strategy discussions: prose-heavy detection, documents, policies for amounts and dates. See research §9.

### Non-goals
- **[owner]** Defending against an actively malicious model that tricks the agent into sending files elsewhere (network sandbox, `tacita run`). This may come later. Claude Code's own permission prompts remain the guard.
- **[owner]** Touching **deliberate sends**:
  - **Images and PDFs** go to the provider unscanned, because you chose to send them. In the owner's words, "image we don't check anything". **[designer]** This applies whether you paste them or Claude Code opens an image file itself; `tacita watch` notes which. Text-type documents are ordinary text and are scrubbed.
  - **claude.ai connector calls** (Notion, Gmail, Calendar, Qonto…) go from Claude Code straight to Anthropic and on to that service. You deliberately connected that service, so it can receive the context it needs. What a connector *returns* still goes through Tacita on the next request, so it is scrubbed before reaching any model.
  - **[designer, applying the same principle]** Claude Code's other **Anthropic-hosted tools**:
    - the Artifact tool, which publishes pages to your claude.ai account;
    - cloud agents and RemoteTrigger;
    - SendMessage to cloud sessions.

    They are services of your own account, so they are treated like connectors. `tacita watch` lists every deliberate send.
- **[designer]** Translating between API formats (for example, Claude Code to a Chat-Completions-only provider). Each tool is paired with a provider that accepts its own format.
- **[designer]** OpenCode or any third-party tool on the Claude subscription. Anthropic blocked this in 2026; see the Claude Max research note.
- **[designer]** Storing or injecting any Claude credential (`claude setup-token`). This is close to forbidden by Anthropic's terms, and a ban has been reported after doing it.
- **[designer]** Local-only Ollama, where nothing leaves the machine and Tacita adds nothing.
- **[designer]** A web dashboard, a team server, or multiple users.

### Anti-goals (failures even if it "works")
- **[owner]** **Any value in the protection file reaching a model, even once,** in any request Tacita forwards. This is the trust-breaker: if it happens, the owner stops using Tacita.
  - The only exceptions are the deliberate sends above and an explicit `tacita pause` (§11).
  - A value only counts as protected from the moment it is in the file; for what happens to earlier copies, see "Late finds" in §10.
- **[owner]** A visible change in how Claude Code feels day to day.
- **[designer]** A stand-in landing silently on disk, or a wrong name restored, corrupting the code.
- **[designer]** Credentials written to disk by Tacita.
- **[designer]** An opaque or heavy dependency in the part that decides what leaves the machine.

## 4. Threat model **[owner]**

| In scope | Out of scope (v1) |
|---|---|
| A provider (Anthropic, OpenRouter, the host behind it) **reads, logs or keeps** what it receives | A provider **actively steering** the agent's tools to send data elsewhere |
| Leaks through any part of the request: messages, tool results, file contents, system prompt (cwd path, git status), CLAUDE.md, MCP tool descriptions, headers, metadata | Malware on the Mac; local plaintext transcripts kept by Claude Code |
| Content-carrying traffic Claude Code sends **directly** to Anthropic, bypassing Tacita: /feedback, transcript surveys, WebFetch preflight. Closed by setup, see §11 | **Deliberate sends** (§3): images and PDFs; claude.ai connector calls and Anthropic-hosted tools; an explicit `tacita pause` |
| | Inference from what remains (domain logic, writing style) |

Single user, one Mac. The project is open source, so others can install it for themselves.

## 5. The protection file **[owner]** (format details **[builder]**)

**One entry per concept.** Each entry has:
- **concept:** the canonical value, e.g. `boarddata`.
- **kind:** project, company, client, person, place, domain and so on. It drives the stand-in word.
- **stand-in:** allocated by Tacita, stable over time, e.g. `project_x01`.
- **spellings:** the ways the concept is written, e.g. `BoardData`, `BOARD_DATA`, `board-data`, `Board Data`, `boarddata.io`, an old name, an internal short form.
  - **[designer]** When an entry is added, Tacita pre-fills this field with the standard coding forms (flat, UPPER, camel, Pascal, snake, SCREAMING_SNAKE, kebab, dotted, spaced title case, accent-folded), so the file shows them. Exact-case entries are the exception: they start with only the typed spelling.
  - The owner adds the irregular ones that can't be derived: old names, short forms.
  - Matching also covers case, accent and Unicode variants nobody listed (§6), so a missing standard form never leaks.
- **hint** (optional): a short description the model is allowed to see, e.g. "a B2B SaaS for board meetings". **[designer]** Stored in v1 but **not sent** until it has been tested (§10).
- **match mode** **[designer]**:
  - *substring*, the default for distinctive names of 6 characters or more;
  - *boundary*, for short names and acronyms. A match must sit on identifier sub-word boundaries, judged on the original casing *before* any folding. Boundaries are:
    - non-alphanumeric characters, including `_`, `-` and `.`;
    - lower-to-upper camelCase transitions;
    - the split between an acronym and a capitalized word;
    - letter–digit transitions.

    So `BD` matches `BD_URL`, `myBDClient` and `bd-api`, but not `BDD` or `abdomen`.
  - *exact-case*, for names that are common words (`Orange`, `Total`, `Pierre`). Only the listed spellings are matched, exactly.
- **status** **[designer]**:
  - `confirmed`;
  - `pending`, for an entry proposed by seeding that the owner hasn't reviewed. It is already hidden and keeps its stand-in across sessions.

An **allow list** holds values the owner marked "OK to send".

Rules:
- **[owner]** The file lives on the machine only, outside every repo. It is the single source of truth, shared by all projects and sessions.
- **[owner]** Changing it must take seconds, through CLI commands and `tacita watch`, with no hand-editing needed. Hand-editing is still possible.
- **[designer]** Storage: human-readable (TOML suggested) at `~/Library/Application Support/Tacita/`, mode 0600. `tacita setup claude` adds Claude Code permission rules that deny the agent Read and Edit on that folder. The file holds names, never credentials. Encryption at rest is a later option; in v1 the protection is FileVault plus file permissions.
- **[designer]** Seeding has two levels:
  - **Automatic on first contact:** the first time Tacita sees a request from a project it doesn't know (identified by the cwd in the request), it adds `pending` entries *before forwarding that first request*. These come from:
    - the project folder name;
    - the git remote org and repo names;
    - the owner's own git name and email.

    Nothing is prompted; the owner reviews later in `watch`.
  - **On demand (`tacita seed`):** it also proposes `git log` authors, package names (Cargo, npm scope, go.mod, pyproject), email domains and CLAUDE.md content. It is not automatic, because a cloned open-source repo would otherwise flood the file with strangers' names. Contacts and Calendar are never used.
- **[designer]** Auto-allow covers names Tacita never *proposes*:
  - third-party dependencies from public registries;
  - language keywords and standard libraries;
  - common French and English words.

  The project's own name, its package scope, and anything that shares the git org name are never auto-allowed, even when they appear as dependencies (`@boarddata/ui`, `github.com/boarddata/…`). A protected entry always beats the allow list. The allow list only suppresses proposals and has no effect on the final check.
- **[designer]** Detector findings (§13: emails, phone numbers, IBANs…) are **session-only**: hidden under a session stand-in until the owner presses **k** in `watch`, which makes them a confirmed, persistent entry. This keeps the file from filling up with every IP address or log email.
- **[designer]** Tacita **refuses** any spelling that its match mode would match in parts it must never modify (§10): the first two system blocks, tool names, model IDs. For example, a person named "Claude" would match "You are Claude Code…" and the `mcp__claude_ai_*` tool names. Tacita suggests protecting the person through full-name spellings instead ("Claude Dupont"). The same check applies to `tacita add`, automatic seeding, the **k** key and `tacita rename`. A common-word entry gets a warning when it is created.

## 6. Stand-ins **[designer]**

- **Format:** a kind word plus a fixed-width ID that contains a letter, for example `project_x01` and `company_x02`. The owner originally imagined `project_1`, but that breaks restoring. In camelCase or flat case, `project_1` becomes `project1` either way, so `boardData` and `boarddata` could no longer be told apart. The `x01` ID avoids this; research §6.2 has the detail.
- **The stand-in follows the spelling's case style:** `BoardDataService` → `ProjectX01Service`, `BOARD_DATA_URL` → `PROJECT_X01_URL`, `board-data` → `project-x01`, and `Boarddata` at the start of a sentence → `Projectx01`.
  - Only the entity part of an identifier is replaced, never the whole identifier. Keeping the rest of the identifier keeps model quality.
  - For substring and boundary entries, any case, accent or Unicode variant (NFC/NFD, full-width letters, zero-width characters inside the name) is hidden, even if it isn't listed.
  - Tacita records the exact form it saw in this session, so it can restore that form byte for byte.
- **Domains and emails:** domains map to the reserved `.example` TLD (`boarddata.io` → `projectx01.example`). Emails map to a person plus a domain stand-in.
- **Every spelling must map to its own stand-in form**, so restoring is unambiguous. This applies to irregular spellings too: old names, short forms. Tacita refuses to save an entry that would make two real forms collide, and shows why. **[builder]** How irregular spellings get their distinct forms.
- **Stable across sessions:** stand-ins come from the file, never from per-request counters. This keeps conversations consistent and provider-side caching working. The one exception is late finds (§10).
- **No custom stand-in text in v1.** Stand-ins are always `kind_xNN`. Restoring depends on that shape, and "rename" changes the kind, the value or the spellings, never the stand-in text.
- **Kind words are English** in v1. French kind words for business prose belong to v2.

## 7. Credentials **[owner]** (mechanics **[builder]**)

Credentials work differently from names:

| | Names | Credentials |
|---|---|---|
| Found by | the protection file plus proposals | rules (below) |
| Stored on disk by Tacita | yes, in the protection file | **never the value**; only a keyed fingerprint (below) |
| What the provider sees | `project_x01` | a clearly fake typed marker, so the model knows what kind of credential it is (exact form **[builder]**) |
| Restored on the way back | everywhere | **only into a file edit (Edit, Write, NotebookEdit) of the same file the credential was read from**; anywhere else the marker stays as is, and `tacita watch` reports it |

**What counts as a credential [designer, following the owner's "all blocked"]:**
- vendor-format rules: gitleaks rule data, MIT, vendored with attribution;
- generic high-entropy tokens, after an entropy filter;
- **context rules:**
  - any value assigned to a key named like password, passwd, secret, token, api_key or private_key, in env, YAML, JSON, TOML, compose files and code;
  - the `user:password@` part of URLs;
  - `Authorization`, `Bearer`, `Cookie` and `X-Api-Key` values in commands and configs;
  - PEM blocks;
- exact matches of secret values that already exist locally: `.env*` files, `~/.aws/credentials`, the shell environment Tacita can see;
- values the owner registers with `tacita secret add`. Input is hidden, and only the fingerprint is kept.

Test and example values (`sk_test_…`, `.env.example` placeholders, fixtures) are masked too.
- **Never treated as credentials:** known non-secret high-entropy formats, such as git object IDs, UUIDs, SRI and lockfile checksums, and protocol IDs (§8, opaque fields).
- **The remaining cost of false positives:** a masked value is unusable in commands, because markers are never restored into Bash. This is accepted.
- **[designer]** The guard hook (§10) **denies** writing a credential marker into a file other than its source, with an explanation, so a marker never lands silently in, say, `.env.local`.

**Fingerprints [designer]:** Tacita never stores a credential value. It does store an HMAC fingerprint of each detected credential, keyed with a secret held in the macOS Keychain, along with the file the credential came from. This lets a session resumed days later, after a Tacita restart, still recognize and mask credentials whose source has since changed. When a same-file restore is needed after a restart, Tacita re-reads the value from that file.

**Why the restore rule:** never restoring would let the model overwrite your real keys with markers when it rewrites a file such as `.env`. Restoring everywhere would let the model put a real key into a command or a web request. **[owner]** The rule will be validated in real use.

Credentials are blocked with every provider, Anthropic included. This cannot be turned off, not even by `tacita pause` (§11).

## 8. How a request flows

1. **Receive.** Claude Code sends `POST /v1/messages` to Tacita on `127.0.0.1`. Local access is protected by a random secret path in the base URL, plus Host/Origin checks; research §7 has the self-security list.
2. **Replay.** Assistant blocks Claude Code resends are swapped back to the exact bytes the provider originally produced, using the replay cache (§10).
3. **Scrub** every string value in the body, walking the JSON generically. This means messages, tool results, tool definitions and descriptions, and the system prompt except its first two blocks (see §10). Scrubbing uses the session's frozen snapshot of the protection file, plus credential rules and the other detectors.
   - **Keys:** property names in tool `input_schema` definitions and in `tool_use.input` are user data, often from MCP servers. They are scrubbed and restored with the same mapping. All other keys are protocol structure.
   - **Skipped:** opaque fields (below), and replayed blocks that are byte-identical to what the *same* provider produced. Sending a provider its own output back reveals nothing new.
4. **Final check (second lock)** **[designer, from the owner's anti-goal]**. After scrubbing, a separate pass scans the whole outgoing body, values and keys, except two things: opaque fields, and same-provider replays. **What counts as a leak:**
   - for substring and boundary entries, any spelling found after normalization: case folding, accent folding, NFKC, removal of zero-width and soft-hyphen characters, a look-alike "skeleton" as defined by Unicode, and line wraps or split JSON strings where the builder can detect them. Boundary-mode boundaries are judged on the original casing (§5);
   - the same after decoding, nested up to 3 levels and mixed: JSON escapes, URL-encoding, base64, hex, HTML entities;
   - for exact-case entries, only the listed spellings, exactly. This keeps `color: orange` and `total` from blocking normal coding requests;
   - any detected credential.

   **What it does with a hit:**
   - **In content Tacita may modify:** it replaces the token around the hit with an opaque marker that is never restored, checks again, then sends. `tacita watch` shows "caught by the second lock" with the location. This keeps the session alive, because a block would recur on every turn as Claude Code resends the history.
   - **In a part Tacita must not modify (§10):** the request is **blocked, not sent**. Claude Code shows an error saying Tacita blocked it and suggesting a new session, and `tacita watch` shows what was caught. **[builder]** An error status that Claude Code displays without retrying in a loop.

   The check may reuse its verdict for a history block whose bytes and protection-file version are unchanged.
5. **Forward** in the same format to the active provider, with the header and auth rules in §9.
6. **Restore** in the streamed reply, byte-exact (§10), without buffering the whole response.
7. **Record** the event for `tacita watch`: provider, model, counts per kind, credentials blocked, second-lock catches, late finds, new findings, images and other deliberate sends (§12).

**Opaque fields [designer]:** these carry protocol bytes or media, not user text, and are never scrubbed, decoded or run through credential or entropy rules:
- thinking `signature`;
- `redacted_thinking.data`;
- image and document `source.data`;
- `tool_use.id` and `tool_use_id`;
- message and request IDs.

They are still forwarded byte-identical. `tacita check` includes a long session with signed thinking, multi-turn tool use and a pasted image, and it must pass without a block.

Other paths:
- `POST /v1/messages/count_tokens` gets a 404. It is never forwarded, because its body carries the whole conversation. Claude Code then estimates the count itself.
- `HEAD /api/hello` is answered locally.
- Every other path is refused and logged (fail closed).

**Performance target [designer]:**
- **Typical turn:** the deterministic layers (dictionary, credentials, structured detectors, final check) add **under 50 ms** per request on the owner's Intel Mac. Research measured single-digit to about 20 ms per turn in Python. Unchanged history blocks are cached by hash, because the API resends the full history on every turn.
- **Worst case:** a cold, resumed, near-1M-token session should stay **under 1 s** for its first request. The plan includes a benchmark at that size.

## 9. Providers and switching

### Anthropic (Max subscription) **[owner: always through Tacita, full protection]**, **[designer: pass-through design]**
- Claude Code keeps its normal `/login` Max login. **No `claude setup-token`, no stored Claude key.** Only `ANTHROPIC_BASE_URL` is set. No `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `apiKeyHelper` / `CLAUDE_CODE_OAUTH_TOKEN` anywhere, because each of them switches the Max login off.
- Tacita forwards Claude Code's own `Authorization` bearer and all `anthropic-*` and `x-claude-code-*` headers **unchanged, to `api.anthropic.com` only**.
- It never stores, logs or reuses that token, never sends it to any other host, and redacts `sk-ant-` from its own logs.
- Upstream error bodies and rate-limit headers pass through unchanged, because Claude Code's recovery logic and limit messages depend on them.
- Token refresh happens directly between Claude Code and Anthropic. Tacita never refreshes.

### OpenRouter **[owner: manual switch, same Claude Code profile]**
- The OpenRouter key is stored in the **macOS Keychain** (`tacita key set openrouter`), never in a file.
- **[owner]** Claude Code stays in the **same profile**, with the Max login still present, so `claude --continue` carries the conversation across the switch. The owner accepts the low-confidence account-risk report (§15).
- In OpenRouter mode, Tacita **drops** Claude Code's Anthropic bearer and the OAuth beta value, injects the OpenRouter key, and maps Claude Code's model IDs to the model the owner chose. Unknown model IDs fail closed. It also strips thinking blocks bound to Anthropic and drops `metadata.user_id`.
- The request then goes to OpenRouter's Anthropic-compatible `/api/v1/messages` with the same scrubbing.
- Every request carries OpenRouter privacy preferences: no data collection, zero-data-retention providers, no silent fallback to other providers, and an optional `only` list of trusted providers. **[designer]** Defaults; the owner chooses the trusted list.
- OpenRouter converts the request internally for non-Claude models. How well a given model copes with Claude Code's tool use is unknown until tested (Step 0, §15).
- **Cost awareness [designer]:**
  - `tacita watch` always shows the active provider and model in its header;
  - `tacita status` shows them too;
  - the setup guide recommends setting a credit limit on the OpenRouter key itself, in OpenRouter's dashboard;
  - Tacita has no spending cap of its own in v1.

### Switching **[owner: manual]**
- Commands: `tacita use anthropic`, `tacita use openrouter --model <slug>`, `tacita status`.
- Tacita reads Anthropic's quota headers. On `429` with `anthropic-ratelimit-unified-*status: rejected`, Claude Code shows its usual limit message, and `tacita watch` suggests `tacita use openrouter` along with the reset time. Tacita never switches by itself.
- **[builder, after Step 0]** Whether switching also needs Claude Code settings changes, such as context-window and output limits for the chosen model, and therefore a Claude Code restart (`claude --continue` keeps the conversation). The requirement is one Tacita command, plus at most a Claude Code restart.
- A session's stand-in snapshot carries over the switch, so the conversation stays consistent.
- **Back to Anthropic [designer]:** `tacita watch` recommends a **new session** after `tacita use anthropic`. History written by another model carries thinking blocks Anthropic won't accept, and adds third-party-classifier exposure. Continuing still works on a best-effort basis: Tacita strips non-Anthropic thinking blocks on the way to Anthropic.

### Policy position **[designer]**, for the README
Routing your own unmodified Claude Code login through a local gateway is documented by Anthropic. **Rewriting the content is a grey area**: Anthropic's gateway guide says "inspect without modifying". Rewritten traffic could be classed as "third-party", which means errors or extra-usage billing.

Mitigations:
- single-user, local use only;
- never touch the first two system blocks;
- never forge Claude Code's fingerprints;
- **keep extra usage OFF**, or capped low, at claude.ai, so a misclassification shows up as an error rather than a bill;
- record a "policy last checked" date in the docs.

Anthropic's Usage Policy dated 2026-11-12 adds a clause against services that "route requests through consumer subscriptions". Tacita stays personal and local.

## 10. Restoring — the rules that matter **[designer]** (research §6.3 has the full list R1–R22, and §6.5 tests T1–T26)

- **Byte-exact or nothing.** Claude Code's Edit tool does exact string replacement. A failed Edit is safe, because the model retries. A wrong restore that succeeds corrupts the repo.
- **Text** is restored as it streams. Tacita holds back only the short tail that could be the start of a stand-in, and flushes it before each block ends.
- **Thinking is never restored.** Stand-ins stay as they are inside streamed thinking (research R14). Restoring there would put real names inside signed blocks that Tacita can never change afterwards.
- **Tool calls** are held until the block ends, then restored inside JSON string values only, with correct escaping. Every other byte stays untouched.
- **Only stand-ins this session actually sent are restored.** IDs the model invents, and fuzzy look-alikes, are never restored. Style changes are restored in that style (`PROJECT_X01` → `BOARD_DATA`).
- **History stays stable.** The protection-file snapshot is frozen per Claude Code session (`x-claude-code-session-id`):
  - newly hidden items apply immediately;
  - renames and "OK to send" apply from the next session, and `tacita watch` says so.

  Claude Code stores real names after restoring, then resends them. A replay cache therefore puts back the exact bytes the provider originally produced, so each provider always sees a consistent conversation. This keeps prompt caching and Anthropic's thinking-binding checks working.
- **Late finds [owner: "hide from now, keep going"].** A value may become protected after it already went out in clear in this session, through `tacita add`, a key pressed in `watch`, a new spelling, or a detector catching a later occurrence. The model may also write a protected value itself, for example from a pasted screenshot.
  - From the next request on, the value is hidden **everywhere, earlier turns included**, under a **session-only stand-in**. The provider can then link the real name to that stand-in, but never to the permanent one (research R17).
  - `tacita watch` warns "already sent once to <provider>".
  - The cost is one prompt-cache reset. On Max it may also cost one recoverable "different conversation" error, after which Claude Code drops old thinking.
  - The session continues.
  - **Precedence:** late-find hiding applies to content that came from the owner's side (messages, tool results, files), and it wins over byte-identical replay for that content.
  - A provider's own output resent to the **same** provider is exempt, including thinking written from a pasted screenshot: that provider already has it (§8).
  - After a switch to another provider, that output is no longer exempt. Thinking blocks are stripped as described in §9. Assistant text follows the late-find rule.
  - Occurrences that already went out under the permanent stand-in keep it, and new occurrences use the session-only one. Both are restored and guarded for the rest of the session.
  - If an old copy sits in a part Tacita must not modify and is not exempt, the final-check rule in §8 applies: block, and suggest a new session.
- **Never modified:** the first two system blocks (the attribution block and "You are Claude Code…"), thinking and redacted-thinking blocks, signatures, tool *names*, model, metadata structure. Never add, remove or reorder fields or blocks. Unknown fields pass through.
  - An MCP server whose *name* contains a protected value makes every request fail the final check. `tacita doctor` detects this and tells the owner to rename the server to a neutral alias.
- **Fail closed on restore failure.** If a tool call can't be restored safely, Claude Code receives an explanation instead of a tool call with stand-ins in it, and the turn ends. Stand-ins are never written to disk silently.
- **Guard hook [designer]:** as a safety net, `tacita setup claude` installs a Claude Code `PreToolUse` hook:
  - It **denies** any tool call that still contains a stand-in this session issued.
  - It also **denies** a call containing a stand-in *look-alike*, meaning an invented ID such as `project_x07` or a near miss such as `Project_X1`, with an explanation for the model, which then retries. It denies rather than asks because the owner works in bypass-permissions mode, where an "ask" might never show up.
  - It denies a credential marker going into a file other than its source (§7).
  - Step 0 checks that hook denials are honoured in bypass mode. The OpenRouter conformance tests measure how often denials happen per model.
- **Names in tool calls** are restored in every tool, including Bash and WebFetch, because local commands such as `cd ~/code/boarddata` need the real name and the threat model is a passive provider.
  - **[designer]** `tacita watch` flags a restored name going into a network-bound tool such as WebFetch, `curl` or `git push`.
  - Connector calls and Anthropic-hosted tools are listed as deliberate sends instead of being flagged.
  - Credentials follow §7.
- **Session state survives restarts [designer]**, so `claude --resume` and `--continue` keep working after a Tacita restart or a reboot. The state is:
  - the frozen snapshot;
  - the issued stand-ins, including session-only ones;
  - the replay cache of upstream bytes, in stand-in form only;
  - credential fingerprints.

  It is stored under Tacita's folder (mode 0600) for 30 days, matching Claude Code's own transcript retention. It holds no credential values.
- **Hints [designer]:** stored in the file, not sent in v1.0. Any added text is extra rewriting, a grey area on Max, and a legend that changes mid-session breaks caching. Sending hints is a v1.x item, OpenRouter first, once tested. They would go as a static legend fixed at session start, appended without touching the first two system blocks.

## 11. Claude Code setup (`tacita setup claude`, with a backup and `--undo`) **[designer]**, coverage **[owner]**

**Coverage [owner]:** every Claude Code session on the Mac goes through Tacita, always on, via the user-level `~/.claude/settings.json`.

Tacita writes only what it needs there, and `tacita doctor` checks it:
- `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/<secret>`. When Tacita is down, Claude Code gets "connection refused": that fails closed, and nothing is sent unscrubbed.
- It closes the direct channels that carry content to Anthropic:
  - `skipWebFetchPreflight: true`
  - the feedback command and transcript surveys disabled
  - telemetry and error reporting off
- **claude.ai connectors stay ON [owner]**, and so do the Anthropic-hosted tools **[designer]**: they are deliberate sends (§3).
- It installs the guard hook (§10) and the permission rules that deny Read and Edit on Tacita's folder.
- It adds the Tacita status-line segment (§1), appended to any existing status line.
- It removes or flags any `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper` or `CLAUDE_CODE_OAUTH_TOKEN` found in settings or shell files. It asks before touching shell files.
- Auto-update stays on, since it carries no content. Tacita notices a new Claude Code version (from the User-Agent) and suggests `tacita check --live` in `tacita watch`.

**The only feature lost: Remote Control.** It is Claude Code's own rule that any custom base URL disables it.

**When Tacita itself breaks [designer]** (for example after a Claude Code update): `tacita pause` switches to a **credentials-only pass-through, to Anthropic only, never to OpenRouter**.
- It is a **deliberate send** and an explicit exception to the anti-goal: names go to Anthropic in clear while it is on. It is the same trust level as the owner's Step 0 fallback (§15).
- It asks for a typed confirmation.
- It shows in Claude Code's status line, with a notification, and in `watch` and `status` (§1).
- It **sticks to the sessions that used it**: they stay paused until they end, so a session never flips to full scrubbing halfway through. It ends for new sessions with `tacita resume`.
- The owner keeps working on Max until Tacita is fixed. This avoids the "I stop working" situation Tacita exists to remove.

**Bypass risk [designer]:**
- A project's own `.claude/settings*.json`, a `--settings` flag or a shell variable can point Claude Code elsewhere. Tacita never sees that traffic.
- `tacita doctor` scans the owner's code folders for such overrides and warns. The README states the remaining risk.
- An opt-in `tacita lock`, which needs admin rights, writes Claude Code's managed settings to pin the base URL. It is not part of the default setup.

Research §4.2 has the full list of candidate settings with sources. **[builder]** The final minimal set, validated in Step 0. The Claude Max research note §3 lists constraints for the subscription path, for example not setting `CLAUDE_CODE_ATTRIBUTION_HEADER=0` there.

## 12. CLI and `tacita watch` **[owner: terminal only]**, command names **[designer]**

- `tacitad`, the background service. **[designer]** It runs as a launchd user agent so it is always on.
- `tacita setup claude` / `tacita doctor` / `tacita status` / `tacita pause` / `tacita resume`
- `tacita use anthropic` / `tacita use openrouter --model <slug>` / `tacita key set openrouter`
- `tacita add "<value>" --as <kind>`, `tacita allow "<value>"`, `tacita rename` (change the kind, the value or the spellings), `tacita remove`, `tacita list`, `tacita edit` (opens `$EDITOR`)
- `tacita secret add`, to register a one-off credential (hidden input, fingerprint only)
- `tacita seed`, to propose entries from the current project
- `tacita check`, which proves the protection works. It pushes synthetic requests containing **every spelling of every entry, plus generated case, Unicode and encoded variants**, plus sample credentials, through the real pipeline, in every field (message, tool result, tool call, system prompt, CLAUDE.md, MCP description). It reports anything that would leak, per field.
  - For exact-case entries, only the listed spellings are tested. Unlisted variants are reported as "not protected by design", not as leaks.
  - `--live` runs the Step 0 conversation against the active provider.
- `tacita watch`, a terminal panel to keep in a split next to Claude Code:
  - **header:** the active provider and model, and the mode (normal, paused);
  - **one line per request:** time, number hidden per kind, credentials blocked, second-lock catches, late finds, blocked requests (with reason and location);
  - **deliberate sends:** images (pasted by you, or opened by the agent), connector calls and Anthropic-hosted tool calls;
  - **new findings:** already hidden, shown with their proposed stand-in. Keys: **k** keep (makes the entry persistent), **s** OK to send, **r** rename (kind, value or spellings), **v** review spellings. Ignoring a finding is fine, since it stays hidden for the session.
  - **detail view [designer]:** open any recent request to see the scrubbed text actually sent. Select a word to add it to the protection file. This is the main way to spot names nothing caught. Detail views live in memory only.
  - **flags:** restored names going to network tools; credential markers that were not restored; a new Claude Code version; quota exhausted (suggests `tacita use openrouter`).
  - **[designer]** Logs on disk keep stand-ins, counts and entry IDs, never credential values. `tacita watch` shows real names live from the protection file. It strips terminal control characters from anything it displays.

## 13. Detection layers **[designer]** (research §5)

1. **Protection file:** all confirmed and pending spellings plus their normalized variants (§6), using a multi-pattern matcher (Rust `aho-corasick`) with the match modes from §5. It runs on the whole request and gives full recall of known values.
2. **Credentials:** the rules in §7, running on Rust's linear-time `regex` engine (chunk huge tool outputs), with a keyword prefilter and decoding of base64, URL, JSON and hex.
3. **Structured detectors** produce session-only *findings* (hidden, shown in `watch`), not silent drops: emails (split into person plus domain), phone numbers (France and international), IBAN, card numbers, SIREN/SIRET/NIR/French VAT with checksums, and IPs.
4. **Optional ML name detector:** candidates only, run on prose only (comments, docstrings, markdown, chat), with a time cap. **[builder]** Timing within v1, and the runtime. In Rust this means `ort` plus a GLiNER-family model pinned to ONNX Runtime 1.23.2, the last Intel macOS build. A configured detector that is down blocks requests rather than skipping them.

No published PII model labels company or project names reliably. **The protection file is the real protection**, and detectors only propose.

## 14. Constraints

- **Language: Rust [owner].** The owner prefers its logic. It suits the problem: the `aho-corasick` and `regex` crates, `serde_json`, hyper/axum, Ratatui. Rust is **not installed** on the machine yet (rustup needed).
- **Machine:** Intel Core i9-9880H, x86_64, macOS 15.6.1. It must also work on Apple Silicon.
- **Claude Code's gateway rules:**
  - no buffering of whole responses;
  - forward `ping` events;
  - treat headers and body fields as open lists;
  - keep the system array shape and `cache_control` markers;
  - return errors verbatim;
  - stay within the stall watchdog of about 300 s; a held tool block must stay bounded.
- **Dependencies:** small and readable in the code that decides what leaves the machine. Vendor rule data (gitleaks, MIT) with a NOTICE file. Never copy AGPL (TruffleHog) code or code with no license. Read PasteGuard, llm-dlp, Kiji, PrivAiTe and veilstream for ideas and turn their known bugs into tests (research §2–3).
- **Unsupported territory:** Anthropic doesn't support Claude Code with non-Claude models, and doesn't recommend body rewriting. Expect breakage on Claude Code updates, so the conformance and canary tests must be quick to re-run, and `tacita pause` exists (§11).

## 15. Risks and Step 0 (before building on assumptions)

Step 0 consists of live tests the owner runs, with Tacita logging only safe metadata (Claude Max note §4):
1. **Max through a pass-through proxy, with scrubbing off.** `/status` shows the Max login and Tacita's URL. Responses are 200 and usage counts against the plan, not extra usage.
2. **Max with scrubbing on**, using a test folder whose path contains a fake client name. Check for:
   - no "third-party apps draw from extra usage" error;
   - no 429 without rate-limit headers;
   - no "bound to a different conversation" error;
   - cache hits from turn 2;
   - the name restored in the reply.

   **Biggest risk:** scrubbing the cwd path inside Claude Code's system prompt could trigger the third-party classifier. That is untested, and the evidence conflicts. **Fallback [owner]:** on Max only, Tacita drops to **credentials-only protection**, which was the owner's original trust in Anthropic. OpenRouter keeps full protection. The plan must keep the policy per provider, so this fallback is a configuration switch, not a rewrite.
3. **Claude Code → OpenRouter `/messages`** with the chosen open-weight model: tool use, thinking fields and system entries all work. **Fallback [designer]:** if no open-weight model passes, the overflow target is Claude models through OpenRouter, with full protection, until one does.
4. **Guard hook under bypass-permissions mode:** a PreToolUse denial is honoured.
5. **Record real traffic** on a throwaway repo seeded with fake names. This becomes the test corpus.

Other risks:
- Claude Code updates change the request shape. Mitigation: canary and conformance tests on every version change, plus `tacita pause`.
- Model quality with stand-ins. Research suggests typed, structure-keeping stand-ins cost little, but this is untested for open-weight models on code.
- **Account risk [owner accepted]:** one low-confidence report of a suspension after routing Claude Code to a non-Claude model through a local proxy. The owner keeps the same profile for simplicity.

## 16. Success criteria

1. When the Max quota runs out, the owner runs one command and keeps coding with Claude Code on OpenRouter, in the same project with the same protection.
2. `tacita check` passes for every entry in the protection file, its variants and sample credentials, in every request field. No protection-file value appears in any request Tacita forwards to a model in real use, apart from deliberate sends (§3).
3. No stand-in is ever written to disk by an edit, and edits apply normally.
4. Tacita never writes a credential value to disk and never sends one.
5. Day to day on Max, Claude Code looks and behaves the same, apart from Remote Control and the small status-line segment. The deterministic layers add under 50 ms per typical request.
6. Whenever protection is reduced or Tacita is broken, Claude Code itself shows it. There is never a silent pass-through.

## 17. Left to the builder

- Crate choices, internal architecture, the port and the launchd setup.
- Protection-file schema details, as long as it stays human-readable and every rule in §5–6 holds.
- The credential marker format and the error status used for blocked requests.
- How irregular spellings get distinct stand-in forms.
- The ML detector's timing and runtime.
- Test harness structure, built from research §6.5 (T1–T26), with property tests over random stream-chunk boundaries.
- Distribution: `cargo install` first; a Homebrew tap later.

## 18. Open questions for the owner

None blocking. The review round's owner questions are answered and recorded above:
- late finds (§10);
- coverage and connectors (§3, §11);
- the OpenRouter profile (§9);
- the Step 0 fallback (§15).
