# Contributing to Sam

Thanks for wanting to make Sam better. This guide covers how the project thinks, how to set up, and what a good pull request looks like: why the change is worth it, what it changes for the user, how it works, and how you tested it.

- [What Sam stands for](#what-sam-stands-for)
- [Before you start](#before-you-start)
- [Development setup](#development-setup)
- [Kinds of contributions](#kinds-of-contributions)
- [Writing a pull request](#writing-a-pull-request)
- [Testing your change](#testing-your-change)
- [Measuring model, prompt and voice changes](#measuring-model-prompt-and-voice-changes)
- [Code style](#code-style)
- [Commits, review and merging](#commits-review-and-merging)
- [Releases](#releases)
- [License](#license)

## What Sam stands for

Every change is weighed against these. A PR that bends one should say so and explain why.

1. **Ask before acting.** Anything that writes (send, post, order, book, delete) is drafted and needs the user's approval. A deterministic check on the user's own words decides what "yes" approves, never the model.
2. **Local first, honest about the rest.** Audio, memory, decisions and credentials stay on the Mac. Whatever does leave (the conversation to the LLM provider, opt-in screen context) is documented in the README's *Risks and privacy* section.
3. **Secrets never reach a model.** Credentials live in the Keychain; the model sees placeholders. Text bound for a model passes `secrets::redact`.
4. **Measured, not guessed.** Defaults come from measurements ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)). A change to a model, prompt or voice setting comes with before/after numbers.
5. **A calm, consistent companion.** One voice per character, no per-sentence styling, short spoken replies, no coaching.
6. **Light on the Mac.** Models load on demand and unload when idle; background work is bounded (`governor.rs`).

## Before you start

- **Small fixes** (typos, clear bugs, docs): open a PR directly.
- **Anything bigger** (a feature, a new tool or connector, a new dependency, a model swap, a change to approvals or privacy): open an issue first, or comment on an existing one, so we can agree on the approach before you invest time.
- **Security problems:** report them privately ([SECURITY.md](SECURITY.md)), not in an issue or PR.
- **A model or API is retiring?** Use the *Model or API change* issue form. Often a manifest edit is all it takes (see [Releases](#releases)).

## Development setup

Requirements: an Apple Silicon Mac with macOS 26, Xcode command-line tools, Rust (stable), [`uv`](https://docs.astral.sh/uv/), and an API key for one LLM provider (or Ollama).

```sh
git clone https://github.com/elijahross/Sam---Local-conversational-agent.git sam && cd sam
cp .env.example .env              # add MINIMAX_API_KEY (or set LLM_PROVIDER=ollama)
./sidecar/computer/signing.sh     # once: local code-signing identity, keeps macOS permissions across rebuilds
./run.sh                          # dev stack; first run creates .venv and downloads models (~4 GB)
./app/Sam/build.sh && open app/Sam/build/Sam.app   # the app uses the running dev stack
```

The dev stack keeps its data in `data/` (git-ignored). That is your real memory and history while you develop: back it up before experimenting, and never commit it.

Where things live:

| Area | Path | Language |
|---|---|---|
| Voice loop, LLM clients, memory, tasks, tools, browser | `server/src/` | Rust |
| Tool definitions | `server/tools/<name>/` (`tool.json`, `instructions.md`) | JSON / Markdown |
| Persona | `server/persona_sam.md` | Markdown |
| Speech (STT, TTS, end of turn) | `sidecar/speech_server.py` | Python (MLX) |
| Memory embeddings, Laya decisions | `sidecar/mind_server.py` | Python |
| Mac app (island, menu bar, onboarding, overview, updates) | `app/Sam/Sources/` | Swift |
| Helpers (Apple STT, computer use, virtual display) | `sidecar/applestt`, `sidecar/computer`, `sidecar/workspace` | Swift / Obj-C |
| Model manifest, update key, version | `release/` | JSON |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the pieces fit together.

## Kinds of contributions

Each kind needs slightly different evidence in the PR.

| Kind | Examples | What the PR needs |
|---|---|---|
| **Bug fix** | A card shows the wrong time, a crash, a misheard command routed wrong | How to reproduce it before the fix, a test that fails before and passes after (unit or E2E) when practical |
| **Functional change** | A new capability, a new connector or tool, a changed default, new UI | The user problem it solves, the flow as the user experiences it, screenshots or a recording, privacy and approval impact, docs |
| **Technical change** | Refactor, performance, dependency, build or CI | Why now (what it unblocks or fixes), proof behaviour is unchanged (tests), numbers for performance claims (latency, RAM, CPU) |
| **Model, prompt or voice change** | A new default model, persona rule, TTS setting, judge threshold | Before/after measurements on the same inputs ([Measuring](#measuring-model-prompt-and-voice-changes)), cost and latency impact |
| **Model manifest change** | A retired model id, a new Hugging Face repo | The announcement or evidence, the raised `version`, a check that the replacement works with the request format Sam uses |
| **Docs** | README, architecture, benchmarks | Keep facts verifiable: link the code or the measurement |

New tools: add `server/tools/<name>/tool.json` (one-line summary, kinds, `read`/`write` effect, args) and `instructions.md`. A `write` tool must go through the drafted-action and approval path; say in the PR how a user sees and approves it.

New dependencies: explain why the standard library or an existing dependency won't do, check the license (Apache-2.0 compatible), and for Python, update `app/runtime/requirements.lock`. The app reinstalls its runtime when that file changes, which is a 1.5 GB download for every user, so batch such changes.

## Writing a pull request

The [PR template](.github/pull_request_template.md) has these sections. Fill in what applies; a reviewer should understand the change without reading the diff first.

**1. Why.** Start with the problem, from the user's side. *"When I say 'remind me at five' late in the evening, the reminder is created for 5 am the next day"* beats *"fix time parsing"*. Say who is affected and how often, and link the issue.

**2. What changes for the user (functional).** What someone using Sam will notice: behaviour, defaults, settings, permissions, anything Sam now says differently. Include screenshots for UI and an audio clip for voice changes. For a pure refactor, write "nothing visible".

**3. How it works (technical).** The approach in a few sentences, the main files, and alternatives you rejected and why. Call out changes to:
- protocols between the app, the server and the sidecars (`/ws` messages, sidecar HTTP and WebSocket APIs);
- stored formats (`data/*.json`, SQLite tables): how existing users' data migrates;
- the model manifest or update path;
- prompts sent to the LLM (token cost per turn).

**4. Privacy, safety and cost.** Tick the checklist honestly. If the change sends new data off the Mac, acts on an account, or adds per-turn LLM calls, describe it and how the user controls it.

**5. How you tested it.** List the commands you ran and what you checked by hand, specifically enough that a reviewer could repeat it ([Testing](#testing-your-change)).

Keep PRs focused: one concern per PR. A refactor mixed with a feature is hard to review and hard to revert.

## Testing your change

**Every PR**

```sh
cd server && cargo test --release --lib     # unit tests (CI runs these too)
./app/Sam/build.sh                          # the app builds (CI too)
```

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) also validates the model manifest, checks Python and shell syntax, and refuses commits of `.env`, `data/` or keys.

**Unit tests** sit next to the code (`#[cfg(test)] mod …` in `server/src/`). Add one for any logic you can test without models or network: parsers, gates, chunking, redaction, scheduling. Tests that need the real Notes, Reminders or a network are `#[ignore]` and run explicitly.

**End-to-end tests** run the real stack and Sam.app in test mode, with speech injected from WAV files instead of the microphone:

```sh
.venv/bin/python app/Tests/E2E/e2e.py confirm cards          # chosen scenarios
.venv/bin/python app/Tests/E2E/e2e.py                        # everything (long; uses your LLM key)
```

- Quit your own Sam first; the suite refuses to run while it holds the ports.
- The suite uses its own data directory and test fixtures (`app/Tests/E2E/fixtures/`). It never reads your real windows, data or accounts, and new scenarios must keep it that way (`SAM_NO_REAL_APPS`).
- Prefer one realistic multi-step scenario over several trivial ones: "point at the invitation, ask for a reply and an event, approve one, cancel the other" catches more than four single-step checks.
- Pick the scenarios that cover your change:

| You changed | Run at least |
|---|---|
| Voice loop, turn-taking, barge-in | `hover handsfree bargein ptt close voice` |
| Cards, approvals, drafted actions | `cards confirm offer_yes tasks_ui` |
| Background tasks, planner, tools | `complex_plan complex_automation split_requests followup_task continuation` |
| Memory, open threads, check-ins | `you threads checkin history` |
| Screen context (focus / highlight and ask) | `point point_chat point_doc point_web` |
| Privacy, redaction | `secrets` plus the security suite |
| Onboarding, connectors, storage | `onboarding connectors logins storage uninstall_plan` |
| Model loading, idle unloading | `models` |

**Security regression** (approvals, local services, browser, redaction, sandbox, updates):

```sh
./run.sh &                                              # stack running
.venv/bin/python app/Tests/Security/attack_tests.py     # 21 attacks; all must pass
```

**Manual checks** matter for a voice app; automated tests can't hear. For anything the user sees or hears, try it on a real Mac and describe what you did in the PR: the phrases you said, hands-free and push-to-talk, light and dark mode, a Mac with and without a notch if layout changed, and the first-run onboarding if setup changed (`defaults delete one.sam.app onboarded` on a test account).

**Updates and the manifest:** if you touched `release/`, `Updates.swift` or `manifest.rs`, run `python3 tools/check_manifest.py` and describe how you tested the change (the maintainer guide in [release/README.md](release/README.md) shows a local feed setup).

## Measuring model, prompt and voice changes

Model output varies from run to run, so a single good example proves nothing. Compare old and new on the same inputs and report the numbers in the PR.

| Change | Measure | Notes |
|---|---|---|
| TTS model or settings | UTMOS per sentence, word error rate (transcribe the output back), glitch rate, first-audio time, real-time factor | Score per sentence: whole-clip scores hide glitches. UTMOS matched human reviewers; speaker-embedding similarity did not. Listen to a sample yourself |
| Speech recognition | WER on clean and noisy sets, latency, resident memory | Use your own recordings, not `say` output |
| LLM model | Time to first token, valid JSON plans, cost per turn, prompt-cache hit rate | Over a real multi-turn conversation, not a single call |
| Persona or prompt | Pairwise LLM judging (old vs new reply, same scenario, order randomised) on 20+ scenarios | Absolute 1–10 scores are too coarse to show prompt changes. Keep test scenarios out of the prompt's examples |
| Memory or recall | Recall@k and MRR on a question set, end-to-end answer accuracy | |
| Laya thresholds | Accuracy on a held-out set, live latency budget (250 ms) | |

Add significant results to [docs/BENCHMARKS.md](docs/BENCHMARKS.md), with the date, the Mac and the sample size.

## Code style

- **Match the surrounding code**: its naming, idioms and comment density. Small, readable functions over clever ones.
- **Comments explain why**, not what: a measurement, a constraint, a past bug (`// 2026-10-13: a 3 s startup probe failed on every launch`).
- **Rust:** don't run `cargo fmt` over whole files; the code isn't rustfmt-formatted, and reformatting buries the real change. Format only what you write. No new `unwrap()` on paths that can fail at runtime; errors fail open where a feature is optional (memory, judge).
- **Swift:** SwiftUI + AppKit, no new third-party packages without discussion. UI strings are short, plain and friendly; follow `Theme.swift` for colours and type.
- **Python (sidecars):** keep them thin model servers; logic belongs in the Rust server. Pin any new package in `app/runtime/requirements.lock`.
- **User-facing text** (UI, spoken replies, docs): plain words, short sentences, no jargon. Sam talks like a friend, not a manual.
- **No personal data in code, tests or fixtures**: use invented names, numbers and addresses.

## Commits, review and merging

- Branch from `master`: `fix/reminder-evening-time`, `feat/telegram-voice-notes`, `models/minimax-m4`.
- Commit messages: a short imperative subject (*"Keep evening reminders on the same day"*), then a body with the why when it isn't obvious.
- Keep the history reviewable: squash fix-up commits before asking for review.
- CI must be green. A maintainer reviews for the principles above, correctness, tests and docs; expect questions, especially about privacy and approvals.
- PRs are squash-merged. The PR description becomes the commit message, which is one more reason to write it well.

## Releases

Maintainers cut releases; contributors don't need the signing key.

- **Model manifest** (`release/manifest.json`): model ids, retirements and Hugging Face repos reach every install within a day, without an app update. A PR that changes it must raise `version`; CI checks this.
- **App release:** `VERSION` and `BUILD` in `release/version.env`, then `app/release.sh --publish`.

Details: [release/README.md](release/README.md).

## License

Sam is licensed under the [Apache License 2.0](LICENSE). By submitting a contribution you agree it is licensed under the same terms (Apache-2.0, section 5), and that you have the right to submit it. Don't include code, models, voices or data you aren't allowed to share under that license.
