"""Speech sidecar: STT (parakeet-mlx) + streaming TTS (mlx-audio) over one local WebSocket.

Protocol (ws://127.0.0.1:47801):
  binary op 3 + u32 id + int16 PCM (16 kHz) -> {"op":"turn","id","p"}: smart-turn v3 end-of-turn probability
  client -> server
    binary  [u8 1][u32 id LE][pcm16le 16 kHz mono]   transcribe utterance
    text    {"op":"tts","id":n,"text":"...","voice":"sam-female"?,"instruct":"..."?}   synthesize, streamed
            voice: a profile in voices/<id>/ (ref.wav + ref.txt), cloned by the Base model so every
            sentence sounds the same; instruct only applies to CustomVoice models
    text    {"op":"cancel","id":n}                     stop a running tts job (barge-in)
    text    {"op":"warm","models":["stt","tts"]?}      start loading now (UI opened); returns at once
    text    {"op":"prewarm","voice":"sam-female"?}    the user started speaking: run a short throwaway
                                                       generation in that voice so the reply's first
                                                       pieces meet a warm model (once per voice and
                                                       load; cut short by the first real piece)
    text    {"op":"status"}                            -> {"op":"status","models":{...},"rss_mb":..}
  server -> client
    text    {"op":"ready","sample_rate":24000,"tts":repo,"stt":repo,"voices":[...],"voice":default}
    text    {"op":"stt","id":n,"text":"...","ms":..}
    binary  [u8 2][u32 id LE][pcm16le @ sample_rate]  tts audio chunk
    text    {"op":"tts_done","id":n,"ttfa_ms":..,"cancelled":bool}
All GPU work runs on one worker thread so STT and TTS never contend inside MLX.
Models load on demand and unload when idle (model_manager.py): 5 min after last use, 60 s after
the last client disconnected, earlier under memory pressure.
"""
import argparse, asyncio, json, os, pathlib, struct, time
from concurrent.futures import ThreadPoolExecutor
import numpy as np, mlx.core as mx, websockets
from model_manager import ModelManager, start_reaper, rss_mb
from mlx_audio.tts.utils import load_model

_env = pathlib.Path(__file__).resolve().parent.parent / ".env"  # HF_TOKEN for gated repos (CSM)
if _env.exists():
    for _l in _env.read_text().splitlines():
        if "=" in _l and not _l.startswith("#"):
            _k, _v = _l.split("=", 1); os.environ.setdefault(_k.strip(), _v.strip())

ap = argparse.ArgumentParser()
ap.add_argument("--host", default="127.0.0.1"); ap.add_argument("--port", type=int, default=47801)
ap.add_argument("--stt", default="mlx-community/parakeet-tdt-0.6b-v3", help='parakeet repo, or "apple" (macOS 26 on-device recognition: no model in this process)')
ap.add_argument("--stt-locale", default=os.environ.get("STT_LOCALE", ""), help="apple STT language, e.g. en_US / de_DE (default: system)")
# Base = voice cloning from voices/<id>/ (voice lab: consistent across sentences, natural pitch range).
ap.add_argument("--tts", default="mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit")
ap.add_argument("--turn-model", default="mlx-community/smart-turn-v3", help="end-of-turn detector repo")
ap.add_argument("--voices", default=str(pathlib.Path(__file__).resolve().parent.parent / "voices"), help="voice profiles dir")
ap.add_argument("--voice", default="sam-male", help="default voice profile")
ap.add_argument("--sr", type=int, default=24000, help="TTS sample rate (known without loading the model)")
ap.add_argument("--idle", type=int, default=300, help="unload a model after this many idle seconds")
ap.add_argument("--eager", action="store_true", help="load both models at start (old behaviour)")
# temperature 0.6: less pitch swing and steadier delivery than the default 0.9 (voice lab);
# 0.45 (voice-consistency lab, 2026-10-16): the fewest outlier voices across replies.
ap.add_argument("--tts-gen", default='{"language":"english","temperature":0.45,"streaming_interval":0.25}', help="JSON kwargs for model.generate")
# voice-glitch lab (2026-10-14): glitches (speaker flips, garbled or runaway takes) sit on pieces
# under five words; the streaming decode is the main cause. Short pieces are synthesized whole,
# and the shortest ones are checked against the voice's reference (speaker embedding, pitch
# direction, length) and taken again when they drift.
ap.add_argument("--short-words", type=int, default=8, help="pieces with fewer words are synthesized whole (no streaming decode); the opening piece of a reply streams from 5 words up")
ap.add_argument("--guard", type=int, default=2, help="retries for pieces under 5 words that drift from the reference voice (0 = off)")
args = ap.parse_args()

