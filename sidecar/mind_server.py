"""Mind sidecar: memory embeddings + Laya micro-judgments, one process in the main MLX venv.

  POST /embed  {"texts": [...], "kind": "query" | "doc"}            -> {"model", "dim", "vectors": [[...]]}
  POST /judge  {"state": str|obj, "questions": {...}, "model"?: str} -> {"answers": {...}, "ms": float}
               (Laya typed questions: noul | choice | score; answers carry calibrated probabilities)
  POST /judge_batch {"items": [{"state", "questions"}...], "model"?}  -> {"results": [...answers...]}
  GET  /health                                                     -> {"model", "dim", "judge"} (never loads a model)
  POST /warm   {"models": ["embed","judge"]?}                      -> starts loading, returns at once
  GET  /status                                                     -> per-model state, rss_mb
Models load on demand and unload after --idle seconds (model_manager.py). While Laya is still
loading, /judge answers 503 at once (callers fail open within their 250 ms budget).

Embeddings: Qwen3-Embedding-0.6B (mlx-embeddings, last-token pooling, query instruction).
Images/video later: Qwen3-VL-Embedding-2B through the same mlx-embeddings package.
Judgments: Laya (ConvAI) on MPS; a fine-tuned checkpoint is registered as "sam" when present.
"""
import argparse, json, os, pathlib, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import mlx.core as mx
import numpy as np
from mlx_embeddings.utils import load as load_embedder
from model_manager import ModelManager, start_reaper, rss_mb

ap = argparse.ArgumentParser()
ap.add_argument("--host", default="127.0.0.1"); ap.add_argument("--port", type=int, default=47802)
ap.add_argument("--embed-model", default="mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ")
ap.add_argument("--dim", type=int, default=1024, help="MRL truncation (32..1024)")
ap.add_argument("--judge-checkpoint", default="elijahross/laya-sam",
                help="fine-tuned Laya checkpoint: a local dir or a Hugging Face repo (downloaded on first use)")
ap.add_argument("--no-judge", action="store_true")
ap.add_argument("--idle", type=int, default=900, help="unload a model after this many idle seconds")
args = ap.parse_args()

TASK = "Given a question from a user to their voice companion, retrieve memories about the user that help answer it"
MODEL_ID = f"{args.embed_model.split('/')[-1]}@{args.dim}"
GPU = threading.Lock()  # MLX: one stream; requests are ~2-15 ms

def _embed_with(m, texts):
    model, tk = m
    out = []
    for i in range(0, len(texts), 32):
        inp = tk(texts[i:i + 32], padding=True, truncation=True, max_length=2048, return_tensors="np")
        e = model(mx.array(inp["input_ids"]), attention_mask=mx.array(inp["attention_mask"])).text_embeds[:, : args.dim]
        e = e / mx.linalg.norm(e, axis=-1, keepdims=True)
        out.append(np.array(e.astype(mx.float32)))
    return np.concatenate(out) if out else np.zeros((0, args.dim), np.float32)

def _load_embed():
    with GPU:
        model, tok = load_embedder(args.embed_model)
        tk = getattr(tok, "_tokenizer", tok); tk.padding_side = "right"  # last-token pooling indexes by mask length
        _embed_with((model, tk), ["warmup"])
        return (model, tk)

def embed(texts, kind):
    m = EMBED.get()
    if kind == "query":
        texts = [f"Instruct: {TASK}\nQuery:{t}" for t in texts]
    with GPU:
        r = _embed_with(m, texts)
    EMBED.touch()
    return r

JUDGE_DEFAULT = None
if not args.no_judge:
    ckpt = pathlib.Path(args.judge_checkpoint)
    _local = ckpt.is_dir() and ((ckpt / "config.json").exists() or any(ckpt.glob("*.safetensors")))
    _repo = not ckpt.exists() and args.judge_checkpoint.count("/") == 1
    JUDGE_DEFAULT = "sam" if _local or _repo else "typed-decisions"

