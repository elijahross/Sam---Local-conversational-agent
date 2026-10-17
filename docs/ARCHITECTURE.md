# Architecture

```
Sam.app (mic + echo cancellation, playback) ──WS pcm16──▶ Rust server :47800 (axum/tokio)
                                                    │
         ┌──────────── live voice loop (never blocks) ┴───────────────────────────┐
         │ Silero VAD → turn/merge/barge-in → STT → [memory recall ≤300 ms]       │
         │ → LLM provider (MiniMax M3.1 Flash default) → chunker → TTS            │
         │ fillers / delegation bridge cover any latency                          │
         └────────────────────────────────────────────────────────────────────────┘
                │ submit jobs (non-blocking)              ▲ notices ("work status injection")
                ▼                                         │
         Background task runner (tasks.rs) ──────────────┘
           • memory consolidation
           • web research: search → read top 10 → store (`browse` CLI)
           • browser agent: logins, forms, navigation (`agent` CLI)
           • thinking agent: email, documents, MCP tools — files only inside user grants
             → results as files in data/artifacts/<task>/, one row in `tasks`
                │
                ▼
     SQLite (WAL, source of truth)          turbovec 4-bit (derived, in-process)
     memories · messages · tasks            rebuilt at boot from SQLite

Sidecars (one venv):    speech :47801  Apple STT (or Parakeet v3) + Qwen3-TTS 0.6B Base (MLX)
                        mind   :47802  Qwen3-Embedding-0.6B (MLX) + Sam-Laya (torch MPS)
Helpers:                computer use :47803 (Swift, Accessibility) · workspace display
```

## Working memory (what the LLM sees each turn)
The working memory is a capped pool:
1. The persona (`server/persona_sam.md`).
2. Long-term recall: the top-6 facts and episodic summaries (z-scored relevance plus recency; outdated facts labeled), about 300 tokens, with relative dates and the framing "newer beats older, what they say now wins".
3. Finished background work, drained once.
4. Today's date and time.
5. Short-term memory: the current session's turns, capped at 24 messages.

Long-term memory is written only by the background consolidation job: fact extraction with supersession, plus a session summary. It runs every 10 user turns and at session end.

## Background thinking agent
- **Submitting work.** Laya routes a turn to `task` (p ≥ 0.8). If the live 250 ms judgment budget is missed, it's judged again off the hot path. The voice loop submits `Job::Request` and Sam says it's on it. The loop never waits.
- **Thinker** (`thinker.rs`). One planning call (the configured `LLM_PROVIDER`, MiniMax M3 by default; `THINKER_MODEL` overrides) picks at most 3 steps:
  - `research`: search, read the top 10 pages, write a cited brief;
  - `scrape`: a recipe once, then code on every page;
  - `browse`: open-ended browser agent;
  - `code`: a Python script in the sandbox VM;
  - `find_files`: granted files only.

  `$prev` chains steps (scrape → code). Requests run one at a time beside the memory worker. Chrome starts only when a step needs it and stops afterwards.
- **Sandbox** (`sandbox.rs`). Apple `container`, one lightweight VM per run:
  - no network, no secrets, read-only root filesystem, non-root user;
  - limits of 1 GB memory, 2 CPUs and 90 s;
  - inputs are mounted read-only and only after `Files::check` (user grants plus denylist);
  - `/out` is the only writable path.
- **Results.** Files go to `data/artifacts/{research,scrape,sandbox}/…` and are indexed in the catalog. The `tasks` row holds the status and a one-line summary.
- **Announcing.** Running jobs appear in every turn's prompt ("still running"). Finished jobs and hand-off requests ("please log in to X") come back as a `Notice` in the next turn.
- **Prompt size.** The voice prompt carries no tool definitions, only one capability paragraph. The planner sees one line per tool. Each tool's detailed prompt loads only when it runs.
- **Tool registry** (`server/tools/<name>/`, `tools.rs`). Progressive disclosure:
  - **Each tool:** `tool.json` (a one-line summary, kinds, `read`/`write` effects, args) and `instructions.md`.
  - **Stage 1:** the planner sees only the top 6 one-liners for the request, ranked by Laya `kind`, word overlap and embedding similarity.
  - **Stage 2:** the chosen tool's instructions or MCP input schema load only to fill its arguments.