GPU = ThreadPoolExecutor(max_workers=1, thread_name_prefix="mlx")
TTS_GEN = json.loads(args.tts_gen); SR = args.sr

class AppleSTT:
    """sidecar/applestt: Apple SpeechAnalyzer over stdin/stdout (the model lives in the OS)."""
    BIN = pathlib.Path(__file__).resolve().parent / "applestt/build/applestt"
    def __init__(self, locale):
        import subprocess
        cmd = [str(self.BIN)] + (["--locale", locale] if locale else [])
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        hello = json.loads(self.p.stdout.readline() or b'{"error":"applestt exited"}')
        if not hello.get("ready"):
            raise RuntimeError(f"apple stt: {hello.get('error')}")
        self.locale = hello.get("locale")
    def transcribe(self, pcm: bytes) -> str:
        self.p.stdin.write(struct.pack("<I", len(pcm)) + pcm); self.p.stdin.flush()
        r = json.loads(self.p.stdout.readline() or b'{"error":"applestt exited"}')
        if "error" in r:
            raise RuntimeError(r["error"])
        return r["text"]
    def close(self):
        self.p.kill()

def _stt_transcribe(model, pcm: bytes) -> str:
    if isinstance(model, AppleSTT):
        return model.transcribe(pcm)
    audio = mx.array(np.frombuffer(pcm, dtype=np.int16).astype(np.float32) / 32768.0)
    from parakeet_mlx.audio import get_logmel  # only for the MLX recognizer (Apple STT needs none)
    mel = get_logmel(audio, model.preprocessor_config)
    return model.generate(mel)[0].text.strip()

def _load_stt():  # runs on the GPU thread (ModelManager executor); warm-up compiles kernels
    if args.stt == "apple":
        return AppleSTT(args.stt_locale)
    from parakeet_mlx import from_pretrained
    m = from_pretrained(args.stt); _stt_transcribe(m, np.zeros(16000, np.int16).tobytes()); return m

VOICES = sorted(p.name for p in pathlib.Path(args.voices).glob("*") if (p / "ref.wav").exists() and (p / "ref.txt").exists())
_refs: dict = {}

def _profile(v):
    """voice.json gain (all voices play at the same level) + verified filler takes, from
    voice lab. Missing files = gain 0 dB, no cached fillers."""
    d = pathlib.Path(args.voices) / v
    meta = json.loads((d / "voice.json").read_text()) if (d / "voice.json").exists() else {}
    idx = json.loads((d / "fillers.json").read_text()) if (d / "fillers.json").exists() else {}
    return {"gain": 10 ** (meta.get("gain_db", 0.0) / 20), "fillers": {" ".join(t.split()): d / f["file"] for t, f in idx.items()}}
PROFILES = {v: _profile(v) for v in VOICES}

TARGET_DB = -20.0  # active-speech level every voice is played at (= prepare_voices.TARGET_DB)

class Leveler:
    """Streaming loudness control for one voice on one connection: the voice's fixed gain, then a
    slow correction (at most ±4 dB, ramped across each chunk) toward TARGET_DB measured on speech
    frames only. State carries over between sentences, so the level never jumps."""
    def __init__(self, gain):
        self.base = gain; self.corr = 1.0
    def __call__(self, a):
        a = a * self.base
        f = 480; n = len(a) // f   # 20 ms frames
        if n:
            r = np.sqrt((a[: n * f].reshape(n, f) ** 2).mean(1) + 1e-12)
            act = r[20 * np.log10(r) > -45]
            if act.size >= 3:
                lvl = 20 * np.log10(np.sqrt((act ** 2).mean()))
                want = 10 ** (np.clip(TARGET_DB - lvl, -4, 4) / 20)
                new = 0.6 * self.corr + 0.4 * want
                a = a * np.linspace(self.corr, new, len(a), dtype=np.float32)
                self.corr = new
            else:
                a = a * self.corr
        return _limit(a)

def _active_db(a):
    f = 480; n = len(a) // f
    if not n: return TARGET_DB
    r = np.sqrt((a[: n * f].reshape(n, f) ** 2).mean(1) + 1e-12)
    act = r[20 * np.log10(r) > 20 * np.log10(r.max()) - 30]
    return float(20 * np.log10(np.sqrt((act ** 2).mean())))

