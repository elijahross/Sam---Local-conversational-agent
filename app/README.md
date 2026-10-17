# Sam Island (macOS)

Sam's native Mac surface: an island in the notch (top centre on Macs without one), hidden until you call it.

## Run
```bash
./sidecar/computer/signing.sh          # once: local code-signing identity (keeps macOS grants across rebuilds)
./app/Sam/build.sh                     # → app/Sam/build/Sam.app
./run.sh                               # sidecars (models load on demand) + Rust server
open app/Sam/build/Sam.app
```

| Activation | How |
|---|---|
| ⌥Space hold | push-to-talk (release = send); needs no permission |
| ⌥Space tap | hands-free on/off |
| fn hold / fn double-tap | push-to-talk / hands-free; needs **Input Monitoring**. Also set System Settings › Keyboard › "Press 🌐 key to: Do Nothing" |
| hover the notch | peek; speak to start |
| Esc, click outside, ✕ | close |

| Permission | Why | Required |
|---|---|---|
| Microphone | listening while the island is open | yes |
| Accessibility | dictation insertion; screen context only if *Highlight and ask* / *Focus and ask* is on | optional |
| Input Monitoring | fn key | optional |

## Architecture
```
Sam.app (Swift, SwiftUI + AppKit)                  Rust server (server/)                 sidecars (MLX)
 NotchPanel  non-activating NSPanel, level             /ws  VAD · STT · LLM · TTS            speech :47801 STT + TTS
             .mainMenu+3, click-through outside        ptt / context / confirm / cards ◀──▶  mind   :47802 embeddings + Laya
 AudioIO     AVAudioEngine + voice processing (AEC)    thinker · tools · memory              (load on warm, unload when idle)
 Hotkeys     ⌥Space (Carbon), fn (CGEventTap)
 SamSocket connected only while the island is used ── connecting = warm the models
```

**Protocol additions (client → server):**
- `{"type":"ptt","state":"down|up"}`: a held key collects audio and the release ends the turn;
- `{"type":"context",app,window,selection,url}`: used for the next turn only, with the selection marked as untrusted data;
- `{"type":"confirm",action_id,decision,args?}`: goes through the same deterministic gate as "yes, go ahead".

**Server → client:** `{"type":"card",kind:"result|confirm|error",blocks,open,confirm}`. The blocks are header, text, list, stats, keyValue, badges and progress, at most 3 per card.

**Models stay out of RAM when idle.**
- **Idle** (models unloaded): about 0.8–1.1 GB, against 7.5 GB before. They load on the first `warm`, which a session start sends.
- **Loading on a warm page cache:**
  - STT 0.9–1.1 s
  - embeddings 0.7–0.9 s
  - Laya 0.6–0.9 s
  - TTS 2.8 s (5 s cold)
- **Unloading:** 5 min idle (speech) or 15 min (mind), 60 s after the last session closes, and earlier under memory pressure.

## Tests
`.venv/bin/python app/Tests/E2E/e2e.py [scenario ...]` (the `.venv` is created by `./run.sh`; quit Sam.app first, the suite refuses to run on ports your own Sam holds)
- **Stack:** starts the real stack and Sam.app in test mode (`--test`): a localhost-only control API on :47804, with speech injected from WAVs instead of the mic.
- **Results:** `e2e-results.jsonl`; screenshots of each state in `shots/` (both git-ignored).
- **Fixtures:** `fixtures/` (web pages, chat doubles, documents) and `vdisplay/` (a virtual display, compiled on first use).
- **Security:** `.venv/bin/python app/Tests/Security/attack_tests.py` replays 21 attacks against a running stack.
- **Test speech:** generated with `say`.
- **Test audio location:** copied to `/tmp/sam-e2e`, because the app has no Documents-folder access.