- **MCP client** (`mcp.rs`, configured in `server/mcp.json`). Local servers over stdio JSON-RPC.
  - **Listing:** tools are listed once, cached in `data/mcp/<server>.json` and merged into the registry.
  - **Environment:** a server gets only PATH, HOME and its configured env; values can come from the Keychain (`keychain:<service>/<account>`).
  - **Results:** capped at 1.5k characters for the notice; the full text goes to `data/artifacts/mcp/`.
  - **Effects:** MCP tools are `write` unless listed in `read_tools`, or the server is marked `trust_hints` (annotations are self-declared).
- **Email** (`email.rs`; tools `email_search`, `email_read`, `email_send`).
  - **Account:** IMAP/SMTP with an app password in the Keychain (`agent --email-setup`). TLS is required except to localhost.
  - **Reading** is read-only (`EXAMINE`).
  - **Sending:** the reply body is drafted with the original mail as untrusted data, plus a recipient guard in code. Sending only happens after the user confirms.
- **Tool safety** (Composio patterns, kept local):
  - **Outcomes:** each step ends `done` / `failed` / `input_required` / `unknown`.
  - **Approval** per tool in `tool.json` / `mcp.json`: `always_allow` / `ask_once` (remembered per tool and target, e.g. recipients) / `ask_always` / `never`.
  - **Action ledger** `data/actions.jsonl`, append-only, also the audit log. A write runs at most once per action id and is never retried. A timeout is reported as "may have gone through".
  - **Connection status** shows in the planner's list ("UNAVAILABLE: not set up…").
  - **Secret masking** (`secrets::redact`) runs on every tool result before a model, log or speech sees it.
  - **MCP `pin` / `hide`** fix arguments and keep them out of the model's view.
- **Local toolkits** (`apps.rs`; Composio catalogue, private versions): `weather` (Open-Meteo), `web_fetch` (static, else rendered), `calendar_events` / `calendar_create`, `reminders_list` / `reminder_create`, `contacts_lookup`, `note_save`.
  - **Calendar, Reminders and Contacts** go through macOS's own apps (JXA), so iCloud, Google and Exchange accounts on the Mac work without OAuth.
  - **Recipient guard:** contacts found for a name the user said count as user-named recipients.
- **Write actions need the user's OK.** A `write` tool is only drafted: its arguments are filled, a pending action goes to `data/pending/<id>.json` (expires after 1 h), and the user gets a notice ("Needs your OK…").
  - **Running it:** a later "yes, go ahead" runs exactly that drafted action. A deterministic word gate on the user's own words decides, never the LLM.
  - **Cancelling:** "no / cancel" drops it.

## Laya micro-judgments
Sam-Laya is Laya fine-tuned in 8 minutes on 1.6k Gemini-labeled decisions. It answers typed questions in about 20–40 ms; each caller has its own threshold and fails open.

| Where | Judgment | Policy |
|---|---|---|
| Every turn (parallel with recall) | route, kind, done | Task at ≥0.8 → thinking agent. Deep at ≥0.8 → MiniMax in delegate mode. Unfinished utterance → hold audio 900 ms |
| Memory write | secret | Regex or ≥0.4 → redacted, never extracted |
| Memory write | rel to nearest facts | Replaces ≥0.7 → LLM verifies → supersede |
| Memory write | low-value session | ≥0.8 → kept, ranked lower |
| Browser agent | DOM element relevance, page state | Laya filters the HTML; the LLM or vision model only sees top candidates |

## Browser module
- **Screens:** host Chrome, one isolated persistent profile per screen. The window is headful but off-screen, which passes Cloudflare and DataDome; Linux containers and micro-VMs got blocked.
- **Logins:** log in once with the window visible; tasks reuse the session (Bops golden-profile pattern).
- **Token use:** the LLM is called twice per research task (plan and brief), about 1.8k tokens total. Everything else is code, Laya `page_state`, consent handling, and embeddings. Raw HTML never reaches a model.
- **Structured scraping:** one recipe call, then code on every page, cached per site (Amazon: 200 products for 4.4k tokens, 0 on repeat). Login walls are handed to the user.
- **Artifacts:** `data/artifacts/research/<date>_<slug>/` holds `brief.md`, relevant `sources/` and `checked.jsonl`; `data/artifacts/scrape/…` holds `items.csv`/`jsonl` and `recipe.json`.

