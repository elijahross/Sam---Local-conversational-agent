"""Prepares voice profiles for the speech sidecar (run once per new voice, or after changing a ref):

  1. reference loudness: ref.wav scaled to the same speech level for every voice;
  2. output gain: the clone's actual loudness on calibration sentences -> voice.json gain_db, so
     every voice plays at TARGET_DB (the sidecar applies it per chunk, plus a soft limiter);
  3. fillers: each filler is rendered TAKES times and the cleanest take is kept (closest to the
     speaker, plausible length, transcribes as the words) -> fillers/<slug>.wav + fillers.json.
     Fillers without a clean take are left out for that voice; the server only uses listed ones.

Usage: .venv/bin/python tools/prepare_voice.py [voice ...]   (default: every voice in voices/)
"""
import json, pathlib, re, sys
import numpy as np, soundfile as sf

ROOT = pathlib.Path(__file__).resolve().parents[1]
VOICES = ROOT / "voices"
MODEL = "mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit"
GEN = {"lang_code": "english", "temperature": 0.45}   # the speech sidecar's sampling
TARGET_DB = -20.0    # active-speech RMS every voice is brought to
TAKES = 6
CALIBRATION = ["Okay, so the next step is the timeline for Wednesday.", "That sounds like a lot to juggle at once.",
               "I'll keep an eye on it and remind you tomorrow morning.", "Honestly, I'd start with the one that matters most.",
               "Vienna, last time I checked.", "Let's work through it together, one thing at a time."]
# Must match the pools in server/src/session.rs (THINK / ACK / STALL fillers).
FILLERS = ["Hmm,", "Ooh, hm,", "Hm, okay,", "Let me think,", "Hmm, good one,", "Let me see,",
           "Mm,", "Mm, yeah,", "Oh, okay,", "Yeah,", "Hm,", "Right,", "Okay,",
           "Mm, hang on,", "Hm, one sec,", "And, um,", "One sec,"]
NONWORDS = {"hmm", "hm", "mm", "mmm", "um", "uh", "ooh", "oh"}


def active_rms_db(a, sr=24000):
    """RMS (dBFS) over 20 ms frames within 30 dB of the loudest frame: speech, not pauses."""
    f = int(0.02 * sr); n = len(a) // f
    if n == 0: return -90.0
    r = np.sqrt((a[: n * f].reshape(n, f) ** 2).mean(1) + 1e-12)
    db = 20 * np.log10(r)
    act = r[db > db.max() - 30]
    return float(20 * np.log10(np.sqrt((act ** 2).mean()) + 1e-12))


def trim(a, sr=24000, floor_db=-42):
    f = int(0.01 * sr); n = len(a) // f
    if n == 0: return a
    db = 20 * np.log10(np.sqrt((a[: n * f].reshape(n, f) ** 2).mean(1)) + 1e-9)
    on = np.where(db > max(floor_db, db.max() - 40))[0]
    if not on.size: return a
    a = a[max(0, (on[0] - 3) * f): min(len(a), (on[-1] + 4) * f)].copy()
    k = int(0.01 * sr); w = np.linspace(0, 1, k)
    a[:k] *= w; a[-k:] *= w[::-1]
    return a


def words(s):
    return re.sub(r"[^a-z' ]", " ", s.lower()).split()


