"""Load models on demand, release them when idle (R1: no RAM while Sam isn't used).

- `get()` loads lazily (blocking only if nobody warmed it); `warm()` starts loading in the
  background and returns at once — the UI calls it on hover/hotkey so loading overlaps speech.
- A reaper thread unloads after `idle_s` without use, early under memory pressure
  (kern.memorystatus_vm_pressure_level: 1 normal, 2 warn, 4 critical) and never while `busy()`.
- Unload = drop references → gc → mx.clear_cache() (+ torch.mps.empty_cache()), run on the
  model's own executor thread (MLX streams are per thread).
"""
import gc, subprocess, threading, time

try:
    import mlx.core as mx
    mx.set_cache_limit(256 << 20)  # default ≈ unbounded buffer cache (mlx#3129); keep it small
except Exception:  # pragma: no cover
    mx = None


def pressure_level() -> int:
    try:
        return int(subprocess.run(["sysctl", "-n", "kern.memorystatus_vm_pressure_level"], capture_output=True, text=True, timeout=2).stdout.strip() or 1)
    except Exception:
        return 1


class ModelManager:
    def __init__(self, name, loader, idle_s=300, executor=None, release=None):
        self.name, self.loader, self.idle_s, self.ex, self.release = name, loader, idle_s, executor, release
        self.model, self.state, self.last = None, "unloaded", time.monotonic()
        self.lock, self.ready = threading.Lock(), threading.Event()
        self.load_ms, self.loads, self.error = None, 0, None

    def _run(self, fn):
        return self.ex.submit(fn).result() if self.ex else fn()

    def touch(self):
        self.last = time.monotonic()

    def warm(self):
        """Idempotent, non-blocking."""
        with self.lock:
            self.touch()
            if self.state in ("loading", "ready"):
                return
            self.state, self.error = "loading", None
            self.ready.clear()
        threading.Thread(target=self._load, name=f"load-{self.name}", daemon=True).start()

    def _load(self):
        t = time.perf_counter()
        try:
            self.model = self._run(self.loader)
            self.load_ms, self.loads = round((time.perf_counter() - t) * 1000), self.loads + 1
            self.state = "ready"
            print(f"[models] {self.name} loaded in {self.load_ms} ms", flush=True)
        except Exception as e:  # keep serving; get() raises
            self.state, self.error = "unloaded", repr(e)
            print(f"[models] {self.name} load failed: {e!r}", flush=True)
        self.touch()
        self.ready.set()

    def get(self):
        self.warm()
        self.ready.wait()
        if self.model is None:
            raise RuntimeError(f"{self.name} unavailable: {self.error}")
        self.touch()
        return self.model

    def unload(self, why="idle"):
        with self.lock:
            if self.state != "ready":
                return
            self.state = "unloading"
            self.ready.clear()

        def go():
            if self.release and self.model is not None:
                try:
                    self.release(self.model)
                except Exception:
                    pass
            self.model = None
            gc.collect()
            if mx is not None:
                mx.clear_cache()
            try:
                import torch
                if torch.backends.mps.is_available():
                    torch.mps.empty_cache()
            except Exception:
                pass

        self._run(go)
        self.state = "unloaded"
        print(f"[models] {self.name} unloaded ({why})", flush=True)

    def status(self):
        return {"state": self.state, "idle_s": round(time.monotonic() - self.last), "load_ms": self.load_ms, "loads": self.loads, "idle_timeout_s": self.idle_s, "error": self.error}


def start_reaper(managers, busy=lambda: False, every=5.0):
    def loop():
        while True:
            time.sleep(every)
            lvl = pressure_level()
            for m in managers:
                if m.state != "ready":
                    continue
                idle = time.monotonic() - m.last
                if lvl >= 4:
                    m.unload("memory pressure critical")
                elif busy():
                    continue
                elif lvl >= 2:
                    if idle < 90: continue   # in use right now (a conversation): a reload would cost 2-6 s mid-reply
                    m.unload("memory pressure warn")
                elif idle > m.idle_s:
                    m.unload(f"idle {int(idle)} s")

    threading.Thread(target=loop, name="model-reaper", daemon=True).start()


def rss_mb() -> int:
    import os, resource  # noqa: F401
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(__import__("os").getpid())], capture_output=True, text=True).stdout.strip()
    return int(out or 0) // 1024