## Backups
- Back up `data/memory.sqlite` with `sqlite3 .backup` or Litestream, plus `data/artifacts/`.
- The turbovec index is derived and needs no backup.
- If this ever moves to Postgres, use the same tables, put the vectors in `pgvector halfvec`, and feed turbovec from an outbox.

## Sam Island: macOS app (see [app/README.md](../app/README.md))
- **App:** native Swift (SwiftUI + AppKit) in `app/Sam`. It's a non-activating notch panel modelled on VoiceOS.
  - **Activation:** ⌥Space (hold = push-to-talk, tap = hands-free), fn (with Input Monitoring), hover on the notch.
  - **Display:** an orb, waveform, streaming answer and cards (result / confirm / error).
- **Protocol additions:**
  - client → server: `ptt`, `context`, `confirm`;
  - server → client: `card`. A confirm card's Approve goes through the same deterministic gate as "yes, go ahead".
- **Models on demand** (`sidecar/model_manager.py`):
  - **Loading:** sidecars load models when a session sends `warm` (a session start does).
  - **Unloading:** 5 min (speech) or 15 min (mind) after last use, 60 s after the last session closes, earlier under memory pressure.
  - **Idle RAM:** about 0.9 GB, against 7.5 GB before.
- **E2E:** `app/Tests/E2E/e2e.py` runs on the real stack (hover, focus, hands-free, warm latency 1.2 s, barge-in, push-to-talk, background result card, confirm card with exact times + cancel, close, idle model release).

## Focus and ask (opt-in)
- **Opt-in:** off until the user turns on *Focus and ask* (window) or *Highlight and ask* (selection) in onboarding or the island menu; with both off nothing on screen is read.
- **Reading what you point at:** ⌥Space reads the **window under the cursor** through the helper (`/window_at`: AX hit test → `AXWindow` → text). This works for covered windows too, without raising them.
- **Request and context:** the pointed-at window travels with the request as untrusted context. Action verbs plus a pointed-at window route to the thinker deterministically.
- **Drafting:** one step per requested action, each a separate pending action and confirmation card (e.g. reply + event).
  - **Event times** are exact (RFC 3339) and overlapping events appear as a ⚠ warning on the card.
  - **Email** goes over IMAP/SMTP, or Mail.app scripting (hidden compose) when no app password is set.
- **Background:** API-first, so nothing appears on screen. For UI-only tasks, a virtual display (`CGVirtualDisplay`; the E2E tests use `app/Tests/E2E/vdisplay`) gives Sam its own invisible screen; tested: move, read, edit, type and screenshot there while the user's frontmost app is untouched.
- **Approval safety:** the island never takes the keyboard unless clicked, and Approve has no keyboard shortcut.

## Voices and characters
- **Profiles:** one TTS model, Qwen3-TTS 0.6B Base, clones a voice profile per piece: `voices/<id>/` holds a 10 s `ref.wav` plus its `ref.txt`. The reference codes are cached, so every piece of every reply is conditioned on the same clip.
- **Characters:** Sam is one persona with two voices, `sam-male` and `sam-female`.
- **Selection:**
  - The island sends `{"type":"character","character":"sam","variant":"female"}` on connect and when the menu changes.
  - The session switches persona and voice from the next piece on.
  - The server default comes from `CHARACTER` / `SAM_VOICE`.
- **Tone tags** still shape the text, but no longer change the voice.

## Chat replies
- **Where it runs:** pointing at WhatsApp Web plus "respond to Markus…" becomes a `message` task. Sam works in **its own** WhatsApp Web session (off-screen Chrome screen, linked once by QR, persistent profile), never in the user's window.
- **`browser/chat.rs`:** opens the chat (exact match, then fuzzy, ambiguity refused), reads messages (untrusted data), and sends only into the chat whose header equals the approved title. The text in the box is checked before Enter, and delivery is verified as the newest outgoing message.
- **`chat_send`:** drafts the reply. When the reply agrees to meet, a separate `calendar_create` suggestion card follows, with conflicts. Each write is its own approval.

