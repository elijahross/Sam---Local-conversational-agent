# Benchmarks

Everything below was measured on one MacBook Pro (M4 Pro, 24 GB) between 2026-10-07 and 2026-10-16, using
the lab harnesses that used to live in `lab/` (they are now only in git history). Sample sizes are
small: dozens to a few hundred takes or questions per row. Where an LLM rated the output, the judge
is named, and judge noise is about ±0.3–0.6 points on a 1–10 scale. "Unmeasured" means no lab
produced the number.

## 1. Text-to-speech

**First bake-off (streaming, CustomVoice "Ryan", before voice cloning)**
<!-- source: lab/experiment-summary.md (55 experiments) -->

| Model | First audio | RTF p50 / max | Round-trip WER | Peak RAM |
|---|---|---|---|---|
| Qwen3-TTS 0.6B CustomVoice 8-bit | 77 ms | 0.26 / 0.28 | 1.2 % | 2.5 GB |
| Qwen3-TTS 1.7B CustomVoice 6-bit | 160 ms | 0.28 | 0.6 % | 3.4 GB |
| Qwen3-TTS 1.7B VoiceDesign 8-bit | 179 ms | 0.32 | 1.8 % | 3.8 GB |
| Chatterbox Turbo 8-bit | 220–232 ms | 0.48–0.55 | 0.6–1.8 % | 2.5 GB |
| Chatterbox fp16 | 1917 ms | 0.63 | 2.4 % | 4.4 GB |
| CSM-1B 8-bit | 803–894 ms | 0.70 / 1.47 | 8–11 % | 4.6 GB |

Qwen3-TTS was the fastest model here, about 2× faster than Chatterbox Turbo. CSM's worst RTF was above 1, which means audible gaps.

**Cloned voices (Base models), glitch lab, sam-female, 104 takes per row**
<!-- source: lab/voice-glitch/experiment-summary.md (34 experiments) -->

| Config | Glitch rate | Speaker sim | UTMOS | RTF |
|---|---|---|---|---|
| 0.6B Base 8-bit, streaming (old production) | 12.5 % | 0.974 | 4.01 | 0.36 |
| 0.6B, short pieces whole + retake guard | 3.8 % | 0.976 | 4.05 | 0.42 |
| 1.7B Base 8-bit, streaming | 11.5 % | 0.969 | 4.10 | 0.44 |
| 1.7B, no streaming decode | 1.9 % | 0.975 | 4.21 | 0.60 |
| 1.7B, pieces < 8 words whole + guard | 0.0 % | 0.975 | 4.19 | 0.55 |
| Pocket TTS (Kyutai 100M, mlx-audio) | 5.8 % | 0.979 | 4.00 | 0.1–0.2 |
| *Human reference recording* | – | – | 4.28 | – |

- Glitches cluster on pieces under five words: 12 of 36 takes, against 1 of 68 longer takes. Temperature, top-k/p, repetition penalty and context size moved the rate only within noise (7–17 %).
- Through the real sidecar, the 1.7B policy measured 1.9 % (1/52). The first audio for a whole short piece was 0.74 s, and 0.20 s for a streamed piece. The male voice measured 1.9 % on the 1.7B and 6.7 % on the 0.6B.
- Pocket TTS takes about 0.4 GB resident and produces first audio in 60–150 ms. It has no speaker encoder, so the retake guard can't run on it.

**Calm, consistent voice (cloning vs per-sentence style instructions)**
<!-- source: lab/voice/experiment-summary.md -->

| Config | Speaker sim min | Pitch spread between sentences | Pitch swing in a sentence | First audio |
|---|---|---|---|---|
| CustomVoice + per-sentence tone instructions | 0.950 | 4.30 st | 6.36 st | 76 ms |
| Clone 0.6B male, temp 0.6 | 0.967 | 2.99 st | 3.12 st | 102 ms |
| Clone 0.6B female, temp 0.6 | 0.961 | 2.37 st | 3.32 st | 108 ms |
| Clone 1.7B female, temp 0.6 | 0.947 | 1.95 st | 2.57 st | 180 ms |

The male reference speaker's own pitch swing is 3.3 st, so CustomVoice was overplaying at roughly twice that. Cloning roughly halved the swing. Loudness between voices went from 16 dB apart to 0.6 dB apart after reference normalisation and the streaming leveler.

**Whole-reply generation, temperature 0.45, golden reference (0.6B, 48–72 generations per condition)**
<!-- source: lab/voice-consistency/experiment-results.jsonl, results/label_auc.json; memory note lab-findings-2026-10-16-voice -->