def _limit(a):
    """Soft limiter: no clipping, however loud a piece comes out."""
    knee = 0.85
    over = np.abs(a) > knee
    if over.any(): a[over] = np.sign(a[over]) * (knee + (1 - knee) * np.tanh((np.abs(a[over]) - knee) / (1 - knee)))
    return a
def _is_pocket(model):
    return "pocket_tts" in type(model).__module__

def _voice_kw(model, voice, instruct):
    """(generate() kwargs, voice name) for one piece: a cloned voice profile (Base model, or Pocket
    TTS from the same reference clip) or a CustomVoice preset plus style instruction. Base models
    take no instruction (tone tags ignored)."""
    if _is_pocket(model):
        v = voice if voice in VOICES else args.voice
        if v not in _refs:
            from mlx_audio.utils import load_audio
            d = pathlib.Path(args.voices) / v
            _refs[v] = (load_audio(str(d / "ref.wav"), sample_rate=24000), (d / "ref.txt").read_text().strip())
        # Pocket takes the reference audio only; its sampling is fixed (temperature from --tts-gen applies).
        return {"ref_audio": _refs[v][0], **({"temperature": TTS_GEN["temperature"]} if "temperature" in TTS_GEN else {}), "streaming_interval": TTS_GEN.get("streaming_interval", 0.25)}, v
    if getattr(model.config, "tts_model_type", "") != "base":
        kw = {"voice": "Ryan", **TTS_GEN, **({"instruct": instruct} if instruct else {})}
        return kw, kw["voice"]
    v = voice if voice in VOICES else args.voice
    if v not in _refs:  # same array object every time -> the model's ICL cache reuses the ref codes
        from mlx_audio.utils import load_audio
        d = pathlib.Path(args.voices) / v
        _refs[v] = (load_audio(str(d / "ref.wav"), sample_rate=24000), (d / "ref.txt").read_text().strip())
    return {**TTS_GEN, "ref_audio": _refs[v][0], "ref_text": _refs[v][1]}, v

SPOKEN: set = set()  # (id(model), voice) that already ran a take since the load (see prewarm)
def _load_tts():
    m = load_model(args.tts)
    for _ in m.generate(text="Hello there.", stream=True, **_voice_kw(m, args.voice, None)[0]): pass
    SPOKEN.add((id(m), args.voice))
    if m.sample_rate != SR: print(f"warning: TTS sample rate {m.sample_rate} != --sr {SR}", flush=True)
    return m

STT = ModelManager("stt", _load_stt, args.idle, GPU, release=lambda m: m.close() if isinstance(m, AppleSTT) else None)
TTS = ModelManager("tts", _load_tts, args.idle, GPU)
CLIENTS = {"n": 0, "closed_at": 0.0}
def _busy():  # an open session keeps models (unless pressure is critical); 60 s grace after the last one
    return CLIENTS["n"] > 0 or time.monotonic() - CLIENTS["closed_at"] < 60
start_reaper([STT, TTS], busy=_busy)
if args.eager:
    STT.warm(); TTS.warm(); STT.ready.wait(); TTS.ready.wait()
    # smart-turn (end-of-turn) too: its first download/load used to land inside the first turn.
    try: _turn_prob(b"\x00" * 32000)
    except Exception as e: print(f"[models] smart-turn warm-up failed: {e}", flush=True)

def _transcribe(pcm: bytes) -> str:
    return _stt_transcribe(STT.model, pcm)

_TURN = {}
def _turn_prob(pcm: bytes) -> float:
    """smart-turn v3 (pipecat, 8M params, ~5 ms): has the speaker finished their turn? Input: the
    turn's audio so far (16 kHz int16, last 8 s), including the silence heard so far."""
    if "m" not in _TURN:
        from mlx_audio.vad.utils import load_model
        _TURN["m"] = load_model(args.turn_model)
    a = np.frombuffer(pcm, dtype=np.int16).astype(np.float32) / 32768
    return float(_TURN["m"].predict_endpoint(a[-8 * 16000:], sample_rate=16000).probability)
print(f"ready: stt={args.stt} tts={args.tts} voices={VOICES} sr={SR} (models load on demand, idle unload {args.idle} s)", flush=True)