def main(names):
    import mlx.core as mx
    from mlx_audio.tts.utils import load_model
    from mlx_audio.utils import load_audio
    from parakeet_mlx import from_pretrained
    model = load_model(MODEL)
    stt = from_pretrained("mlx-community/parakeet-tdt-0.6b-v3")
    # UTMOS matched the user's own review of wrong-voice takes (AUC 1.0 female / 0.8 male);
    # speaker similarity alone did not for the male voice.
    import torch
    from scipy.signal import resample_poly
    utmos = torch.hub.load("tarepan/SpeechMOS:v1.2.0", "utmos22_strong", trust_repo=True)
    def mos(a):
        with torch.no_grad(): return float(utmos(torch.from_numpy(resample_poly(a, 2, 3).astype(np.float32))[None], 16000))
    import tempfile; tmp = pathlib.Path(tempfile.mkdtemp()) / "_take.wav"

    def emb(a):
        e = np.array(model.extract_speaker_embedding(mx.array(a))).reshape(-1); return e / np.linalg.norm(e)

    def say(text, ref, ref_text):
        return np.clip(np.concatenate([np.array(r.audio, np.float32).reshape(-1) for r in model.generate(text=text, ref_audio=ref, ref_text=ref_text, **GEN)]), -1, 1)

    report = {}
    for v in names:
        d = VOICES / v
        # 1. reference at the common speech level (the clone follows the reference's level)
        ref_np, _ = sf.read(d / "ref.wav", dtype="float32")
        ref_np = ref_np if ref_np.ndim == 1 else ref_np.mean(1)
        ref_np = np.clip(ref_np * 10 ** ((TARGET_DB - active_rms_db(ref_np)) / 20), -0.98, 0.98)
        sf.write(d / "ref.wav", ref_np, 24000)
        ref = load_audio(str(d / "ref.wav"), sample_rate=24000); ref_text = (d / "ref.txt").read_text().strip()
        ref_e = emb(ref_np)
        # 2. output gain from calibration sentences
        cal = [say(t, ref, ref_text) for t in CALIBRATION]
        levels = [active_rms_db(a) for a in cal]
        gain_db = round(TARGET_DB - float(np.median(levels)), 2)
        sims = [float(emb(a) @ ref_e) for a in cal]
        floor = float(np.median(sims)) - 0.03   # a filler must sound as much like the speaker as a sentence
        # 3. fillers: best clean take
        fd = d / "fillers"; fd.mkdir(exist_ok=True)
        for old in fd.glob("*.wav"): old.unlink()
        kept, dropped = {}, {}
        for text in FILLERS:
            real = [w for w in words(text) if w not in NONWORDS]
            best = None
            for _ in range(TAKES):
                a = trim(say(text, ref, ref_text))
                dur = len(a) / 24000
                if not (0.15 <= dur <= 0.5 + 0.38 * len(words(text))): continue
                # Tiny clips are repeated to ~1.2 s so the speaker embedding is stable enough to check.
                sim = float(emb(np.tile(a, int(np.ceil(1.2 / dur))) if dur < 0.6 else a) @ ref_e)
                if sim < floor: continue
                sf.write(tmp, a, 24000)
                heard = words(stt.transcribe(str(tmp)).text)
                if real and not all(w in heard for w in real): continue
                if not real and len([w for w in heard if w not in NONWORDS]) > 0: continue
                score = sim + 0.1 * mos(np.tile(a, int(np.ceil(1.2 / dur))) if dur < 0.6 else a) - 0.05 * abs(dur - 0.25 - 0.3 * len(words(text)))
                if best is None or score > best[0]: best = (score, a, sim, dur)
            if best is None:
                dropped[text] = "no clean take"; continue
            slug = re.sub(r"[^a-z]+", "-", text.lower()).strip("-")
            sf.write(fd / f"{slug}.wav", np.clip(best[1] * 10 ** (gain_db / 20), -0.95, 0.95), 24000)  # stored at the target level
            kept[text] = {"file": f"fillers/{slug}.wav", "sim": round(best[2], 3), "dur_s": round(best[3], 2)}
        (d / "fillers.json").write_text(json.dumps(kept, indent=1))
        (d / "voice.json").write_text(json.dumps({"gain_db": gain_db, "target_db": TARGET_DB, "calibration_db": [round(x, 1) for x in levels],
                                                  "calibration_sim": [round(x, 3) for x in sims]}, indent=1))
        report[v] = {"gain_db": gain_db, "level_spread_db": round(float(np.std(levels)), 2), "fillers_kept": len(kept), "dropped": dropped}
        print(v, json.dumps(report[v]), flush=True)
    tmp.unlink(missing_ok=True)


if __name__ == "__main__":
    main(sys.argv[1:] or sorted(p.name for p in VOICES.iterdir() if (p / "ref.wav").exists()))