def _judge_dir():
    """The checkpoint dir: the local one, or the Hub repo's cached snapshot (fetched once)."""
    if not _repo:
        return str(ckpt)
    from huggingface_hub import snapshot_download
    try:
        return snapshot_download(args.judge_checkpoint, local_files_only=True)
    except Exception:
        return snapshot_download(args.judge_checkpoint)

def _load_judge():
    from laya import Router
    router = Router()
    if JUDGE_DEFAULT == "sam":
        router.register("sam", _judge_dir(), description="Laya fine-tuned on Sam's micro-judgments")
    router.predict("warm up", {"x": {"type": "noul", "instructions": "Is this a test?"}}, model=JUDGE_DEFAULT)
    return router

def _release_judge(router):
    router.unload()  # laya: no name = every loaded model (calls torch.mps.empty_cache)

EMBED = ModelManager("embed", _load_embed, args.idle)
JUDGE = ModelManager("judge", _load_judge, args.idle, release=_release_judge) if not args.no_judge else None
start_reaper([m for m in (EMBED, JUDGE) if m])
JLOCK = threading.Lock()  # torch MPS model: serialize

class NotReady(Exception):
    pass

def judge(state, questions, model_name=None):
    if JUDGE.state != "ready":  # don't make a 250 ms caller wait for a load; it fails open
        JUDGE.warm()
        raise NotReady("judge loading")
    with JLOCK:
        a = JUDGE.model.predict(state, questions, model=model_name or JUDGE_DEFAULT)["answers"]
    JUDGE.touch()
    return a

class H(BaseHTTPRequestHandler):
    def _send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b)))
        self.end_headers(); self.wfile.write(b)

    def _local(self):
        """Native local clients only: no browser Origin, localhost Host (DNS rebinding)."""
        ok = self.headers.get("Origin") is None and self.headers.get("Host", "") in (f"127.0.0.1:{args.port}", f"localhost:{args.port}")
        if not ok: self._send(403, {"error": "forbidden"})
        return ok

    def do_GET(self):
        if not self._local(): return
        if self.path == "/health":
            return self._send(200, {"model": MODEL_ID, "dim": args.dim, "judge": JUDGE_DEFAULT})
        if self.path == "/status":
            return self._send(200, {"models": {"embed": EMBED.status(), **({"judge": JUDGE.status()} if JUDGE else {})}, "rss_mb": rss_mb()})
        self._send(404, {})

    def do_POST(self):
        if not self._local(): return
        try:
            req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            t = time.perf_counter()
            if self.path == "/warm":
                for name in req.get("models") or ["embed", "judge"]:
                    m = {"embed": EMBED, "judge": JUDGE}.get(name)
                    if m: m.warm()
                return self._send(200, {"ok": True})
            if self.path == "/embed":
                v = embed(req["texts"], req.get("kind", "doc"))
                return self._send(200, {"model": MODEL_ID, "dim": args.dim, "ms": round((time.perf_counter() - t) * 1000, 1), "vectors": v.round(6).tolist()})
            if self.path == "/judge" and JUDGE:
                a = judge(req["state"], req["questions"], req.get("model"))
                return self._send(200, {"answers": a, "ms": round((time.perf_counter() - t) * 1000, 1)})
            if self.path == "/judge_batch" and JUDGE:
                res = [judge(i["state"], i["questions"], req.get("model")) for i in req["items"]]
                return self._send(200, {"results": res, "ms": round((time.perf_counter() - t) * 1000, 1)})
            self._send(404, {})
        except NotReady as e:
            self._send(503, {"error": str(e), "retry": True})
        except Exception as e:  # callers fail open (no memory / default decision), never crash the server
            self._send(400, {"error": f"{type(e).__name__}: {e}"})

    def log_message(self, *a):
        pass

print(f"mind ready: embed={MODEL_ID} judge={JUDGE_DEFAULT} on {args.host}:{args.port} (models load on demand, idle unload {args.idle} s)", flush=True)
ThreadingHTTPServer((args.host, args.port), H).serve_forever()