def _whole(model, text, kw):
    """One complete take (no streaming decode) as float32 at the model's rate."""
    chunks = [np.asarray(r.audio, dtype=np.float32).reshape(-1) for r in model.generate(text=text, stream=False, **kw)]
    return np.concatenate(chunks) if chunks else np.zeros(0, np.float32)

_voice_ref: dict = {}   # voice -> (speaker embedding, median pitch in semitones) of its reference clip
def _f0_st(a, sr=24000, fmin=60.0, fmax=400.0, frame=1024, hop=256, thresh=0.15):
    """Median pitch of the voiced frames in semitones above 100 Hz (None when too little voicing).
    Plain-numpy YIN: the app's runtime has no numba (setup removes it), so librosa.pyin crashed the
    drift guard in every installed app (2026-10-16: short pieces dropped, glitches never retaken)."""
    x = np.asarray(a, dtype=np.float32).reshape(-1)
    if len(x) < frame * 2: return None
    tmin, tmax = int(sr / fmax), min(int(sr / fmin), frame - 1)
    n = 1 + (len(x) - frame) // hop
    fr = np.lib.stride_tricks.as_strided(x, shape=(n, frame), strides=(x.strides[0] * hop, x.strides[0]))
    fr = fr[np.sqrt((fr ** 2).mean(axis=1)) > 0.01]          # skip silence
    if len(fr) < 5: return None
    # YIN difference d(tau) = sum_{j<W} (x_j - x_{j+tau})^2 = E(x[0:W]) + E(x[tau:tau+W]) - 2 r(tau),
    # r by FFT cross-correlation of the first W samples against the frame.
    w = frame // 2
    r = np.fft.irfft(np.fft.rfft(fr, 2 * frame, axis=1) * np.conj(np.fft.rfft(fr[:, :w], 2 * frame, axis=1)), axis=1)[:, :w + 1]
    c = np.concatenate([np.zeros((len(fr), 1), np.float32), np.cumsum(fr.astype(np.float64) ** 2, axis=1)], axis=1)
    taus = np.arange(w + 1)
    d = c[:, w][:, None] + (c[:, taus + w] - c[:, taus]) - 2 * r
    d[:, 0] = 0
    cmnd = d[:, 1:] * np.arange(1, w + 1) / np.maximum(np.cumsum(d[:, 1:], axis=1), 1e-12)
    out = []
    for row in cmnd:
        seg = row[tmin - 1:tmax]
        idx = np.where(seg < thresh)[0]
        if not len(idx): continue
        i = idx[0]
        while i + 1 < len(seg) and seg[i + 1] < seg[i]: i += 1   # down to the local minimum
        tau = i + tmin
        # parabolic interpolation around the minimum
        if 1 <= i < len(seg) - 1:
            y0, y1, y2 = seg[i - 1], seg[i], seg[i + 1]; den = y0 - 2 * y1 + y2
            tau = tau + (0.5 * (y0 - y2) / den if den else 0)
        out.append(sr / tau)
    v = np.array(out)
    v = v[(v >= fmin) & (v <= fmax)]
    return float(np.median(12 * np.log2(v / 100.0))) if len(v) >= 5 else None

def _drift(model, vname, ref_audio, a, words):
    """Why a short take doesn't sound like the reference voice ("" when it does): speaker
    similarity below 0.95 (0.92 under a second), pitch 8+ semitones the wrong way for the voice
    or 12+ either way, or a runaway / cut length per word (voice-glitch lab thresholds)."""
    if vname not in _voice_ref:
        e = np.asarray(model.extract_speaker_embedding(ref_audio), dtype=np.float32).reshape(-1)
        _voice_ref[vname] = (e / np.linalg.norm(e), _f0_st(np.asarray(ref_audio, dtype=np.float32).reshape(-1)))
    ref_e, ref_f0 = _voice_ref[vname]
    secs = len(a) / 24000; spw = secs / max(1, words)
    # One or two words carry their own pause ("Hm, okay," runs 1.6 s): a looser ceiling there, or
    # every opener would be taken three times (E2E 2026-10-14: 3.3 s to first audio).
    if spw > (1.3 if words <= 2 else 0.9): return f"runaway {spw:.2f} s/word"
    if spw < 0.12: return f"cut {spw:.2f} s/word"
    e = np.asarray(model.extract_speaker_embedding(mx.array(a)), dtype=np.float32).reshape(-1); sim = float(e / np.linalg.norm(e) @ ref_e)
    if sim < (0.95 if secs >= 1.0 else 0.92): return f"speaker sim {sim:.3f}"
    f = _f0_st(a)
    if f is not None and ref_f0 is not None:
        d = f - ref_f0
        low_voice = vname.endswith("male") and not vname.endswith("female")
        if (not low_voice and d < -8) or (low_voice and d > 8) or abs(d) > 12: return f"pitch {d:+.1f} st"
    return ""