| Condition (sam-female) | Pitch std across replies | Min pairwise sim | Sim to reference | Bad takes (user's ear) |
|---|---|---|---|---|
| Production split (first piece + rest) | 1.84–2.39 st | 0.936 | 0.986 | 7/24 (29 %) |
| Whole reply, temp 0.45 | 2.22–2.27 st | 0.92–0.93 | 0.982–0.984 | – |
| Whole reply, temp 0.45, golden reference | 1.64–1.80 st | 0.95 | 0.989 | 1/36 (3 %) |

- Generating the whole reply in one pass removes the restart between the first piece and the rest: that piece used to differ by about 2 st and 30 % in pace. It also starts about 0.6 s sooner.
- For the male voice, an automatically picked golden reference made things worse (23/24 bad takes). A reference rebuilt from takes the user had approved scored UTMOS 4.52, with 1/24 takes below the threshold.
- UTMOS matched human labels (AUC 1.0 female, 0.80 male). Speaker-embedding similarity did not: AUC 0.64 female, 0.39 male.

**Other voice findings**
<!-- source: lab/voice-joins/experiment-results.jsonl; lab/voice-prewarm/experiment-results.jsonl; memory notes tts-prewarm, voice-samples-retake -->

- First piece and the rest of the reply as one generation, 20 replies: glitches fell from 5 % to 0 %. A sampling sweep on top of that (context size 50, temp 0.5 / top-p 0.9, repetition penalty 1.1) gave 0 glitches and 0 voice switches in every condition. Repetition penalty 1.1 raised WER from 1.9 % to 3.1 %.
- Prewarm after a fresh model load: the first short whole piece dropped from 1.2 s to 0.64 s. In a warm session it gained nothing.
- Per-sentence scoring of the published 1.7B samples: 8 of 42 sentence takes (19 %) failed. Scores over the whole clip had rated those same clips clean.

**Failed LoRA fine-tune (Qwen3-TTS 0.6B, mlx-audio-train, 146 female / 116 male approved takes)**
<!-- source: memory note voice-finetune-result; lab/voice-sft/experiment-results.jsonl (smoke run only) -->

| Run | Holdout (30 sentences) result |
|---|---|
| Smoke test (4 sentences) | UTMOS 4.48, sim 0.988, WER 0 %: train, save, load and generate all worked, 2.6 GB RSS |
| 3 epochs, lr 1e-4 | No measurable change from the base model |
| 10 epochs, lr 3e-4 | Female voice broke: UTMOS 4.37 → 1.31, WER 203 % |

Fine-tuning was stopped. Voice quality now comes from the reference clip and the generation policy.

**Multilingual TTS (3 sentences × 8 EU languages, cloning the English sam-male reference)**
<!-- source: lab/multilingual/experiment-summary.md -->

| Model | Languages | UTMOS | Garbled sentences | Speaker sim | RTF | Weights |
|---|---|---|---|---|---|---|
| Qwen3-TTS 0.6B 8-bit | en de fr es it pt (nl/pl read as English) | 4.46 | 6/24 | 0.989 | 0.27 | 1.9 GB |
| Qwen3-TTS 1.7B 8-bit | same | 4.41 | 7/24 | 0.988 | 0.33 | 2.1 GB |
| VoxCPM2 2B 8-bit | all 8 | 3.78 | 0/24 | 0.982 | 1.03 | 3.0 GB |
| Pocket TTS 24-layer packs | en de fr es it pt | 3.52 | 2/18 | 0.981 | 0.59 | 0.6 GB/lang |
| Kokoro-82M (preset voices) | en fr es it pt | 3.67 | 1/15 | – | 0.18 | 0.35 GB |
| macOS `say` | all 8 | 2.58 | 0/24 | – | 0.11 | 0 |

Qwen3 sounded the most natural in every language but garbled about 1 sentence in 4 outside English. VoxCPM2 never garbled a sentence but runs at real time.

## 2. Speech recognition and turn-taking

<!-- source: lab/stt/summary.md; lab/experiment-summary.md; lab/speech-footprint/experiment-summary.md -->

| Test | Apple SpeechAnalyzer | Parakeet TDT 0.6B v3 (MLX) | Others |
|---|---|---|---|
| Clean, generic sentences (9 utterances) | 3.5 % WER, 110 ms | 4 %, 95 ms | – |
| Synthetic `say` set (10 utterances) | – | 2.4 %, 92 ms | whisper-large-v3-turbo 2.4 % / 430 ms; whisper-small 2.4 % / 141 ms |
| Dictation clean + SNR 5 (112 samples) | 10.9 %, 97 ms (117 ms busy) | 5.4 %, 55 ms (336 ms busy) | Fun-ASR-Nano 9.8 % |
| Resident RAM | 60–85 MB | ~1.7 GB | – |

<!-- source: lab/dictation/experiment-summary.md (47 experiments); lab/multilingual/experiment-summary.md -->

| Dictation set | Apple | Parakeet |
|---|---|---|
| Full set, 380 samples (names, numbers, 6 noise types) | 16.1 % | 11.5 % |
| Clean subset | 6.9 % | 3.1 % |
| Best stack: Parakeet + DeepFilterNet3 capped at 6 dB + phonetic contacts matcher | – | 9.3 % (clean 1.6 %) |
| German | 13.8 % | 15.2 % |
| Real helmet audio, 68 segments, fillers and bleed excluded* | 6.7 % | 3.5 % |
| Children, 60 segments* | 9.1 % | 5.2 % |
| FLEURS mean en/de/fr/es | 6.0 % | 4.4 % (Whisper turbo 4.5 %) |
| FLEURS mean over 8 EU languages | n/a (5 locales max) | 5.0 % (Whisper turbo 4.7 %) |

\* The reference transcript is Parakeet's own long-form draft, so these rows flatter Parakeet.

- Denoisers did not help. Uncapped DeepFilterNet3 and RNNoise raised the error rate on visor-up helmet audio to 35–37 %.
- The phonetic contacts/places corrector costs 0.6 ms with 0 % harm. LLM correction (MiniMax M2.7) got names right but took 4.7 s per utterance.

**End of turn** (236 natural pauses: 208 mid-sentence, 28 sentence ends)
<!-- source: lab/endpointing/experiment-summary.md -->

| Policy | Mid-sentence pauses cut | Latency at real ends |
|---|---|---|
| Fixed 200 ms silence (old) | 86.1 % | 200 ms |
| Fixed 2000 ms | 17.3 % | 2000 ms |
| smart-turn v3 at 200 ms, θ 0.9, 3 s fallback | 20.2 % | 200 ms |

A smart-turn check takes about 3 ms (8 MB model). Synthetic `say` audio is useless for this test: the model scored a sentence cut at a mid pause 0.97. Real use later moved the fallback to 1.5 s plus one re-check at 1 s; that change has no lab re-measurement.

## 3. LLM providers for the voice agent

<!-- source: lab/llm-cost/experiment-summary.md (minimax_m3.py, voice_turns.py, pointask_draft.sh) -->

| Model | Voice TTFT p50 | JSON plan (time, valid) | Point-and-ask drafting |
|---|---|---|---|
| MiniMax M2.7 (retired) | 2.42–2.46 s | 29.5 s, 1/2 | 1/2 |
| MiniMax M3 (standard/heavy tier) | 1.58 s | 3.9 s, 2/2 | unmeasured |
| MiniMax M3.1 Flash Preview, effort low (voice + light tier) | 0.49 s | 1.5 s, 2/2 | unmeasured |
| gpt-6-luna (none) | 1.00 s | – | 2/2, 1.16k tokens |
| gpt-5.4-nano (none) | 0.52 s | – | 2/2, 1.25k tokens |
| gpt-4.1-nano | 0.94 s | – | 0/2 |
| gpt-5-nano (minimal) | 10.7 s | – | – |

<!-- source: lab/experiment-summary.md; lab/conversation-flow-summary.md; lab/local-llm/experiment-summary.md -->

| Other models | Measurement |
|---|---|
| gemini-3.5-flash-lite (minimal thinking) | TTFT 641–716 ms; voice-to-voice p50 1.1–1.3 s in the first pipeline |
| gemini-3.5-flash / gemini-3.8-flash (low) | 935 ms / 2635 ms TTFT |
| qwen3.5:9b via Ollama (local) | First spoken word 1.8 s median (MiniMax M2.7 1.5 s); E2E 115/133 checks; roughly 1 in 3 multi-step drafts had wrong arguments before the thinker hardening, 2/2 point-and-ask after it |
| Claude (via the CLI) | Used only as a judge and in the wit lab; no latency benchmark |

**Prompt caching**
<!-- source: lab/llm-cost/prompt-caching.md; memory notes lab-findings-2026-10-15, llm-provider-choice -->

| Provider | Measurement |
|---|---|
| MiniMax (7.2k-token system prompt, 2nd/3rd call) | 99 % cached |
| OpenAI (same probe) | 99.8 % cached |
| MiniMax M3.1 Flash in an E2E conversation (turns ~10–40 s apart) | 86–92 % hit; warm first audio ~0.8–0.9 s |

**Cost:** a daily proactive check-in costs about 2.9k tokens, roughly €0.002 per day on MiniMax. No lab produced a per-turn cost for any other provider. <!-- source: lab/companion/experiment-summary.md -->

**Persona quality** (LLM judges; absolute 1–10 scores turned out insensitive, so the later rounds used pairwise judging)
<!-- source: lab/companion/experiment-summary.md; lab/companion/experiment-results.jsonl; memory note lab-findings-2026-10-15 -->

| Test | Result |
|---|---|
| Persona, 8 scenarios, Claude Sonnet judge | MiniMax M2.7 6.3–6.6 / 10; local qwen3.5:9b 4.1–5.1 |
| Wit/acumen pairwise, n=48, rewritten core + 4 examples vs base | Wit win 70 %, acumen 53 %, warmth equal in absolute scores |
| Same test: Gemini 3.5 Flash, gpt-6-luna, Claude Haiku, M3, extra prose rules | No gain; acumen never went past ~55 % with any prompt or model |
| Leading quiet users (n=10): code-side question budget + profile seed + no-coaching rule | leads 4.1 → 5.1, specific 3.8 → 5.7, not-interviewer 5.6 → 7.6 |
| Voice persona v2 vs v1 (Gemini Flash-Lite, two judges) | 9.44 vs 9.17 |

Tool-use accuracy was never measured as a benchmark of its own. The closest numbers are the point-and-ask pass rates above and the E2E check counts.

## 4. Memory, knowledge and the Laya judge

<!-- source: lab/memory/memory-summary.md; lab/memory/experiment-results.jsonl (mem-087..116) -->

| Embedder (no decay, soft supersession) | Recall@5 | Recall@10 | MRR |
|---|---|---|---|
| Qwen3-Embedding-0.6B 8-bit @1024 | 0.915 | 0.994 | 0.820 |
| Qwen3-Embedding-0.6B 4-bit DWQ @1024 (shipped) | 0.915 | 0.994 | 0.815 |
| Qwen3-Embedding-0.6B 8-bit @512 | 0.935 | 0.988 | 0.824 |
| EmbeddingGemma 2 (earlier set) | 0.945 | – | – |

| Shipped decay (z-scored relevance + 0.5·recency) vs no decay | No decay | v2 decay |
|---|---|---|
| Recall@5 / Recall@10 | 0.868 / 0.952 | 0.904 / 0.989 |
| "Recently / lately" questions | 0.34 | 0.78 |
| Old facts still true / history questions | 0.8 / 0.667 | 0.8 / 0.667 |

- End-to-end answers (36 questions × 3 runs): two-layer prompt 80.6 % correct vs 78.7 % for the old layout.
- On a long conversation, the rolling summary recalled 7–8 of 8 facts, against 1 of 8 with a hard 24-message cut.
- Live recall takes 10–31 ms per turn. Changing k between 6, 8 and 10 gave 64.8 / 65.7 / 66.7 %, which is within noise. An absolute cosine floor can't separate small talk from questions.
- Cross-source knowledge index, 68 documents and 25 questions: the production server scored 0.947 fact coverage, 0 stale answers and 0.71 citation recall. <!-- source: lab/knowledge/experiment-summary.md -->

**Laya micro-judge (ModernBERT-large fine-tune, 13 tasks)**
<!-- source: lab/laya/laya-summary.md -->

| Task | Accuracy (3-model-unanimous test set; hand-labelled set in brackets) |
|---|---|
| route / kind / end of turn | 97.4 % (91) / 93.1 % (91) / 89.1 % (90) |
| secret / supersession / low-value session | 99.2 % (94) / 98.8 % (96) / 93.8 % (96) |
| DOM relevance / page state / action ok | 96.7 % (76 cross-site) / 100 % / 96.2 % |
| Mean over 13 tasks | 96.4 %, one 811 MB checkpoint |

- A Laya judgment takes 20–28 ms on MPS, against 615–680 ms for a Gemini Flash-Lite call.
- In the first round, Gemini Flash-Lite scored higher than fine-tuned Laya on every task, for example routing 100 % vs 88%.
- Route, kind and end of turn run in parallel with memory recall, about 80 ms in total.
- Laya can't check whether one exact entry appears in a listing page (75 %).

## 5. Moving the Python sidecars to Rust

<!-- source: lab/rust-sidecar/experiment-summary.md -->

| Model | Rust route | Parity | Speed | Verdict |
|---|---|---|---|---|
| smart-turn v3 | ort + Rust log-mel | 19/20 vs the official model (MLX port: 17/20) | 8 ms (MLX 3.9 ms) | Portable |
| Parakeet v3 | parakeet-rs int8 | WER 8.9 % vs 5.1 % | 53 ms vs 55 ms | Works, loses accuracy |
| Qwen3-Embedding 0.6B | int8 ONNX | cosine 0.94 to the MLX vectors | 10 ms/text | Portable, memory needs re-embedding |
| Laya judge | ONNX fp32 / int8 | 100 % route agreement | 127 / 93 ms vs 52 ms on MPS | Portable on paper |
| Qwen3-TTS 0.6B | candle `qwen_tts`, bf16 | 7.7 % glitches, UTMOS 4.11 | RTF 0.98 vs 0.36, 3.2 GB | Not yet |
| Pocket TTS | candle | Blocked by gated weights | – | – |

The ports of the judge, embeddings and smart-turn shipped and were reverted the same day. Under real load the CPU judge missed its live budget, while torch on MPS did not.

## 6. Resource footprint

<!-- source: lab/speech-footprint/experiment-summary.md; lab/perf/experiment-summary.md; README.md; memory notes voice-agent-roadmap, voice-consistency -->

| TTS under synthetic load (4 CPU spinners + 3 GB + GPU loop) | On disk | Idle footprint | RTF idle → busy | Stalls (34 s, 0.3 s buffer) |
|---|---|---|---|---|
| Qwen3-TTS 0.6B 8-bit | 1.9 GB | 3.4 GB | 0.27 → 1.74 | 130 stalls, 19.6 s of gaps |
| Pocket TTS | 229 MB | 2.5 GB (includes harness buffers) | 0.087 → 0.43 | 0 |
| Kokoro-82M | 349 MB | 1.2 GB | 0.037 → 0.14 | 0 |
| macOS `say` | 0 | 68 MB | 0.17 → 0.20 | 0 |

| Stack | Measurement |
|---|---|
| Idle, models unloaded | ~0.7–0.9 GB (was 7.5 GB with models always loaded) |
| First pipeline, sidecar + server | 2.6 GB + 56 MB |
| Burst of 16 tasks (2 run, 8 queue, 6 refused) | Sam peak 3.6 GB + Chrome 2.3 GB |
| Voice weights: Tiny / Light / Steady | ~0.3–0.4 / 1.9 / 2.4 GB |
| Install footprint | App ~0.9 GB, runtime ~1.2 GB, models ~2.2 GB; first-run model download ~4.5 GB |

Under load the stutter comes from running out of compute, not from model size: 4-bit Qwen still reached RTF 1.49 busy. Install and download sizes are what the README documents. No lab measured download time.

## 7. Known limitations

- **Voice:** short pieces (1–4 words) are where clones glitch. Lone "Mm" or "Hmm" never clones cleanly. The 0.6B with the guard still glitches on 3.8 % of takes, mostly garbled words, which the guard can't detect. The male voice depended on a reference built from takes the user approved.
- **Load:** Qwen3-TTS can't keep real time on a busy Mac (RTF 1.74). Pocket or Kokoro can, but at lower naturalness.
- **Fine-tuning:** LoRA either changed nothing or destroyed the voice. Full SFT needs CUDA.
- **Other languages:** Qwen3 garbles about 1 in 4 sentences outside English. VoxCPM2 is reliable but runs at real time and isn't wired in.
- **STT:** at 0 dB SNR every stack stays at 22–30 % WER. Parakeet is worse than Apple in German. The helmet and children's references are machine drafts.
- **Turn-taking:** slow, hesitant speakers still get cut (40 of 157 pauses for one speaker). There are no German or accent recordings yet. The shipped 1.5 s fallback has not been re-measured.
- **LLM:** M3.1 Flash over-splits spoken requests, so splitting runs on M3. Acumen plateaued around 55 % pairwise. The local 9B can't carry the persona and invents facts. Thinking mode breaks the JSON planner.
- **Memory:** partial supersessions are still missed (2/8 in v1). Questions like "what did we talk about in our first chat" rank the right memory 8th to 16th by cosine. The question sets are small (20–36 questions).
- **Unmeasured:** phone RTF for Pocket TTS, tool-use accuracy as its own benchmark, per-turn cost by provider, and listening tests for the multilingual clips.