## Tasks, menu bar and overview
- **`tasklog.rs`:** one record per background request: plan, live steps, actions, files, result. Approvals attach to the drafting task. Served at `/api/tasks`.
- **Sam.app:**
  - **Menu bar** (`StatusBar.swift`): the running / needs-you count, a glass drop-down, and notifications.
  - **Overview** (`Overview.swift`): full-screen blur, tasks on the left, the selected task in the middle, approvals editable.
- **Files:** the helper's `/window_at` returns `document` / `file` / `url`. Doc tools may read exactly the pointed file (`docs.rs`): `doc_review` and `papers_similar` (OpenAlex + arXiv, ranked by the local embedder).
- **Website tasks:** `web_task` is the browser agent in approval mode. The final click is held and the tab parked (`browser/park.rs`); approving re-checks the values, clicks once and reports. It never types payment details.
- **More sites:** Telegram Web K in `browser/chat.rs` (per-site hooks), Gmail on the web in `browser/gmail.rs` (after Mail.app).

## Sam Workspace, island activity and connectors
- **Sam Workspace** (`sidecar/workspace`, `browser/workspace.rs`): an invisible virtual display, created once at the first browser use and kept for the server's lifetime. It exits with the server (stdin EOF). Sam's Chrome launches and parks there, so no window ever flashes on the user's screens. `WORKSPACE_DISPLAY=0` falls back to off-screen parking.
- **Island activity:**
  - **Live activity:** while tasks run or wait, the hidden island widens around the notch (spinner or orange hand plus a count); clicking it opens the overview.
  - **Toasts:** finished or waiting tasks drop down from the island as a toast (Review / Dismiss, 6 s, hovering keeps it). This happens only while the island isn't in a conversation.
- **Connectors** (`connectors.rs`, `/api/connectors`, `Connectors.swift`):
  - **Catalog:** built-in Mac toolkits, email (IMAP setup or Mail.app), WhatsApp / Telegram / Gmail sessions (link once), website logins (Keychain), file folders (folder picker), and MCP integrations (one-click templates or custom).
  - **Secrets** go to the Keychain; mcp.json holds only `keychain:` references.
  - **Turning a connector off** marks its tools unavailable to the planner.
- **API guard:** every POST under the server needs the `X-Sam` header, which a web page can't send without a CORS preflight the server never answers.

## Security guardrails
- **Local services:** the server :47800 (`/ws` + `/api`), the helper :47803 and the test API :47804 need a localhost Host, no foreign browser Origin and the local token (`~/Library/Application Support/Sam/local-token`, 0600; `localauth.rs`, `LocalAuth.swift`). mind and speech refuse browser origins. The helper's computer-use actions are off unless enabled (`computer-use-enabled` flag file or `SAM_COMPUTER_USE=1`).
- **Approvals:** judged on the user's own words only (≤12 words); ids match exactly; a bare "yes" only with a single fresh draft; orders and payments need a click; `VOICE_CONFIRM=0` makes all approvals click-only. Card edits are limited to editable fields. `ask_once` approvals are per target and expire after 7 days. Destructive MCP tools always ask.
- **Browser:** http(s) only (test fixtures excepted). Destructive controls are refused or become the approval step; in approval mode the agent stays on the site. `browse` is gated like `web_task`. Accounts (chat, mail) use their own profile.
- **Redaction:** pointed text and MCP output are redacted (keys, tokens, passwords, one-time codes) before models see them.
- **Privacy:** every cloud request passes `secrets::redact` (keys, tokens, passwords, cards, IBANs, one-time codes) and contact masking; password managers and secure fields are never read.
- **Tests:** `app/Tests/Security/attack_tests.py` (21 attacks).

## Resources
- **Governor** (`governor.rs`): MAX_PARALLEL slots, a MAX_QUEUE queue (refuses beyond it), memory-aware admission from `vm_stat` available memory against the job's need plus a reserve, and a per-task timeout. Queued tasks are visible as "Queued".
- **Browsers:** one Chrome per profile, reference counted and closed by its last user. Parked tabs are reaped after PARK_TTL_MIN. Shutdown closes everything; orphans are reaped at boot.
- **Sandbox:** one VM at a time; the container service stops when idle and nothing else uses it.