_END = object()
def _next(gen):
    """Next audio chunk as float32 numpy. Must run on the GPU thread: MLX streams are
    per-thread and some models (Chatterbox) yield lazy arrays that evaluate on conversion."""
    try: r = next(gen)
    except StopIteration: return _END
    return np.clip(np.array(r.audio, dtype=np.float32).reshape(-1), -1, 1)

# Prewarm (voice-prewarm lab, 2026-10-15): the first pieces after the model loads are the cold
# ones. On a fresh load the opener's first audio came 25 ms later and a short whole piece took
# 1.2 s instead of 0.66 s (the first voice check pays for the pitch tracker and the reference
# embedding). While the user speaks, a throwaway take in the reply's voice runs once per voice
# and load; in a running session it gained nothing, so it isn't repeated.
WARM_TEXT = "Okay, I'm right here, give me just a second."
WARM_SHORT = "Sure, okay."
PREWARM = {"task": None, "epoch": 0, "runs": 0, "skipped": 0, "aborted": 0, "stt": 0}

async def _gpu_free():
    """A pending transcription goes first: the prewarm only takes the next GPU step after it."""
    while PREWARM["stt"] > 0: await asyncio.sleep(0.02)

def _stop_prewarm():
    """A reply piece has the GPU: any prewarm asked for before it is stale. The turn's STT
    doesn't stop it: it slots in between prewarm steps on the GPU queue, and the prewarm keeps
    going while the LLM thinks (E2E: a first turn after a load ended it before its first chunk)."""
    PREWARM["epoch"] += 1

def _ref_voice(model, vname, ref_audio):
    """The voice check's reference (embedding on the GPU thread, pitch on any thread)."""
    e = np.asarray(model.extract_speaker_embedding(ref_audio), dtype=np.float32).reshape(-1)
    return e / np.linalg.norm(e)

async def prewarm(voice):
    loop = asyncio.get_running_loop(); t0 = time.perf_counter(); e0 = PREWARM["epoch"]
    stale = lambda: PREWARM["epoch"] != e0
    vname = voice if voice in VOICES else args.voice
    if TTS.state != "ready":
        TTS.warm()
        await loop.run_in_executor(None, TTS.ready.wait)
    # The voice check's pitch tracker (librosa import + JIT, CPU only, ~0.5 s the first time)
    # warms on its own thread, never on the GPU queue; after the load, which it would slow down.
    ref_f0 = None
    if "-Base" in args.tts and vname not in _voice_ref:
        from mlx_audio.utils import load_audio
        ref_f0 = loop.run_in_executor(None, lambda: _f0_st(np.asarray(load_audio(str(pathlib.Path(args.voices) / vname / "ref.wav"), sample_rate=24000), dtype=np.float32).reshape(-1)))
    model = TTS.model
    if model is None or stale(): return
    kw, vname = _voice_kw(model, voice, None)
    guard = "ref_audio" in kw and hasattr(model, "extract_speaker_embedding") and vname not in _voice_ref
    if (id(model), vname) in SPOKEN and not guard:
        PREWARM["skipped"] += 1; return
    n = 0
    if guard:
        # Short pieces (often a reply's first real one) are made whole and checked against the
        # voice: that path first, then the streamed one.
        await _gpu_free()
        a = await loop.run_in_executor(GPU, lambda: _whole(model, WARM_SHORT, kw))
        if not stale():
            await _gpu_free()
            e = await loop.run_in_executor(GPU, lambda: _ref_voice(model, vname, kw["ref_audio"]))
            _voice_ref.setdefault(vname, (e, await ref_f0 if ref_f0 is not None else _f0_st(np.asarray(kw["ref_audio"], dtype=np.float32).reshape(-1))))
    if (id(model), vname) not in SPOKEN and not stale():
        gen = await loop.run_in_executor(GPU, lambda: model.generate(text=WARM_TEXT, stream=True, **kw))
        while not stale():
            await _gpu_free()
            a = await loop.run_in_executor(GPU, _next, gen)  # chunk by chunk: STT, smart-turn and a real piece slot in
            if a is _END: SPOKEN.add((id(model), vname)); break
            n += 1
    if stale(): PREWARM["aborted"] += 1
    PREWARM["runs"] += 1; TTS.touch()
    print(f"prewarm voice={vname} chunks={n}{' guard' if guard else ''} ms={round((time.perf_counter() - t0) * 1000)}{' aborted' if stale() else ''}", flush=True)

