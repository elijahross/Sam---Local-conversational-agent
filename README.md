<p align="center">
  <img src="media/logo-black.svg#gh-light-mode-only" width="72" alt="Sam">
  <img src="media/logo-white.svg#gh-dark-mode-only" width="72" alt="Sam">
</p>

<h1 align="center">Sam</h1>

<p align="center">A voice companion for the Mac that lives in the notch, talks back in a natural cloned voice,<br>remembers what matters to you and does real work in the background, asking before anything leaves your Mac.</p>

<p align="center">
  <a href="docs/ARCHITECTURE.md">Architecture</a> ·
  <a href="docs/BENCHMARKS.md">Benchmarks</a>
</p>

---

> **Status: experimental.** Sam is a personal project used daily on one Mac. It is not notarized, it acts on your accounts when you ask it to, and the defaults reflect one person's setup. Read [Risks](#risks-and-privacy) before you install it.

## Contents

- [What Sam does](#what-sam-does)
- [Requirements](#requirements)
- [Install](#install)
- [Using Sam](#using-sam)
- [Risks and privacy](#risks-and-privacy)
- [Models](#models)
- [Benchmarks](#benchmarks)
- [Limitations](#limitations)
- [Development](#development)
- [Configuration](#configuration)
- [Repository layout](#repository-layout)
- [Contributing](#contributing)
- [License](#license)

## What Sam does

| | |
|---|---|
| **Talk** | Hold ⌥Space (or fn), speak, let go. Sam answers in about a second with a cloned voice that runs on your Mac. You can interrupt it mid-sentence. |
| **Remember** | Facts, preferences and open threads ("how did the test go?") from past conversations, recalled in ~20 ms from a local database. |
| **Work in the background** | Web research with sources, scraping, documents, email, WhatsApp / Telegram / Gmail replies, calendar, reminders, notes, MCP integrations (GitHub, Notion, Slack, …), sandboxed Python. |
| **Ask first** | Anything that writes (send, book, post, order, delete) is drafted and shown as a card. It runs only after you say "yes, go ahead" or click Approve. |
| **Dictate** | Hold ⌥⇧Space anywhere: cleaned-up text is typed into the focused field. |
| **Look at your screen (opt-in)** | *Highlight and ask* ("what does this mean?") and *Focus and ask* ("summarize this"), both off until you switch them on. |

Speech recognition, speech synthesis, memory and the small decision model all run locally on Apple Silicon. The conversation itself goes to the LLM provider you choose: a cloud API, or a fully local model through Ollama.

## Requirements

| | Minimum | Tested |
|---|---|---|
| Mac | Apple Silicon (M1 or later) | M4 Pro |
| macOS | 26 (on-device speech recognition uses SpeechAnalyzer) | 26.5 |
| Memory | 16 GB (use the *Tiny* voice and a cloud LLM) | 24 GB |
| Disk | ~6 GB free | |
| LLM | A MiniMax, OpenAI, Anthropic, Gemini or Mistral API key, **or** Ollama (local, no key), **or** a signed-in `claude` CLI | MiniMax |

## Install

Download `Sam-<version>.dmg` from the [latest release](https://github.com/elijahross/TTS/releases/latest), or build it from source:

```sh
git clone <this repo> sam && cd sam
./sidecar/computer/signing.sh   # once: a local code-signing identity, so macOS keeps permissions across rebuilds
app/package.sh                  # → dist/Sam.app and dist/Sam-<version>.dmg
```

Building needs Xcode command-line tools, Rust (stable) and [`uv`](https://docs.astral.sh/uv/).

Open the dmg, drag Sam to Applications and start it. The onboarding walks through:

1. **Requirements and setup:** checks the Mac, then a one-time download of the Python runtime (~1.5 GB) and the models (~3.2 GB) into `~/Library/Application Support/Sam/`, which keeps running while you continue. Pick Sam's voice (male or female) and whether Sam opens at login.
2. **AI provider:** pick one and paste a key (stored in the macOS Keychain), or choose Ollama / the Claude CLI.
3. **Talking:** the keys and gestures below.
4. **About you:** optional name, birthday and interests, so Sam can be a better companion.
5. **Permissions:** microphone (required), Accessibility (dictation, opt-in screen context), Input Monitoring (only for the fn key), the computer-use helper (optional).
6. **Apps and connectors:** Calendar, Reminders, Notes, Contacts and Mail access; *Highlight and ask* / *Focus and ask*; notes backend (Apple Notes or Obsidian); computer use.

### Updates

Sam keeps itself current in two ways, both signed with the project's update key and rejected if the signature doesn't match:
- **App updates** (Sparkle): checked daily, or *Check for Updates…* in the menu-bar drop-down (⏻ menu). If an update changes the Python packages, Sam reinstalls its runtime on the next start.
- **Model manifest:** when a provider retires a model or a better default ships, Sam picks up the new model ids within a day, downloads any new local model and restarts its services. No reinstall, and your own environment settings always win.

Details for maintainers: [release/README.md](release/README.md).

Sam starts and supervises its own services (agent server, speech, memory, helper) and stops them on Quit. Logs are in `~/Library/Logs/Sam/`. **Uninstall Sam…** in the menu-bar drop-down removes the app, its data, Keychain items and login item.

## Using Sam

### Talking

| Action | How |
|---|---|
| Push to talk | Hold ⌥Space, speak, release |
| Hands-free | Tap ⌥Space (or double-tap fn); Sam listens until you stop talking |
| fn key | Hold fn while the island is open. Needs Input Monitoring and *System Settings › Keyboard › Press 🌐 key to: Do Nothing* |
| Peek | Hover the notch |
| Interrupt | Just start talking while Sam speaks |
| Close | Esc, click outside, or ✕ |
| Voice off (meetings) | Island menu: text and cards only, no audio |

Things to try: *"What's the weather tomorrow?"* · *"Remind me to call Anna at five."* · *"Add to my journal: finished the onboarding."* · *"Find me night trains from Vienna to Venice next month."* · *"Reply to Markus on WhatsApp that Monday works."*

### Background tasks and approvals

Longer requests become tasks. Sam says it's on it and mentions the result in a later turn. While work runs, the menu-bar icon shows a count; an orange hand means something is waiting for you. The **overview** (menu bar › Tasks) shows every step, file and pending approval.

Every write action is drafted first. A card shows exactly what would happen (recipient, text, time, total). It runs once you approve, and never retries on its own. Orders and payments need a click; voice approval is not enough. `VOICE_CONFIRM=0` makes every approval click-only.

### Screen context (opt-in)

| Switch | What Sam receives with your question |
|---|---|
| **Highlight and ask** | The text you have selected in the frontmost app |
| **Focus and ask** | The title, web address, file and visible text of the window in front or under the pointer |

Both are off by default. Change them in onboarding (Connectors step) or in the island menu under *Share with your question*. Password managers and password fields are never read.

### Dictation, notes, reminders

- **Dictation:** hold ⌥⇧Space (or fn+Shift), speak, release. Fillers are removed; "new line" and "new paragraph" work.
- **Notes and journal:** "note this…", "add to my journal…", "find my notes about…" go to Apple Notes (folders *Sam* and *Journal*) or an Obsidian vault. Sam only adds, never deletes or overwrites.
- **Reminders:** "remind me…", "what's on my to-do list", "mark … as done".

### Connectors

Open **Connectors** from the island menu or the menu bar. Set up email (IMAP or Mail.app), link WhatsApp, Telegram or Gmail in Sam's own browser (scan a QR code once), save website logins, grant folders Sam may read, add MCP integrations, or turn built-in toolkits off. Every secret goes to the Keychain.

**Computer use** (off by default) lets the background agent operate Mac apps through Accessibility, in the background. Every goal asks you first. Terminal, Keychain Access, Disk Utility and Activity Monitor are off-limits; System Settings is read-only; quit / close / save / delete shortcuts and destructive buttons are refused.

## Risks and privacy

Sam is powerful because it can see, remember and act. Know what that means before you turn things on.

**What leaves your Mac**
- **Your conversation goes to your LLM provider.** That includes what you say, the recalled memories that are relevant to the turn, and your profile (name, interests). Only Ollama keeps everything local.
- **Screen context goes with your question when you switch it on.** ⚠️ If an `.env` file, a password, an API key or a private message is selected or visible in the focused window, it can be sent to the provider. Sam masks secrets it recognizes (API keys, tokens, passwords, one-time codes, card numbers, IBANs) and never reads password managers or password fields, but pattern matching cannot catch everything. Don't ask Sam anything while secrets are on screen, or keep *Focus and ask* off.
- **Background tasks visit websites, query search engines and call MCP servers** on your behalf. Fetched pages and tool results are sent to the provider as untrusted data.
- **Hugging Face** is contacted to download models during setup.

**What stays local**
- Audio: speech recognition and the voice run on the Mac; no recording is uploaded.
- Memory: `~/Library/Application Support/Sam/data/memory.sqlite`, plus artifacts (research briefs, scrapes, reports). Back these up, or delete them, like any personal data.
- Credentials: website logins, email app passwords, API keys and MCP tokens live in the Keychain. The model never sees them; it writes placeholders that are filled in only on the matching site.

**Acting on your behalf**
- Sam can send messages and email, create events, fill forms and operate apps, but only after an approval. A deterministic check on your own words decides what "yes" approves, never the model. Still, read cards before approving: a draft can be wrong.
- Pages, emails and chats can contain prompt injections. They are passed to the model marked as untrusted, the browser agent stays on the requested site in approval mode, and destructive controls are refused, but no defence is perfect.
- Sam's browser profiles stay signed in to your accounts (WhatsApp, Telegram, Gmail, sites you log in to). Anyone with access to your user account can use them.

**Other things to know**
- **Voice cloning:** Sam's voices come from synthetic source clips, not real people. Qwen3-TTS can clone any voice from 10 seconds of audio: only add voices you have the right to use, and never use Sam to impersonate someone.
- **Local services:** the agent server and helpers listen on 127.0.0.1 only and require a per-user token (`~/Library/Application Support/Sam/local-token`). They refuse requests from web pages (Origin / Host checks).
- **Signing:** builds are signed with a self-made local identity, not a Developer ID. Other Macs will show Gatekeeper warnings; distributing Sam needs proper signing and notarization.
- **Claude CLI provider:** uses your own Claude subscription through the signed-in `claude` CLI, for personal use on your own Mac only (Anthropic's terms apply). It runs headless with every tool off and only essential environment variables.
- **Not a safety device:** Sam can mishear, misremember and be confidently wrong. Don't rely on it for medical, legal, financial or safety decisions.

## Models

All local models are downloaded from Hugging Face on first setup. Check each model card for its license and terms.

| Role | Model | Size | License | Runs on |
|---|---|---|---|---|
| Voice (default, *Light*) | [mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit](https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit) | 2.0 GB | Apache-2.0 | MLX (GPU) |
| Voice (*Steady*, optional) | [mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit](https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit) | 2.4 GB | Apache-2.0 | MLX |
| Voice (*Tiny*, optional) | [mlx-community/pocket-tts](https://huggingface.co/mlx-community/pocket-tts) | 0.3 GB | CC-BY-4.0 | MLX |
| Speech recognition | Apple SpeechAnalyzer (on-device, macOS 26) | built in | Apple | Neural Engine |
| Speech recognition (dev alternative) | [mlx-community/parakeet-tdt-0.6b-v3](https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v3) | 2.5 GB | CC-BY-4.0 | MLX |
| End of turn | [mlx-community/smart-turn-v3](https://huggingface.co/mlx-community/smart-turn-v3) | 32 MB | BSD-2-Clause | MLX |
| Memory embeddings | [mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ](https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ) | 0.35 GB | Apache-2.0 | MLX |
| Micro-decisions (route, end of turn, secrets, supersession, page state) | [elijahross/laya-sam](https://huggingface.co/elijahross/laya-sam): Laya fine-tuned on Sam's decisions | 0.85 GB | Apache-2.0 | PyTorch (MPS) |
| Conversation and planning | Your provider: MiniMax M3.1 Flash (voice) and M3 (planning) by default; OpenAI, Anthropic, Gemini, Mistral, Ollama or the Claude CLI | cloud / local | provider terms | |

Models load on first use and unload when idle (5 min for speech, 15 min for memory), so an idle Sam uses about 0.8 GB.

## Benchmarks

Measured on a MacBook Pro M4 Pro, 24 GB. The full tables, methods and caveats are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

**Voice (TTS)**

| Model | First audio | Real-time factor | Glitch rate | UTMOS (naturalness, 1–5) | RAM |
|---|---|---|---|---|---|
| Qwen3-TTS 0.6B Base 8-bit + retake guard (default) | ~0.1 s streamed, ~0.7 s whole piece | 0.36–0.42 | 3.8 % | 4.05 | ~2.5 GB |
| Qwen3-TTS 1.7B Base 8-bit, short pieces whole + guard | ~0.2 s | 0.55 | 0–1.9 % | 4.19 | ~3.4 GB |
| Pocket TTS (100M) | 60–150 ms | 0.1–0.2 | 5.8 % | 4.00 | ~0.4 GB |
| *Human reference recording* | | | | 4.28 | |

Generating the whole reply in one pass at temperature 0.45 with a clean "golden" reference cut takes that a listener rejected (wrong voice, glitch) from 29 % to 3 % for the female voice.

**Speech recognition (word error rate, lower is better)**

| Test set | Apple (on-device) | Parakeet v3 |
|---|---|---|
| Clean sentences | 3.5 %, 110 ms | 4 %, 95 ms |
| Dictation, 380 samples with names, numbers and noise | 16.1 % | 11.5 % |
| German dictation | 13.8 % | 15.2 % |
| Resident memory | 60–85 MB | ~1.7 GB |

End-of-turn detection (smart-turn v3) cuts users off at 20 % of mid-sentence pauses, against 86 % for a fixed 200 ms silence, at the same 200 ms response time.

**Conversation models (time to first token of a voice reply)**

| Model | TTFT p50 | Notes |
|---|---|---|
| MiniMax M3.1 Flash, low reasoning (default voice model) | 0.49 s | 86–92 % prompt-cache hits in real conversations; warm first audio ~0.8–0.9 s |
| MiniMax M3 (planning) | 1.58 s | Valid JSON plans 2/2 |
| gpt-5.4-nano / gpt-6-luna | 0.52 s / 1.00 s | |
| gemini-3.5-flash-lite | 0.64–0.72 s | |
| qwen3.5:9b via Ollama (local) | ~1.8 s to first spoken word | Can't carry the companion persona; invents facts |

**Memory and decisions**

| | Result |
|---|---|
| Memory recall@5 / @10 (Qwen3-Embedding 0.6B 4-bit) | 0.915 / 0.994; 10–31 ms per turn |
| Laya micro-decisions, 13 tasks | 96.4 % mean accuracy, 20–28 ms per decision (a cloud LLM call: 615–680 ms) |

## Limitations

**The app**
- macOS 26 on Apple Silicon only; no Intel, Windows, Linux or mobile client.
- English first. Other languages work for recognition, but the voice garbles about 1 in 4 sentences outside English.
- One user per Mac. There is no sync between Macs and no multi-user separation.
- Not notarized: other Macs show Gatekeeper warnings until Sam is signed with a Developer ID.
- The cloud LLM is still the main latency and the main privacy boundary.
- Computer use and website automation depend on app and site layouts, and break when those change.

**The models**
- **Voice:** very short pieces (1–4 words) are where clones glitch, and lone "Mm" or "Hmm" never clone cleanly. On a busy Mac (heavy CPU and GPU load) the 0.6B falls behind real time (RTF 1.74) and stutters. A clone occasionally drifts to a slightly different pitch or pace; automatic scores catch garbled takes, but not every drift.
- **Fine-tuning the voice doesn't help:** LoRA on Sam's own approved takes either changed nothing or broke the voice (UTMOS 4.37 → 1.31).
- **Speech recognition:** at 0 dB signal-to-noise every engine tested stays at 22–30 % WER; denoisers made things worse. Slow, hesitant speakers still get cut off at some pauses.
- **LLM:** the fast voice model sometimes splits one spoken request into several; splitting therefore runs on the larger model. Local 4–9B models can't carry the persona and planning reliably.
- **Memory:** partial updates ("I moved, but still work in Vienna") are sometimes missed, and "what did we talk about in our first chat" style questions rank poorly.
- **Laya:** can't verify whether one exact entry appears in a long listing (75 %).

## Development

```sh
./run.sh               # dev stack: speech + memory sidecars and the Rust server on :47800
                       # (creates .venv with the app's pinned packages on first run)
./app/Sam/build.sh     # → app/Sam/build/Sam.app; uses the running dev server instead of its own
open app/Sam/build/Sam.app
```

`.env` (see `.env.example`) holds API keys for the dev stack; the app itself uses the Keychain.

**Tests**

```sh
cd server && cargo test --release --lib                 # unit tests
.venv/bin/python app/Tests/E2E/e2e.py [scenario ...]    # real stack + Sam.app in test mode (quit your own Sam first)
.venv/bin/python app/Tests/Security/attack_tests.py     # 21 attacks against a running stack
```

The E2E suite runs its own data directory and never touches your real data, windows or accounts. It refuses to start while your own Sam holds the ports.

**Command-line tools** (`server/target/release/`): `agent` (browser agent, logins, folder grants, email setup), `browse` (research brief), `scrape` (structured scraping), `code` (Python in the sandbox VM), `think` (run the background agent on one request).

**Sandbox:** generated code runs in an Apple `container` VM without network or secrets. Build the image once: `container build -t sam-sandbox:1 sandbox/`.

**Adding a voice:** see [voices/README.md](voices/README.md). **Adding a tool:** create `server/tools/<name>/` with `tool.json` and `instructions.md`; MCP servers go in `server/mcp.json`.

## Configuration

The app writes its settings itself; these environment variables matter for the dev stack (`./run.sh`). All defaults come from the measurements in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

| Variable | Default | Effect |
|---|---|---|
| `LLM_PROVIDER` | `minimax` | `minimax`, `openai`, `anthropic`, `gemini`, `mistral`, `ollama` or `claude-code` |
| `MINIMAX_VOICE_MODEL` / `MINIMAX_MODEL` | `MiniMax-M3.1-Flash-Preview` / `MiniMax-M3` | Voice turns / planning and background work |
| `OPENAI_MODEL` / `GEMINI_MODEL` | `gpt-6-luna` / `gemini-3.5-flash-lite` | With those providers |
| `OLLAMA_URL` / `OLLAMA_NUM_CTX` | `http://127.0.0.1:11434` / 16384 | Local models |
| `THINKER_MODEL` | provider default | Overrides the provider for background work only |
| `TTS_REPO` | Qwen3-TTS 0.6B Base 8-bit | Voice model |
| `STT` / `STT_LOCALE` | `apple` / system language + English | `parakeet` switches to the MLX model |
| `SAM_VOICE` | `male` | Default voice |
| `TTS_WHOLE_WAIT_MS` / `TTS_REST_WORDS` | 450 / 45 | Whole-reply voice generation |
| `VAD_SILENCE_MS` / `TURN_TIMEOUT_MS` | 250 / 1500 | End of turn |
| `BARGE_IN_MS` | 300 | Speech needed to interrupt Sam |
| `MEMORY` / `MEMORY_K` | 1 / 8 | Long-term memory on/off, memories recalled per turn |
| `JUDGE` / `JUDGE_TIMEOUT_MS` | 1 / 250 | Laya decisions and their live deadline |
| `VOICE_CONFIRM` | 1 | 0: approvals by click only |
| `MAX_PARALLEL` / `MAX_QUEUE` | 2 / 8 | Background tasks at once / waiting |
| `MEM_RESERVE_MB` / `TASK_TIMEOUT_MIN` | 1500 / 20 | Memory kept free before a task starts / hung-task timeout |

## Repository layout

```
app/
  Sam/               macOS app (Swift, SwiftUI + AppKit): island, menu bar, onboarding, overview
  Tests/E2E/         end-to-end suite, fixtures, virtual display
  Tests/Security/    attack regression tests
  runtime/           pinned Python packages the app installs on first run
  package.sh         builds dist/Sam.app and the dmg
  release.sh         signed release files (update zip, dmg, appcast, manifest) in dist/release/
server/              Rust agent server: voice loop, LLM clients, memory, tasks, browser, tools
  tools/             tool definitions (tool.json + instructions.md)
  persona_sam.md     Sam's persona
sidecar/
  speech_server.py   MLX speech: recognition, voice cloning, end of turn
  mind_server.py     memory embeddings and Laya decisions
  applestt/          Apple on-device speech recognition (Swift)
  computer/          "Sam Computer Use" helper: Accessibility, window reading (Swift)
  workspace/         invisible virtual display for Sam's browser (Swift)
voices/              voice profiles: reference clip, transcript, gain, fillers
sandbox/             container image for generated code
media/               logos
docs/                architecture and benchmarks
release/             model manifest, update key, version; how releases are made
tools/               voice preparation, manifest checker
.github/             CI, PR and issue templates
```

## Contributing

Pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, what a good PR describes (why, what changes for users, how it works, privacy impact), how to test each kind of change, and how to measure model, prompt and voice changes. Security issues: see [SECURITY.md](SECURITY.md).

## License

Sam is licensed under the [Apache License 2.0](LICENSE). The models it downloads have their own licenses (see [Models](#models)); LLM providers' terms apply to what you send them.
