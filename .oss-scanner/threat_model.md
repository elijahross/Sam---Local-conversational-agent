# Threat model

## What this project does and where untrusted input enters
Sam is a voice assistant for macOS. A Rust server (`server/`) runs on the user's Mac, listens on
`127.0.0.1:47800` (HTTP + WebSocket for the Swift app), plans background work with an LLM and
runs tools: web research and scraping in its own Chrome over CDP, email, chats, calendar,
notes, files the user granted, social media research, marketplace monitors and datasets.

Untrusted input enters from:
- **web pages** Sam reads (research, scraping, monitors, social media JSON over CDP, product pages);
- **email, chat messages and files** it reads for the user;
- **LLM output** (plans, tool arguments, extraction recipes, judge verdicts), which is shaped by
  the content above (prompt injection);
- **other local processes / web pages** reaching the local ports `127.0.0.1:47800–47804`;
- **update channels**: Sparkle appcast + app zip, and the signed model manifest (`release/`).

## Components that matter most / least
- Most: the approval gate (`server/src/tools.rs` pending actions, `thinker.rs`: any `write` tool
  must not run without the user's OK, and must run exactly what the card showed); local
  HTTP/WS auth (`server/src/localauth.rs`, `main.rs`); secret handling (`secrets.rs`, Keychain;
  secrets must never reach a model, a log or a third party); the sandbox runner for generated
  code (`sandbox.rs`, `codegen.rs`); browser automation that can submit forms (`browser/agent.rs`,
  `browser/park.rs`, `web_task`); manifest / update verification (`manifest.rs`).
- Medium: parsers of untrusted pages and JSON (`browser/scrape.rs`, `browser/listings*`,
  `social*.rs`, `creators.rs`, `market.rs`, `monitors.rs`, `email.rs`), SQLite stores, file
  access grants (`files.rs`).
- Least: `lab/` (not shipped; gitignored), benchmarks and docs.

## How to exercise it
- `cd server && cargo test --release --lib` (unit tests).
- `server/target/release/think "<request>"` runs the planner + tools on one request
  (needs LLM keys in `.env`; offline it fails at the LLM call).
- `server/target/release/scrape "<task>" <url> field1,field2` runs the scraper.
- The HTTP API is in `server/src/main.rs` (axum routes).

## How you rate severity
- Critical: a write action (send, post, buy, delete, submit) that runs without the user's
  approval or differs from the approved card; a remote web page / email / message causing data
  to leave the Mac or code to run outside the sandbox; accepting an unsigned or downgraded update
  or manifest; another local user or a web page driving the local API.
- High: secrets (API keys, passwords, Keychain items) reaching an LLM prompt, a log or a third
  party; reading files outside the user's grants; bypassing the per-site budgets / tripwires in
  ways that get the user's accounts banned.
- Medium: denial of service of the local server from untrusted content; parser panics.
- Low: issues that need an attacker who already controls the user's macOS account.

## Anything to leave alone
- Sending page content and user requests to the user's chosen LLM provider is by design.
- The local Chrome profiles holding the user's own logins are by design (the user signed in).
- `lab/` is research code, not shipped.