def start_prewarm(voice):
    t = PREWARM["task"]
    if t is not None and not t.done(): return
    PREWARM["task"] = asyncio.get_running_loop().create_task(prewarm(voice))

async def tts_job(ws, jid, text, voice, instruct, cancelled: set, levelers: dict, first=False):
    loop = asyncio.get_running_loop(); t0 = time.perf_counter(); ttfa = None
    vname = voice if voice in VOICES else args.voice
    prof = PROFILES.get(vname, {"gain": 1.0, "fillers": {}})
    # Verified filler takes are clone takes: only valid while a Base (clone) model is in use.
    cached = prof["fillers"].get(" ".join(text.split())) if "-Base" in args.tts else None
    if cached is not None:  # stored at the target level; plays even while the model still loads
        import soundfile as sf
        a, _ = sf.read(cached, dtype="float32")
        a = _limit(a * 10 ** (np.clip(TARGET_DB - _active_db(a), -6, 6) / 20))  # each take at the speech level
        for i in range(0, len(a), SR // 4):
            if jid in cancelled: break
            await ws.send(struct.pack("<BI", 2, jid) + (a[i:i + SR // 4] * 32767).astype(np.int16).tobytes())
        print(f"tts id={jid} voice={vname} cached filler", flush=True)
        await ws.send(json.dumps({"op": "tts_done", "id": jid, "ttfa_ms": round((time.perf_counter() - t0) * 1000), "cancelled": jid in cancelled}))
        return
    _stop_prewarm()  # a real piece has the GPU (a cached filler above doesn't need it)
    model = await loop.run_in_executor(None, TTS.get)  # waits only if it is still loading
    kw, vname = _voice_kw(model, voice, instruct)
    level = levelers.setdefault(vname, Leveler(prof["gain"]))
    words = len(text.split())
    # Short pieces are made whole (voice-glitch lab: the streaming decode flips the speaker on
    # them); the opening piece of a reply streams from five words up so the first audio stays early.
    stream = kw.pop("stream", True) and (words >= args.short_words or (first and words >= 5))
    retries = 0
    if not stream and "ref_audio" in kw:
        # Whole take, checked against the reference voice; a drifted take is made again.
        a = await loop.run_in_executor(GPU, lambda: _whole(model, text, kw))
        while args.guard and words < 5 and retries < args.guard and jid not in cancelled and hasattr(model, "extract_speaker_embedding"):
            try:
                why = await loop.run_in_executor(GPU, lambda: _drift(model, vname, kw["ref_audio"], a, words))
            except Exception as e:  # a broken check must never cost the piece
                print(f"tts id={jid} drift check failed, take kept: {e!r}", flush=True); why = ""
            if not why: break
            retries += 1; print(f"tts id={jid} voice={vname} retake {retries}: {why}", flush=True)
            a = await loop.run_in_executor(GPU, lambda: _whole(model, text, kw))
        if jid not in cancelled and a.size:
            ttfa = (time.perf_counter() - t0) * 1000; TTS.touch()
            for i in range(0, len(a), SR // 4):
                await ws.send(struct.pack("<BI", 2, jid) + (level(a[i:i + SR // 4]) * 32767).astype(np.int16).tobytes())
    else:
        gen = await loop.run_in_executor(GPU, lambda: model.generate(text=text, stream=True, **kw))
        while jid not in cancelled:
            a = await loop.run_in_executor(GPU, _next, gen)  # one chunk per GPU slot -> STT can interleave
            if a is _END: break
            if not a.size: continue
            if ttfa is None: ttfa = (time.perf_counter() - t0) * 1000
            TTS.touch()
            await ws.send(struct.pack("<BI", 2, jid) + (level(a) * 32767).astype(np.int16).tobytes())
    SPOKEN.add((id(model), vname))
    print(f"tts id={jid} voice={vname} ttfa_ms={round(ttfa or 0)}{' whole' if not stream else ''}{f' retakes={retries}' if retries else ''}", flush=True)
    await ws.send(json.dumps({"op": "tts_done", "id": jid, "ttfa_ms": round(ttfa or 0), "cancelled": jid in cancelled}))

async def handler(ws):
    # A connection counts as a client (keeps models loaded) only once it sends real work;
    # status probes must not keep the models alive. Loading starts on {"op":"warm"}.
    counted = False
    def count():
        nonlocal counted
        if not counted: CLIENTS["n"] += 1; counted = True
    await ws.send(json.dumps({"op": "ready", "sample_rate": SR, "tts": args.tts, "stt": args.stt, "voices": VOICES, "voice": args.voice,
                              "fillers": {v: sorted(p["fillers"]) for v, p in PROFILES.items()} if "-Base" in args.tts else {}}))
    cancelled: set = set(); tts_queue: asyncio.Queue = asyncio.Queue(); levelers: dict = {}
    async def tts_worker():  # sentences of one reply must play in order -> serial per connection
        while True:
            jid, text, voice, instruct, first = await tts_queue.get()
            if jid in cancelled:
                await ws.send(json.dumps({"op": "tts_done", "id": jid, "ttfa_ms": 0, "cancelled": True})); continue
            try:
                await tts_job(ws, jid, text, voice, instruct, cancelled, levelers, first)
            except websockets.ConnectionClosed:
                return
            except Exception:  # one bad piece must not silence the rest of the session
                import traceback; traceback.print_exc()
                await ws.send(json.dumps({"op": "tts_done", "id": jid, "ttfa_ms": 0, "cancelled": False, "error": True}))
    worker = asyncio.create_task(tts_worker())
    try:
        async for msg in ws:
            if isinstance(msg, bytes):
                count()
                op, jid = struct.unpack_from("<BI", msg)
                if op == 1:
                    t = time.perf_counter(); PREWARM["stt"] += 1
                    try:
                        await asyncio.get_running_loop().run_in_executor(None, STT.get)
                        text = await asyncio.get_running_loop().run_in_executor(GPU, _transcribe, msg[5:])
                    finally:
                        PREWARM["stt"] -= 1
                    STT.touch()
                    print(f"stt id={jid} ms={round((time.perf_counter() - t) * 1000)}", flush=True)
                    await ws.send(json.dumps({"op": "stt", "id": jid, "text": text, "ms": round((time.perf_counter() - t) * 1000)}))
                elif op == 3:
                    t = time.perf_counter()
                    try: prob = await asyncio.get_running_loop().run_in_executor(GPU, _turn_prob, msg[5:])
                    except Exception as e:
                        import traceback; traceback.print_exc(); prob = 1.0  # no model → behave like plain silence endpointing
                    await ws.send(json.dumps({"op": "turn", "id": jid, "p": prob, "ms": round((time.perf_counter() - t) * 1000)}))
                continue
            m = json.loads(msg)
            if m["op"] != "status": count()
            if m["op"] == "tts": tts_queue.put_nowait((m["id"], m["text"], m.get("voice"), m.get("instruct"), bool(m.get("first"))))
            elif m["op"] == "cancel": cancelled.add(m["id"])
            elif m["op"] == "prewarm": start_prewarm(m.get("voice"))
            elif m["op"] == "warm":
                for name in m.get("models") or ["stt", "tts"]:
                    {"stt": STT, "tts": TTS}.get(name, STT).warm()
            elif m["op"] == "status":
                await ws.send(json.dumps({"op": "status", "models": {"stt": STT.status(), "tts": TTS.status()}, "prewarm": {k: v for k, v in PREWARM.items() if k not in ("task", "stt")}, "rss_mb": rss_mb(), "clients": CLIENTS["n"]}))
    finally:
        worker.cancel()
        if counted: CLIENTS["n"] -= 1; CLIENTS["closed_at"] = time.monotonic()

async def main():
    # Only native local clients: no browser Origin (websites could otherwise load models / use
    # the speech stack), Host must be localhost (DNS rebinding).
    def local_only(conn, request):
        host = request.headers.get("Host", "")
        if host not in (f"127.0.0.1:{args.port}", f"localhost:{args.port}"):
            return conn.respond(403, "bad host\n")
    async with websockets.serve(handler, args.host, args.port, max_size=2**24, compression=None, origins=[None], process_request=local_only):
        await asyncio.Future()
asyncio.run(main())
