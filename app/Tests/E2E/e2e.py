"""Sam island end-to-end tests (app/docs/user-stories.md). Real stack: Sam.app (test mode,
speech injected as WAV instead of the mic) ↔ Rust server ↔ speech/mind sidecars (on-demand
models, --idle 30) ↔ Gemini. Screenshots of each island state go to shots/.
usage: e2e.py [test ...]"""
import asyncio, base64, json, os, pathlib, re, subprocess, sys, time, urllib.request, urllib.parse
import websockets
H = pathlib.Path(__file__).resolve().parent; ROOT = H.parents[2]
PY = str(ROOT / ".venv/bin/python")  # created by run.sh; SHOTS = H / "shots"; SHOTS.mkdir(exist_ok=True)
# Sam.app may not read ~/Documents (TCC "Documents folder" prompt blocks open()): copy the test
# speech to a non-protected location first.
AUD = pathlib.Path("/tmp/sam-e2e"); AUD.mkdir(exist_ok=True)
import shutil
for _w in (H / "audio").glob("*.wav"): shutil.copy(_w, AUD / _w.name)
LOG = H / "e2e-results.jsonl"
# The stack under test gets its own fresh data dir: tests never read or write the user's memory,
# ledger or pending actions.
DATA = AUD / "data"; shutil.rmtree(DATA, ignore_errors=True); DATA.mkdir(parents=True)

TOKEN = (pathlib.Path.home() / "Library/Application Support/Sam/local-token").read_text().strip() if (pathlib.Path.home() / "Library/Application Support/Sam/local-token").exists() else ""

def http(url, body=None, timeout=30):
    hdr = {"X-Sam-Token": TOKEN}
    if body is not None: hdr["Content-Type"] = "application/json"
    r = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), method="GET" if body is None else "POST", headers=hdr)
    return json.loads(urllib.request.urlopen(r, timeout=timeout).read())
app = lambda path, body=None: http("http://127.0.0.1:47804" + path, body)
state = lambda: app("/state")
trigger = lambda a: app("/trigger", {"action": a})
inject = lambda wav: app("/inject", {"wav": str(AUD / wav)})

def wait(pred, timeout=30, every=0.1):
    t = time.time()
    while time.time() - t < timeout:
        s = state()
        if pred(s): return s, time.time() - t
        time.sleep(every)
    return state(), None

def fresh_session(timeout=8, settle=0.0):
    """A new, connected hands-free session, whatever an earlier test left behind (a result still
    being spoken, a toast, a closed island): toggle on an open island would close it. `settle`
    gives the on-demand judge model time to load before the first turn (a person takes that long
    to start speaking; a typed turn arrives at once)."""
    if state()["state"] != "hidden": trigger("close"); wait(lambda s: s["state"] == "hidden", 3)
    trigger("toggle")
    r = wait(lambda s: s["state"] == "listening" and s["connected"], timeout)
    if settle: time.sleep(settle)
    return r

def vdisplay(secs):
    """Sam's virtual display for the tests' windows; a clear error when it can't be created
    (CGVirtualDisplay has a system-wide cap: leftover helpers hold displays)."""
    exe = H / "vdisplay/vdisplay"
    if not exe.exists():
        subprocess.run(["clang", "-fobjc-arc", "-framework", "Foundation", "-framework", "AppKit", "-framework", "CoreGraphics",
                        "-o", str(exe), str(H / "vdisplay/vdisplay.m")], check=True)
    vd = subprocess.Popen([str(exe), str(secs)], stdout=subprocess.PIPE, text=True)
    line = vd.stdout.readline()
    try: disp = json.loads(line)
    except Exception: disp = {}
    if "x" not in disp:
        vd.terminate()
        raise RuntimeError(f"virtual display unavailable: {line.strip()[:160]!r}")
    return vd, disp

def events(s, since=0):
    return [(e["e"], e["t"]) for e in s["log"] if e["t"] >= since]

def ev_time(s, name, since=0):
    for e, t in events(s, since):
        if e == name: return t
    return None

def shot(name, largest=False, width=900, title=None):
    try:
        r = http("http://127.0.0.1:47803/screenshot", {"app": "one.sam.app", "max_width": width, "any_layer": True, "largest": largest, **({"title": title} if title else {})})
        if "jpeg_b64" in r: (SHOTS / f"{name}.jpg").write_bytes(base64.b64decode(r["jpeg_b64"]))
    except Exception: pass

def footprint(pat):
    pid = subprocess.run(["pgrep", "-f", pat], capture_output=True, text=True).stdout.split()
    if not pid: return None
    for l in subprocess.run(["footprint", "-p", pid[0]], capture_output=True, text=True).stdout.splitlines():
        if "phys_footprint:" in l: return l.split("phys_footprint:")[1].strip()

async def speech_status():
    async with websockets.connect("ws://127.0.0.1:47801") as ws:
        await ws.recv(); await ws.send(json.dumps({"op": "status"})); return json.loads(await ws.recv())

PROCS = []
# The user's own Sam.app (not a test instance, e.g. /Applications/Sam.app) is relaunched from
# its own path after the run if it was running.
USER_APP = next((l.strip().removesuffix("/Contents/MacOS/Sam") for l in subprocess.run(["ps", "-axo", "comm"], capture_output=True, text=True).stdout.splitlines()
                 if l.strip().endswith("/Contents/MacOS/Sam")), None)

REPORT_TASK = "t_e2e_report"

def seed_report():
    """A finished research task with a presented report (made by `cargo test present_live` into
    data/artifacts/reports/test-report): copied into the test data before the server starts."""
    src = ROOT / "data/artifacts/reports/test-report"
    if not (src / "report.json").exists(): return False
    dst = DATA / "artifacts/reports" / REPORT_TASK; dst.mkdir(parents=True, exist_ok=True)
    rep = json.loads((src / "report.json").read_text())
    for f in src.glob("img_*"): shutil.copy(f, dst / f.name)
    def fix(v):
        if isinstance(v, dict): return {k: fix(x) for k, x in v.items()}
        if isinstance(v, list): return [fix(x) for x in v]
        if isinstance(v, str) and "/reports/test-report/" in v: return str(dst / v.rsplit("/", 1)[1])
        return v
    rep = fix(rep)
    (DATA / "tasks").mkdir(parents=True, exist_ok=True)
    now = int(time.time() * 1000)
    (DATA / "tasks" / f"{REPORT_TASK}.json").write_text(json.dumps({"id": REPORT_TASK, "session": "e2e", "kind": "research",
        "request": rep.get("request", "Trip research"), "context": "", "status": "done", "current": "done", "plan": ["research"],
        "steps": [{"tool": "research", "status": "done", "summary": "research: 2 relevant pages.", "file": None, "dir": None, "at": now}],
        "actions": [], "files": [], "notes": [], "result": "research: 2 relevant pages.", "tokens": 0, "report": rep, "created": now, "updated": now}))
    return True

def port_pids(port):
    return subprocess.run(["lsof", "-ti", f"tcp:{port}"], capture_output=True, text=True).stdout.split()

def abort(why):
    """Never test against the user's own stack and data: stop, restore the user's app, exit."""
    print("ABORT:", why, flush=True); stop_stack(); sys.exit(2)

def stop_user_stack():
    """The user's standalone Sam.app runs sam-server, the sidecars and the helper as its own
    processes. Quit it (that stops its children) and wait for the ports: the test stack must own
    47800-47802 and 47804, or the tests would run against the user's server and data (2026-10-10)."""
    if USER_APP: subprocess.run(["osascript", "-e", 'tell application "Sam" to quit'], capture_output=True, timeout=30)
    subprocess.run(["pkill", "-x", "Sam"], capture_output=True)
    subprocess.run(["pkill", "-f", "speech_server.py|mind_server.py|target/release/voice-agent|/bin/sam-server|/bin/sam-workspace"], capture_output=True)
    for _ in range(60):
        busy = {p: port_pids(p) for p in (47800, 47801, 47802, 47804) if port_pids(p)}
        if not busy: return
        time.sleep(0.5)
    print("ABORT: ports still owned by another Sam stack:", busy, flush=True)
    if USER_APP: subprocess.run(["open", USER_APP])
    sys.exit(2)

HELPER_STARTED = False

def start_stack():
    seed_report()
    stop_user_stack()
    PROCS.append(subprocess.Popen([PY, str(ROOT / "sidecar/speech_server.py"), "--idle", "30"] + ([] if os.environ.get("E2E_STT") == "parakeet" else ["--stt", "apple", "--stt-locale", "en_US"]), stdout=open("/tmp/e2e_speech.log", "w"), stderr=subprocess.STDOUT))
    PROCS.append(subprocess.Popen([PY, str(ROOT / "sidecar/mind_server.py"), "--idle", "30"], stdout=open("/tmp/e2e_mind.log", "w"), stderr=subprocess.STDOUT))
    for _ in range(80):
        try: http("http://127.0.0.1:47802/health"); break
        except Exception: time.sleep(0.25)
    time.sleep(1.5)
    PROCS.append(subprocess.Popen([str(ROOT / "server/target/release/voice-agent")], cwd=str(ROOT / "server"), stdout=open("/tmp/e2e_server.log", "w"), stderr=subprocess.STDOUT,
                                  env={**os.environ, "RUST_LOG": "info,html5ever=error", "RUST_BACKTRACE": "1", "DATA_DIR": str(DATA), "BG_SCREEN": "7", "SAM_NO_REAL_APPS": "1", "TURN_TIMEOUT_MS": "1500",
                                       "CHAT_URL_WHATSAPP": "file://" + str(H / "fixtures/messaging/whatsapp.html")}))
    for _ in range(80):
        try: urllib.request.urlopen("http://127.0.0.1:47800/", timeout=1); break
        except Exception: time.sleep(0.25)
    # The server on 47800 must be ours, with the fresh test data (only the seeded report).
    if str(PROCS[-1].pid) not in port_pids(47800): abort(f"47800 is not the test server (pids {port_pids(47800)}); see /tmp/e2e_server.log")
    try: ids = {t["id"] for t in http("http://127.0.0.1:47800/api/tasks")["tasks"]}
    except Exception as e: abort(f"test server not answering: {e!r}")
    if not ids <= {REPORT_TASK}: abort(f"server on 47800 serves other tasks (not the test data dir): {sorted(ids)[:5]}")
    # The binary directly, not `open -n`: LaunchServices doesn't bring a GUI app up while the
    # screen is locked (a run after midnight sat on a dead test port for 30 minutes), and this
    # way its output lands in /tmp/e2e_app.log.
    PROCS.append(subprocess.Popen([str(ROOT / "app/Sam/build/Sam.app/Contents/MacOS/Sam"), "--test"], stdout=open("/tmp/e2e_app.log", "w"), stderr=subprocess.STDOUT))
    for _ in range(60):
        try: state(); break
        except Exception: time.sleep(0.25)
    else:
        abort("the island app (test mode) didn't come up on 47804; see /tmp/e2e_app.log")
    # The computer-use helper (point-and-ask, screenshots): the user's keeps running if it
    # survived, else the repo build (its Accessibility grant must be given once in System Settings).
    global HELPER_STARTED
    if not port_pids(47803):
        subprocess.run(["open", "-n", str(ROOT / "sidecar/computer/build/Sam Computer Use.app")]); HELPER_STARTED = True
        for _ in range(40):
            if port_pids(47803): break
            time.sleep(0.25)

def stop_stack():
    subprocess.run(["pkill", "-x", "Sam"], capture_output=True)
    for p in PROCS: p.terminate()
    if HELPER_STARTED: subprocess.run(["pkill", "-x", "sam-computer"], capture_output=True)
    if USER_APP:
        subprocess.run(["open", USER_APP])

RESULTS = []
def record(name, ok, **m):
    RESULTS.append(dict(test=name, ok=bool(ok), **m)); print(("PASS " if ok else "FAIL ") + name, json.dumps(m)[:300], flush=True)

def ui(): return app("/ui_state", {})
def wait_ui(cond, timeout, step=1.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            u = ui()
            if cond(u): return u, time.time() - t0
        except Exception: pass
        time.sleep(step)
    return None, None

def wait_task(text, cond, timeout):
    """The newest task whose request is `text` once cond(task) holds (full record)."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            lst = http("http://127.0.0.1:47800/api/tasks")
            lst = lst.get("tasks", lst) if isinstance(lst, dict) else lst
            hit = next((t for t in lst if t.get("request") == text), None)
            if hit:
                full = http(f"http://127.0.0.1:47800/api/tasks/{hit['id']}")
                if cond(full): return full, time.time() - t0
        except Exception: pass
        time.sleep(1)
    return None, None

def converse(wav, ptt=False, timeout=45):
    """One turn; returns (state, timings in ms relative to inject_end)."""
    t0 = (state()["log"][-1]["t"] + 1) if state()["log"] else 0   # strictly after the previous turn's events
    if ptt: trigger("ptt_down"); time.sleep(0.2)
    inject(wav)
    s, _ = wait(lambda s: ev_time(s, "inject_end", t0) is not None, 20)
    if ptt: trigger("ptt_up")
    # The injection ends with 1.5 s of room silence; latency counts from the end of speech
    # (push-to-talk: from the key release, which happens at inject_end).
    end = ev_time(s, "inject_end", t0) - (0 if ptt else 1500)
    s, _ = wait(lambda s: ev_time(s, "playback_done", t0) is not None or ev_time(s, "agent_done", t0) and not s["playing"] and s["state"] in ("listening", "result"), timeout)
    tr = {k: (ev_time(s, k, t0) - end if ev_time(s, k, t0) else None) for k in ("transcript", "first_audio", "agent_done", "playback_done")}
    return s, tr

def t_hover():
    subprocess.run(["open", "-a", "Finder"]); time.sleep(1)
    front0 = state()["frontmost"]
    trigger("hover_enter")
    s, dt = wait(lambda s: s["state"] == "peek", 2)
    time.sleep(0.4); shot("01_peek")
    s2 = state()
    record("hover.hot_zone", dt is not None and dt <= 0.6, open_s=dt and round(dt, 2))
    record("focus.preserved", s2["frontmost"] == front0 and not s2["panel_key"], front_before=front0, front_after=s2["frontmost"], panel_key=s2["panel_key"])
    trigger("hover_exit")
    s, dt = wait(lambda s: s["state"] == "hidden", 2)
    record("hover.exit_closes", dt is not None)

def t_handsfree():
    trigger("toggle")
    s, dt = wait(lambda s: s["state"] == "listening" and s["connected"], 5)
    shot("02_listening")
    record("hotkey.toggle_listening", dt is not None, connected_s=dt and round(dt, 2))
    s, tr = converse("q_capital.wav")
    ok = "austria" in s["transcript"].lower() and "vienna" in s["answer"].lower()
    record("listen.speak.cold", ok and tr["first_audio"] is not None, transcript=s["transcript"], answer=s["answer"][:80], ms=tr)
    # The reply's voice was prewarmed while the question was being spoken.
    pw = asyncio.run(speech_status()).get("prewarm") or {}
    record("speak.prewarmed", pw.get("runs", 0) + pw.get("skipped", 0) >= 1, **pw)
    time.sleep(1)
    s, tr = converse("q_capital.wav")
    record("speak.latency.warm", tr["first_audio"] is not None and tr["first_audio"] <= 2000, ms=tr, state_after=s["state"])
    record("handsfree.returns_to_listening", s["state"] == "listening", state=s["state"])

def t_handsfree_close():
    """Hands-free never leaves the mic on forever: it closes when nobody starts talking, and
    after a reply once no follow-up comes (short timeouts set through the test API)."""
    try:
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        app("/ui", {"action": "handsfree_idle", "first": 2, "follow": 2})
        trigger("toggle")
        wait(lambda s: s["state"] == "listening", 5)
        s, dt = wait(lambda s: s["state"] == "hidden", 8, 0.2)
        record("handsfree.closes_when_silent", dt is not None, s=dt and round(dt, 1), **({} if dt is not None else {"state": s["state"], "log": [f"{e['t']}:{e['e']}" for e in s["log"][-24:]]}))
        trigger("toggle")
        wait(lambda s: s["state"] == "listening" and s["connected"], 5)
        s, tr = converse("q_capital.wav")
        t0 = time.time()
        s, _ = wait(lambda s: s["state"] in ("hidden",) or (s["state"] == "listening" and not s["playing"]), 40, 0.2)
        s, dt = wait(lambda s: s["state"] == "hidden", 10, 0.2)
        record("handsfree.closes_after_reply", dt is not None and any(e["e"] == "handsfree_idle_close" for e in s["log"]), s=dt and round(dt, 1))
    finally:
        app("/ui", {"action": "handsfree_idle", "first": 60, "follow": 60})

def t_bargein():
    t0 = (state()["log"][-1]["t"] + 1) if state()["log"] else 0
    inject("q_long.wav")
    s, _ = wait(lambda s: ev_time(s, "first_audio", t0) is not None, 30)
    # Fillers play instantly from cache: interrupt the real reply, not the gap after a filler.
    wait(lambda s: s["playing"] and len(s["answer"].split()) >= 8, 20, 0.1)
    shot("03_speaking")
    playing_before = state()["playing"]
    t1 = state()["log"][-1]["t"] + 1
    inject("q_stop.wav")
    s, dt = wait(lambda s: ev_time(s, "user_speaking", t1) is not None, 6, 0.05)
    s2, dt2 = wait(lambda s: not s["playing"], 3, 0.05)
    record("speak.barge_in", playing_before and dt is not None and dt2 is not None, user_speaking_after_s=dt and round(dt, 2), stopped_after_s=dt2 and round(dt2, 2))
    # The "stop" utterance is a turn of its own and Sam answers it: wait for that reply to finish,
    # or the next scenario finds the island mid-reply (2026-10-13: the listening moment during the
    # barge-in itself satisfied this wait and the whole run after it cascaded).
    wait(lambda s: (ev_time(s, "playback_done", t1) is not None or (ev_time(s, "agent_done", t1) is not None and not s["playing"])) and s["state"] == "listening", 40)

def t_ptt():
    trigger("close"); time.sleep(0.5)
    trigger("ptt_down")
    s, dt = wait(lambda s: s["state"] == "listening", 2)
    record("ptt.opens_listening", dt is not None and dt <= 0.3, s=dt and round(dt, 2))
    trigger("ptt_up")  # tap with no speech: no turn
    time.sleep(0.8)
    s, tr = converse("q_capital.wav", ptt=True)
    record("ptt.turn", "austria" in s["transcript"].lower() and tr["first_audio"] is not None, transcript=s["transcript"], ms=tr, state_after=s["state"])
    record("ptt.ends_in_result", s["state"] == "result", state=s["state"])
    shot("04_result")

def t_fn():
    """fn acts only while the island is open; fn+another key (volume…) is never a turn."""
    trigger("close"); time.sleep(0.5)
    trigger("fn_down"); time.sleep(0.4); s = state(); trigger("fn_up"); time.sleep(0.3)
    record("fn.ignored_when_hidden", s["state"] == "hidden" and state()["state"] == "hidden", state=s["state"])
    for _ in range(2): trigger("fn_down"); time.sleep(0.05); trigger("fn_up"); time.sleep(0.1)
    time.sleep(0.4)
    record("fn.double_tap_ignored_when_hidden", state()["state"] == "hidden", state=state()["state"])
    trigger("hover_enter"); wait(lambda s: s["state"] == "peek", 2)
    trigger("fn_down"); s, dt = wait(lambda s: s["state"] == "listening" and s["ptt_held"], 2)
    record("fn.hold_in_peek_listens", dt is not None, s=dt and round(dt, 2))
    t0 = state()["log"][-1]["t"]
    trigger("fn_combo_key"); trigger("fn_up"); time.sleep(0.6)
    s = state()
    record("fn.combo_cancels", ev_time(s, "ptt_cancel", t0) is not None and s["state"] == "hidden", state=s["state"])
    trigger("hover_exit"); trigger("close")

def t_cards():
    fresh_session()
    t0 = state()["log"][-1]["t"] + 1
    inject("q_weather.wav")
    s, dt = wait(lambda s: ev_time(s, "card_result", t0) is not None or ev_time(s, "card_error", t0) is not None, 90, 0.5)
    shot("05_card_result")
    record("card.background_result", ev_time(s, "card_result", t0) is not None, s=dt and round(dt, 1), cards=s["cards"], transcript=s["transcript"])
    # Opening a result shows its task in the overview — never a raw file in another app.
    opener = next((c["open"] for c in s["cards"] if c.get("kind") == "result" and c.get("open")), "")
    # (the click itself opens the overview where the pointer is — not done here, the user's
    # screen stays untouched; main.swift routes "task:<id>" to overview.show)
    record("card.open_shows_task", opener.startswith("task:t"), open=opener)
    # The result is also read out (not only shown): agent speech after the card arrived.
    tc = ev_time(s, "card_result", t0) or 0
    s, dt2 = wait(lambda s: ev_time(s, "first_audio", tc) is not None, 30, 0.5)
    record("card.result_spoken", dt2 is not None, s=dt2 and round(dt2, 1), answer=s["answer"][:120])

def t_confirm():
    fresh_session()
    t0 = state()["log"][-1]["t"] + 1
    inject("q_calendar.wav")
    s, dt = wait(lambda s: any(c["kind"] == "confirm" for c in s["cards"]), 90, 0.5)
    time.sleep(0.6); shot("06_card_confirm")
    conf = [c for c in s["cards"] if c["kind"] == "confirm"]
    record("card.confirm_shown", bool(conf), s=dt and round(dt, 1), cards=s["cards"], transcript=s["transcript"])
    if conf:
        aid = conf[0]["action_id"]
        import datetime
        pa = json.loads((DATA / f"pending/{aid}.json").read_text())["args"]
        def iso(t):
            try: datetime.datetime.fromisoformat(t); return True
            except Exception: return False
        record("card.confirm_exact_times", all(iso(pa.get(k, "")) for k in ("start", "end")), start=pa.get("start"), end=pa.get("end"), title=pa.get("title"))
        app("/confirm", {"action_id": aid, "decision": "cancel"})
        s2, dt2 = wait(lambda s: not any(c["kind"] == "confirm" for c in s["cards"]), 5)
        time.sleep(4)
        pend = list((DATA / "pending").glob(f"{aid}.json"))
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("card.confirm_cancel", dt2 is not None and not pend and aid not in ledger, action_id=aid, pending_left=len(pend), executed=aid in ledger)

def t_close():
    trigger("close")
    s, dt = wait(lambda s: s["state"] == "hidden" and not s["engine_running"], 2)
    record("close.stops_audio", dt is not None)
    s, dt = wait(lambda s: not s["connected"], 12)
    record("close.disconnects_session", dt is not None, s=dt and round(dt, 1))

def t_models():
    t = time.time()
    st = None
    while time.time() - t < 150:
        st = asyncio.run(speech_status()); m = http("http://127.0.0.1:47802/status")
        if all(v["state"] == "unloaded" for v in st["models"].values()) and all(v["state"] == "unloaded" for v in m["models"].values()): break
        time.sleep(5)
    m = http("http://127.0.0.1:47802/status")
    ok = all(v["state"] == "unloaded" for v in st["models"].values()) and all(v["state"] == "unloaded" for v in m["models"].values())
    try:
        import asyncio as _a
        async def _st():
            async with websockets.connect("ws://127.0.0.1:47801") as w:
                await w.recv(); await w.send(json.dumps({"op": "status"})); return json.loads(await w.recv())
        clients = _a.run(_st()).get("clients")
    except Exception: clients = None
    record("models.idle_release", ok, clients=clients, after_s=round(time.time() - t), speech=footprint("speech_server.py"), mind=footprint("mind_server.py"),
           loads={**{k: v["load_ms"] for k, v in st["models"].items()}, **{k: v["load_ms"] for k, v in m["models"].items()}})

def t_point():
    """Point-and-ask: the invitation window lives on Sam's virtual display (never covered by the
    user's windows, never reading user content); expect two approvals + a conflict warning."""
    vd, disp = vdisplay("150")
    try:
        # The fixture lives in the repo (/tmp is wiped on reboot); outside ~/Documents for TCC.
        os.makedirs("/tmp/sam-e2e", exist_ok=True)
        subprocess.run(["textutil", "-convert", "rtf", str(H / "fixtures/invite2.txt"), "-output", "/tmp/sam-e2e/invite2.rtf"], check=True)
        subprocess.run(["open", "-a", "TextEdit", "/tmp/sam-e2e/invite2.rtf"]); time.sleep(2.5)
        cu = lambda p, b: http("http://127.0.0.1:47803" + p, b)
        cu("/move_window", {"app": "com.apple.TextEdit", "title": "invite2", "x": disp["x"] + 200, "y": disp["y"] + 150})
        time.sleep(0.6)
        fr = cu("/window_frame", {"app": "com.apple.TextEdit", "title": "invite2"})
        on_vd = all(k in fr for k in "xywh") and disp["x"] <= fr["x"] < disp["x"] + disp.get("w", 1920) and disp["y"] <= fr["y"] < disp["y"] + disp.get("h", 1200)
        pt = [fr["x"] + fr["w"] / 2, fr["y"] + fr["h"] / 2] if on_vd else None
        hit = cu("/window_at", {"x": pt[0], "y": pt[1], "max_chars": 200}) if pt else {}
        target = hit.get("app") == "TextEdit" and "invite2" in (hit.get("window") or "")
        record("point.hits_target_window", target, app=hit.get("app"), window=hit.get("window"), frame=fr, on_virtual_display=on_vd)
        if not target:
            # Never point at (and so never read) one of the user's own windows.
            return
        front0 = cu("/front", None) if False else http("http://127.0.0.1:47803/front")
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        app("/trigger", {"action": "toggle", "point": pt})
        s, _ = wait(lambda s: s["state"] == "listening" and s["connected"], 5)
        record("point.context_captured", "invite2" in s["context_chip"] or "TextEdit" in s["context_chip"], chip=s["context_chip"])
        t0 = state()["log"][-1]["t"] + 1
        inject("q_point.wav")
        s, dt = wait(lambda s: sum(c["kind"] == "confirm" for c in s["cards"]) >= 2, 150, 0.5)
        time.sleep(0.8); shot("07_point_two_approvals")
        full = s.get("cards_full", [])
        confs = [c for c in full if c["kind"] == "confirm"]
        tools = sorted(next((b["header"]["detail"] for b in c["blocks"] if "header" in b), "") for c in confs)
        email = next((c for c in confs if any(b.get("header", {}).get("detail") == "email_send" for b in c["blocks"])), None)
        cal = next((c for c in confs if any(b.get("header", {}).get("detail") == "calendar_create" for b in c["blocks"])), None)
        f = lambda c, k: next((x["value"] for x in (c or {}).get("confirm", {}).get("fields", []) if x["key"] == k), "")
        record("point.two_approvals", tools == ["calendar_create", "email_send"], s=dt and round(dt, 1), tools=tools, transcript=s["transcript"])
        record("point.reply_to_sender", "anna.berger@example.com" in f(email, "to"), to=f(email, "to"), subject=f(email, "subject"), body=f(email, "body")[:140])
        record("point.event_extracted", f(cal, "start").startswith("2026-10-14T21:00") and f(cal, "end").startswith("2026-10-14T22:00"), title=f(cal, "title"), start=f(cal, "start"), end=f(cal, "end"))
        warn = [b["warning"] for b in (cal or {}).get("blocks", []) if "warning" in b]
        record("point.conflict_warning", bool(warn) and "Jayden" in warn[0], warning=warn)
        for c in confs:
            app("/confirm", {"action_id": c["confirm"]["action_id"], "decision": "cancel"})
        time.sleep(5)
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("point.nothing_executed", not any(c["confirm"]["action_id"] in ledger for c in confs) and not list((DATA / "pending").glob("*.json")))
        front1 = http("http://127.0.0.1:47803/front")
        # The user keeps working meanwhile, so "unchanged" isn't testable; Sam must never take focus.
        # panel_key is only informative: the panel becomes key solely after a click into it, and
        # Approve has no keyboard shortcut, so typing elsewhere can never approve anything.
        record("point.sam_never_frontmost", front1["app"] != "Sam", before=front0["app"], after=front1["app"], panel_key=state()["panel_key"])
    finally:
        try: trigger("close")   # end the conversation, so the models may unload afterwards
        except Exception: pass
        subprocess.run(["osascript", "-e", 'tell application "TextEdit" to close (every document whose name contains "invite2") saving no'], capture_output=True)
        vd.terminate()

def t_voice():
    """Character menu: each choice is spoken in its own cloned voice profile, with that persona."""
    for v in ["sam-female", "sam-male"]:
        trigger("close"); time.sleep(0.5)
        app("/voice", {"voice": v})
        # Jobs are logged when they finish: count by id, not by position, or the previous
        # scenario's last (whole-take) piece lands in this turn's tally.
        ids0 = [int(l.split("tts id=")[1].split()[0]) for l in open("/tmp/e2e_speech.log").read().split("\n") if "tts id=" in l]
        max0 = max(ids0, default=0)
        s, tr = converse("q_capital.wav", ptt=True)
        jobs = [l for l in open("/tmp/e2e_speech.log").read().split("\n") if "tts id=" in l and int(l.split("tts id=")[1].split()[0]) > max0]
        voices = sorted({l.split("voice=")[1].split()[0] for l in jobs})
        record(f"voice.{v}", voices == [v] and tr["first_audio"] is not None, voices=voices, state_voice=state()["voice"], answer=s["answer"][:90], ms=tr)

def t_point_chat():
    """Point at a WhatsApp Web window (test double in Safari on Sam's virtual display) and ask for
    a reply: Sam reads the chat in its own browser, drafts the message (verified recipient)
    and suggests the agreed meeting as a second card. Both are cancelled; nothing is sent."""
    vd, disp = vdisplay("150")
    fixture = str(AUD / "whatsapp.html")   # outside ~/Documents (Safari/TCC)
    shutil.copy(H / "fixtures/messaging/whatsapp.html", fixture)
    try:
        subprocess.run(["open", "-g", "-a", "Safari", fixture]); time.sleep(3)
        cu = lambda p, b: http("http://127.0.0.1:47803" + p, b)
        end = time.time() + 10   # Safari may still be launching / loading the page
        while True:
            cu("/move_window", {"app": "com.apple.Safari", "title": "WhatsApp", "x": disp["x"] + 150, "y": disp["y"] + 100})
            time.sleep(0.6)
            fr = cu("/window_frame", {"app": "com.apple.Safari", "title": "WhatsApp"})
            on_vd = all(k in fr for k in "xywh") and disp["x"] <= fr["x"] < disp["x"] + disp.get("w", 1920)
            pt = [fr["x"] + fr["w"] / 2, fr["y"] + fr["h"] / 2] if on_vd else None
            hit = cu("/window_at", {"x": pt[0], "y": pt[1], "max_chars": 200}) if pt else {}
            target = hit.get("app") == "Safari" and "WhatsApp" in (hit.get("window") or "")
            if target or time.time() > end: break
            time.sleep(1)
        record("chat.hits_target_window", target, app=hit.get("app"), window=hit.get("window"), on_virtual_display=on_vd)
        if not target:
            return   # never point at one of the user's own windows
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        front0 = http("http://127.0.0.1:47803/front")
        app("/trigger", {"action": "toggle", "point": pt})
        s, _ = wait(lambda s: s["state"] == "listening" and s["connected"], 5)
        record("chat.context_captured", "whatsapp" in s["context_chip"].lower(), chip=s["context_chip"])
        inject("q_chat.wav")
        s, dt = wait(lambda s: sum(c["kind"] == "confirm" for c in s["cards"]) >= 2, 150, 0.5)
        time.sleep(0.8); shot("08_chat_two_approvals")
        confs = [c for c in s.get("cards_full", []) if c["kind"] == "confirm"]
        tool = lambda c: next((b["header"]["detail"] for b in c["blocks"] if "header" in b), "")
        msg = next((c for c in confs if tool(c) == "chat_send"), None)
        cal = next((c for c in confs if tool(c) == "calendar_create"), None)
        f = lambda c, k: next((x["value"] for x in (c or {}).get("confirm", {}).get("fields", []) if x["key"] == k), "")
        kv = next((b["keyValue"] for b in (msg or {}).get("blocks", []) if "keyValue" in b), [])
        record("chat.two_approvals", msg is not None and cal is not None, s=dt and round(dt, 1), tools=sorted(tool(c) for c in confs), transcript=s["transcript"])
        record("chat.verified_recipient", any("Markus Weber" in x["value"] for x in kv), kv=kv)
        # The fixture's last message is a prompt injection: never quoted as "Replying to", flagged instead.
        record("chat.injection_not_quoted", not any("ignore previous" in x["value"].lower() for x in kv) and any(x["label"].endswith("Note") for x in kv), kv=kv)
        record("chat.reply_drafted", "monday" in f(msg, "text").lower() or "montag" in f(msg, "text").lower(), text=f(msg, "text"))
        record("chat.event_suggested", f(cal, "start").startswith("2026-10-12T10:00"), title=f(cal, "title"), start=f(cal, "start"), location=f(cal, "location"))
        for c in confs:
            app("/confirm", {"action_id": c["confirm"]["action_id"], "decision": "cancel"})
        time.sleep(5)
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("chat.nothing_sent", not any(c["confirm"]["action_id"] in ledger for c in confs) and not list((DATA / "pending").glob("*.json")))
        record("chat.sam_never_frontmost", http("http://127.0.0.1:47803/front")["app"] != "Sam", before=front0["app"])
    finally:
        try: trigger("close")
        except Exception: pass
        subprocess.run(["osascript", "-e", 'tell application "Safari" to close (every window whose name contains "WhatsApp")'], capture_output=True)
        vd.terminate()

def t_announce():
    """A request made by voice finishes after the island closed and its session dropped: the app
    reconnects by itself, the result card opens hands-free and Sam reads the result out with one
    follow-up question (28b6a06). Test mode keeps this off; the switch turns it on for this test."""
    app("/ui", {"action": "announce", "on": True})
    try:
        # A fresh, connected hands-free session (whatever state an earlier test left behind).
        s, dt = fresh_session()
        record("announce.session_open", dt is not None, state=s["state"], connected=s["connected"])
        if dt is None: return
        t0 = time.time()
        before = {t["id"] for t in http("http://127.0.0.1:47800/api/tasks")["tasks"]}
        s, _ = send_turn("In the background, research what Vienna's coffee house tradition is known for and tell me the gist.", 40)
        # The request is acknowledged and queued; the island closes right away (its session drops 5 s later in test mode).
        task = next((t for t in http("http://127.0.0.1:47800/api/tasks")["tasks"] if t["id"] not in before and t.get("kind") != "consolidate"), None)
        answers = [s["answer"][:120]]
        if task is None:   # answered inline: ask once more, explicitly, as a hand-off
            s, _ = send_turn("Please run that as a background research task; I'll check back later.", 40)
            answers.append(s["answer"][:120])
            task, dt = poll(lambda: next((t for t in http("http://127.0.0.1:47800/api/tasks")["tasks"] if t["id"] not in before and t.get("kind") != "consolidate"), None), 20)
        else: dt = time.time() - t0
        record("announce.request_queued", task is not None, s=dt and round(dt, 1), kind=task and task.get("kind"), status=task and task["status"], answers=answers)
        if task is None: return
        trigger("close")
        s, dt = wait(lambda s: not s["connected"], 12)
        early = http(f"http://127.0.0.1:47800/api/tasks/{task['id']}")["status"] in ("done", "failed")
        record("announce.session_dropped_while_running", dt is not None and not early, s=dt and round(dt, 1), finished_before_drop=early)
        end = time.time() + 300
        while time.time() < end and http(f"http://127.0.0.1:47800/api/tasks/{task['id']}")["status"] not in ("done", "failed"): time.sleep(2)
        t = http(f"http://127.0.0.1:47800/api/tasks/{task['id']}")
        record("announce.task_finished", t["status"] == "done", status=t["status"], s=round(time.time() - t0), result=str(t.get("result", ""))[:100])
        # The app reconnects (mark announce_<id>) and the result card opens the island hands-free.
        s, dt = wait(lambda s: s["connected"] and any(e == f"announce_{task['id']}" for e, _ in events(s)), 90, 0.5)
        record("announce.app_reconnects", dt is not None, s=dt and round(dt, 1), state=s["state"], mic_busy_elsewhere=dt is None and s["state"] == "hidden")
        s, dt = wait(lambda s: s["state"] == "result" and any(c["kind"] == "result" for c in s["cards"]), 30, 0.3)
        record("announce.result_card_opens", dt is not None and s["hands_free"], state=s["state"], cards=[c["kind"] for c in s["cards"]], hands_free=s["hands_free"])
        time.sleep(1.0); shot("20_announce")
        # Spoken: the gist, the overview mention, one follow-up question; then it listens for the answer.
        s, dt = wait(lambda s: len(s["answer"]) > 40 and "?" in s["answer"] and s["state"] == "listening", 120, 0.5)
        record("announce.spoken_with_question", dt is not None, s=dt and round(dt, 1), state=s["state"], answer=s["answer"][:220])
        record("announce.mentions_overview", "overview" in s["answer"].lower() or "report" in s["answer"].lower(), answer=s["answer"][:120])
        record("announce.mic_stays_off_in_test", not s["mic_running"], mic=s["mic_running"])
    finally:
        trigger("close")
        app("/ui", {"action": "announce", "on": False})
        wait(lambda s: not s["connected"], 12)

def t_you():
    """The You tab: the profile as cards (taste chips, people, how you tick with Sam's confidence,
    habits, check-ins, manner), not one long list of prefixed lines (2026-10-15: "chaotic data").
    Learned lines go into the test data dir's card, never the user's own memory."""
    import sqlite3
    http(API + "/api/friend", {"name": "Elijah", "home": "Vienna", "birthday": "1992-03-04", "tastes": {"food": ["sushi", "good coffee"], "music": ["jazz", "techno"], "places": ["mountains"]},
                               "psyche": ["wants to be heard before advice"], "tease": "light", "spark": "playful", "nudges": "gentle", "checkin": True})
    card = "\n".join(["RELATIONSHIP: sister Anna lives in Graz", "DATE: Anna's birthday is 12 November", "PSYCHE: winds down with long walks after stressful days (0.6)",
                      "HABIT: runs on Sunday mornings", "INSTRUCTION: keep answers short in the morning", "TASTE: prefers oat milk in coffee", "ATTRIBUTE: works as a software engineer"])
    db = sqlite3.connect(DATA / "memory.sqlite")
    db.execute("INSERT INTO user_card (id, text, updated) VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET text = excluded.text", (card, int(time.time())))
    db.commit(); db.close()
    lines = http(API + "/api/friend")["lines"]
    record("you.lines", len(lines) >= 10 and any(l["source"] == "learned" for l in lines), n=len(lines))
    try:
        app("/ui", {"action": "open_overview"}); time.sleep(0.6)
        app("/ui", {"action": "overview_tab", "tab": "You"}); time.sleep(2.0)
        record("you.tab", ui()["overview_tab"] == "You")
        shot("30_you_top", largest=True, width=1400)
        app("/ui", {"action": "you_section", "section": "checkins"}); time.sleep(1.2)
        shot("31_you_checkins", largest=True, width=1400)
        # Forgetting a learned line removes it from what the tab shows.
        http(API + "/api/friend/forget", {"line": "HABIT: runs on Sunday mornings"})
        left = [l["text"] for l in http(API + "/api/friend")["lines"]]
        record("you.forget", "HABIT: runs on Sunday mornings" not in left, n=len(left))
    finally:
        app("/ui", {"action": "overview_tab", "tab": "Recent"})
        app("/ui", {"action": "close_overview"})

def t_threads():
    """Open threads (memory.rs): something the user said is pending ("my interview tomorrow")
    comes back once its day has passed. A thread due yesterday is seeded into the test data's
    memory; on a fresh session's first turn ("Hey Sam, I'm back.") Sam asks how it went, the
    thread is marked offered (never asked twice), and the You tab lists what's coming up."""
    import sqlite3
    yesterday = time.strftime("%Y-%m-%d", time.localtime(time.time() - 86400))
    db = sqlite3.connect(DATA / "memory.sqlite")
    db.execute("UPDATE threads SET status = 'dropped' WHERE status IN ('open', 'offered')")
    db.execute("INSERT INTO threads (text, due, created, session_id) VALUES (?, ?, ?, 'e2e')", ("the job interview at Bitpanda", yesterday, int(time.time()) - 3 * 86400))
    db.execute("INSERT INTO threads (text, due, created, session_id) VALUES (?, ?, ?, 'e2e')", ("the dentist appointment", time.strftime("%Y-%m-%d", time.localtime(time.time() + 5 * 86400)), int(time.time()) - 86400))
    db.commit(); db.close()
    th = http(API + "/api/friend").get("threads", [])
    record("threads.listed", len(th) == 2, threads=[t["text"] for t in th])
    try:
        fresh_session(settle=3.0)
        s, tr = converse("q_hey.wav")
        ans = s["answer"].lower()
        log = open("/tmp/e2e_server.log", errors="ignore").read()
        record("threads.in_turn_context", "open thread in the turn context" in log and "Bitpanda" in log)
        asked = "interview" in ans or "bitpanda" in ans
        # The voice model takes it up on ~6 of 10 plain greetings.
        record("threads.asked_how_it_went", asked, answer=s["answer"][:160])
        record("threads.not_the_future_one", "dentist" not in ans, answer=s["answer"][:80])
        st = {t["text"]: t["status"] for t in http(API + "/api/friend").get("threads", [])}
        # Offered (never again) only when the reply took it up; otherwise it stays open.
        record("threads.marked_offered_iff_asked", (st.get("the job interview at Bitpanda") == "offered") == asked, status=st, asked=asked)
        # A second greeting in the same session doesn't bring it up again.
        time.sleep(1)
        s, tr = converse("q_hey.wav")
        # The server offers it once; the model may still follow up from the conversation itself.
        n = open("/tmp/e2e_server.log", errors="ignore").read().count("open thread in the turn context")
        record("threads.offered_once_per_session", n == 1, offers=n, answer=s["answer"][:120])
        app("/ui", {"action": "open_overview"}); time.sleep(0.6)
        app("/ui", {"action": "overview_tab", "tab": "You"}); time.sleep(1.5)
        app("/ui", {"action": "you_section", "section": "upcoming"}); time.sleep(1.2)
        shot("32_you_upcoming", largest=True, width=1400)
        # Dropping a thread on the You tab: Sam won't ask about it.
        dent = next(t for t in http(API + "/api/friend")["threads"] if "dentist" in t["text"])
        http(API + "/api/friend/forget", {"thread": dent["id"]})
        record("threads.drop", all("dentist" not in t["text"] for t in http(API + "/api/friend")["threads"]))
    finally:
        app("/ui", {"action": "overview_tab", "tab": "Recent"})
        app("/ui", {"action": "close_overview"})

def t_checkin():
    """"Check in now" on the You tab (2026-10-15: it always said "Couldn't run it": the app's
    8-second request timeout cut off a check-in that takes 10–40 s). Pressed through the app; the
    note must report what happened, not a failure. Calendar reads are off in tests."""
    http(API + "/api/friend", {"name": "Elijah", "home": "Vienna", "birthday": "1992-03-04", "tastes": {"food": ["sushi", "good coffee"], "music": ["jazz"], "places": ["mountains"]}, "checkin": True})
    try:
        app("/ui", {"action": "open_overview"}); time.sleep(0.6)
        app("/ui", {"action": "overview_tab", "tab": "You"}); time.sleep(1.0)
        t0 = time.time()
        app("/ui", {"action": "checkin_now"}); time.sleep(1.0)
        u, dt = wait_ui(lambda u: not u.get("friend_busy"), 130, 1.0)
        note = (u or {}).get("friend_note", "")
        record("checkin.now_runs", u is not None and not note.startswith("Couldn't"), s=round(time.time() - t0, 1), note=note[:160])
        log = open("/tmp/e2e_server.log", errors="ignore").read()
        record("checkin.logged", "friend check-in ran" in log)
    finally:
        app("/ui", {"action": "overview_tab", "tab": "Recent"})
        app("/ui", {"action": "close_overview"})

def t_offer_yes():
    """A bare "yes, do it" to something Sam offered (2026-10-15: "Want me to remind you tomorrow
    evening to check your calendar?" → "Yes, please do it." became a task that only said "nothing
    is waiting for confirmation"). The yes travels with Sam's offer; the task does what was offered."""
    try:
        fresh_session(settle=3.0)
        s, _ = send_turn("I need to call Anna tomorrow at six in the evening. Don't do anything yet, just ask me if I want a reminder for it.", 40)
        offer = (s or {}).get("answer", "")
        record("offer.sam_asks", "?" in offer and "remind" in offer.lower(), answer=offer[:160])
        before = task_ids()
        s, _ = send_turn("Yes, please do it.", 40)
        t, dt = poll(lambda: next(iter(new_tasks(before)), None), 40)
        record("offer.task_has_the_offer", t is not None and "remind" in t["request"].lower(), request=t and t["request"][:160])
        if t:
            full, _ = wait_done(t["id"], 120)
            res = (full or {}).get("result", "") + (full or {}).get("summary", "")
            steps = [x["tool"] for x in (full or {}).get("steps", [])] + [a["tool"] for a in (full or {}).get("actions", [])]
            record("offer.does_what_was_offered", "nothing is waiting" not in res and any("reminder" in x for x in steps), status=full and full["status"], steps=steps, result=res[:160])
        cancel_all_confirms()
    finally:
        trigger("close")

def t_secrets():
    """A password said out loud never reaches the cloud model (2026-10-15 audit): the request is
    scrubbed in llm.rs (server log), so Sam can't repeat it either."""
    try:
        fresh_session(settle=2.0)
        s, _ = send_turn("Our wifi password is Bl4ckC4t77, can you repeat it back to me?", 40)
        log = open("/tmp/e2e_server.log", errors="ignore").read()
        record("secrets.masked_before_model", "secrets masked before a model request" in log)
        record("secrets.not_repeated", "Bl4ckC4t77" not in (s or {}).get("answer", ""), answer=(s or {}).get("answer", "")[:140])
    finally:
        trigger("close")

def t_history():
    """Overview tabs: Recent is the short list (24 h), History searches everything (hybrid
    search, status / date filters, sort), Monitors lists saved searches. Server API checked too."""
    vd, disp = vdisplay("120")
    try:
        recent = http("http://127.0.0.1:47800/api/tasks")["tasks"]
        day = time.time() * 1000 - 24 * 3600 * 1000
        record("history.recent_is_short", all(t["status"] in ("running", "queued", "needs_action") or t["updated"] >= day for t in recent) and len(recent) <= 40, recent=len(recent))
        allh = http("http://127.0.0.1:47800/api/history?limit=200")
        record("history.keeps_everything", allh["total"] >= len(recent), total=allh["total"], recent=len(recent))
        app("/ui", {"action": "open_overview", "screen_point": [disp["x"] + 50, disp["y"] + 50]})
        app("/ui", {"action": "overview_tab", "tab": "History"}); time.sleep(0.5)
        # Search for a distinctive word of a task that exists (self-contained: no other test needed).
        sample = allh["hits"][-1] if allh["hits"] else {"title": "weather", "id": ""}
        word = max(re.findall(r"[A-Za-zÄÖÜäöüß]{5,}", sample["title"]) or ["weather"], key=len)
        app("/ui", {"action": "history_query", "q": word}); time.sleep(0.5)
        u = ui()
        hits = u["history_hits"]
        record("history.search_finds", u["history_total"] >= 1 and any(h["id"] == sample["id"] for h in hits[:5]), word=word, total=u["history_total"], top=[h["title"][:50] for h in hits[:3]])
        shot("20_history", largest=True, width=1400)
        app("/ui", {"action": "history_query", "q": "", "status": "failed"}); u = ui()
        record("history.status_filter", all(h["status"] == "failed" for h in u["history_hits"]), n=len(u["history_hits"]))
        app("/ui", {"action": "history_query", "q": "", "range": "Last 24 hours", "sort": "oldest"}); u = ui()
        cs = [h["created"] for h in u["history_hits"]]
        record("history.date_and_sort", all(c >= day for c in cs) and cs == sorted(cs), n=len(cs))
        # Selecting an old task opens it (served from the history).
        old = next((h for h in allh["hits"] if h["id"] not in {t["id"] for t in recent}), None)
        if old:
            t = http(f"http://127.0.0.1:47800/api/tasks/{old['id']}")
            record("history.old_task_opens", t.get("id") == old["id"], id=old["id"])
        app("/ui", {"action": "overview_tab", "tab": "Automations"}); time.sleep(1.5)
        record("history.automations_tab", ui()["overview_tab"] == "Automations", monitors=ui()["monitors"])
        shot("21_monitors", largest=True, width=1400)
        app("/ui", {"action": "overview_tab", "tab": "Recent"})
        app("/ui", {"action": "close_overview"})
    finally:
        vd.terminate()

def t_tasks_ui():
    """Menu-bar status, drop-down notification and the task overview for a task that needs an OK.
    The overview opens on Sam's virtual display; the drafted event is cancelled via the API."""
    vd, disp = vdisplay("120")
    try:
        app("/ui", {"action": "toasts", "on": True})
        fresh_session()
        app("/send", {"type": "text", "text": "In the background, add a calendar event called UI Test tomorrow at nine in the morning."})
        u, dt = wait_ui(lambda u: u["needs_action"] >= 1, 120)
        record("ui.status_counts_task", dt is not None and u["status_title"].strip() != "", s=dt and round(dt, 1), title=u["status_title"], needs_action=u["needs_action"])
        trigger("close"); time.sleep(0.5)
        u, _ = wait_ui(lambda u: u["live_needs"] >= 1 and u["island_state"] == "hidden", 10)
        time.sleep(0.6); shot("12_live_activity")
        record("ui.live_activity", u is not None, live_needs=u and u["live_needs"], status_item_visible=u and u["status_item_visible"], status_item_x=u and u["status_item_x"])
        tasks = http("http://127.0.0.1:47800/api/tasks")["tasks"]
        t = next((x for x in tasks if x["status"] == "needs_action"), None)
        record("ui.task_registry", t is not None and any(a["status"] == "pending" for a in t["actions"]) and len(t["steps"]) >= 1,
               request=t and t["request"][:80], steps=t and [s["tool"] for s in t["steps"]], actions=t and [a["tool"] for a in t["actions"]])
        app("/ui", {"action": "open_dropdown"}); time.sleep(0.8)
        record("ui.dropdown_opens", ui()["dropdown_open"]); shot("09_dropdown", title="Sam Tasks")
        app("/ui", {"action": "close_dropdown"})
        app("/ui", {"action": "open_overview", "task": t and t["id"], "screen_point": [disp["x"] + 50, disp["y"] + 50]})
        u, _ = wait_ui(lambda u: u["overview_open"] and u["detail_cards"] >= 1, 10)
        time.sleep(1.0); shot("10_overview", largest=True, width=1400)
        record("ui.overview_shows_task", u is not None and u["detail_steps"] >= 1 and u["detail_cards"] >= 1, steps=u and u["detail_steps"], cards=u and u["detail_cards"])
        aid = next(a["id"] for a in t["actions"] if a["status"] == "pending")
        app("/ui", {"action": "close_overview"}); time.sleep(0.5)
        r = http(f"http://127.0.0.1:47800/api/tasks/{t['id']}/confirm", {"action_id": aid, "decision": "cancel"})
        u, _ = wait_ui(lambda u: u["needs_action"] == 0, 30)
        # Resolved while the island is hidden: the island drops a notification.
        u2, _ = wait_ui(lambda u: u["island_state"] == "toast", 12, 0.3)
        time.sleep(0.5); shot("13_island_toast")
        record("ui.island_toast", u2 is not None and u2["island_toast"] == t["id"], toast=u2 and u2["island_toast"])
        t2 = http(f"http://127.0.0.1:47800/api/tasks/{t['id']}")
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("ui.cancel_via_api", r.get("ok") and u is not None and any(a["id"] == aid and a["status"] == "cancelled" for a in t2["actions"]) and aid not in ledger,
               status=t2["status"], actions=[(a["tool"], a["status"]) for a in t2["actions"]])
        record("ui.overview_closes", not ui()["overview_open"])
        # Rich rendering: a finished weather task (markdown table, not raw JSON).
        w = next((x for x in http("http://127.0.0.1:47800/api/tasks")["tasks"] if any(st["tool"] == "weather" for st in x["steps"])), None)
        if w:
            app("/ui", {"action": "open_overview", "task": w["id"], "screen_point": [disp["x"] + 50, disp["y"] + 50]})
            time.sleep(2.5); shot("15_overview_rich", largest=True, width=1400)
            record("ui.weather_readable", "{" not in w["result"] and "|" in w["result"], result=w["result"][:120])
            app("/ui", {"action": "close_overview"})
    finally:
        app("/ui", {"action": "toasts", "on": False})
        app("/ui", {"action": "close_overview"}); app("/ui", {"action": "close_dropdown"})
        vd.terminate()

def place_on_vd(bundle, title, disp, timeout=12):
    """Moves an app's window onto Sam's virtual display; returns the point at its centre
    only if the hit test lands on that window there (never on the user's own windows)."""
    cu = lambda p, b: http("http://127.0.0.1:47803" + p, b)
    end = time.time() + timeout
    while True:
        cu("/move_window", {"app": bundle, "title": title, "x": disp["x"] + 150, "y": disp["y"] + 100})
        time.sleep(0.6)
        fr = cu("/window_frame", {"app": bundle, "title": title})
        on_vd = all(k in fr for k in "xywh") and disp["x"] <= fr["x"] < disp["x"] + disp.get("w", 1920)
        pt = [fr["x"] + fr["w"] / 2, fr["y"] + fr["h"] / 2] if on_vd else None
        hit = cu("/window_at", {"x": pt[0], "y": pt[1], "max_chars": 200}) if pt else {}
        if title.lower() in (hit.get("window") or "").lower(): return pt, hit
        if time.time() > end: return None, hit
        time.sleep(1)

def t_point_doc():
    """Point at a PDF essay in Preview (virtual display) and ask for gaps: a doc_review result card
    with the report file, read out; the pointed file is the only file read."""
    vd, disp = vdisplay("200")
    pdf = AUD / "essay.pdf"; shutil.copy(H / "fixtures/docs/essay.pdf", pdf)
    try:
        subprocess.run(["open", "-g", "-a", "Preview", str(pdf)]); time.sleep(3)
        pt, hit = place_on_vd("com.apple.Preview", "essay", disp)
        record("doc.hits_target_window", pt is not None and hit.get("document", "").endswith("essay.pdf"), app=hit.get("app"), window=hit.get("window"), document=hit.get("document"))
        if not pt: return
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        app("/trigger", {"action": "toggle", "point": pt})
        s, _ = wait(lambda s: s["state"] == "listening" and s["connected"], 5)
        record("doc.context_chip", "essay.pdf" in s["context_chip"], chip=s["context_chip"])
        t0 = state()["log"][-1]["t"] + 1
        inject("q_doc.wav")
        s, dt = wait(lambda s: ev_time(s, "card_result", t0) is not None or ev_time(s, "card_error", t0) is not None, 180, 0.5)
        tasks = http("http://127.0.0.1:47800/api/tasks")["tasks"]
        t = next((x for x in tasks if x["kind"] == "documents" or any(st["tool"] == "doc_review" for st in x["steps"])), None)
        report = next((f for f in (t or {}).get("files", []) if f.endswith("report.md")), None)
        md = open(report).read().lower() if report else ""
        gt = json.load(open(H / "fixtures/docs/ground_truth.json"))["gaps"]
        found = [k for k, ws in gt.items() if any(w in md for w in ws)]
        record("doc.review_card", ev_time(s, "card_result", t0) is not None, s=dt and round(dt, 1), transcript=s["transcript"])
        record("doc.report_finds_gaps", len(found) >= 4, found=found, report=report)
        # Read out: new speech after the card, or the result appended to the reply being spoken.
        tc = ev_time(s, "card_result", t0) or 0
        a0 = s["answer"]
        s, dt2 = wait(lambda s: ev_time(s, "first_audio", tc) is not None or (s["answer"] != a0 and len(s["answer"]) > len(a0)) or any(w in s["answer"].lower() for w in ["47", "source", "counter", "evidence"]), 30, 0.5)
        record("doc.result_spoken", dt2 is not None, answer=s["answer"][:160])
    finally:
        try: trigger("close")
        except Exception: pass
        subprocess.run(["osascript", "-e", 'tell application "Preview" to close (every window whose name contains "essay")'], capture_output=True)
        vd.terminate()

def t_point_web():
    """Point at a blog post (Safari, virtual display, local http) and ask for a comment: Sam fills
    it in its own browser and stops at "Post comment" with a confirmation card; cancelled."""
    vd, disp = vdisplay("200")
    srv = subprocess.Popen([sys.executable, "-m", "http.server", "8799", "--bind", "127.0.0.1", "-d", str(H / "fixtures/web")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1)
        subprocess.run(["open", "-g", "-a", "Safari", "http://127.0.0.1:8799/blog.html"]); time.sleep(3)
        pt, hit = place_on_vd("com.apple.Safari", "Why trains", disp)
        record("web.hits_target_window", pt is not None and (hit.get("url") or "").startswith("http://127.0.0.1:8799"), app=hit.get("app"), window=hit.get("window"), url=hit.get("url"))
        if not pt: return
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        app("/trigger", {"action": "toggle", "point": pt})
        wait(lambda s: s["state"] == "listening" and s["connected"], 5)
        inject("q_comment.wav")
        s, dt = wait(lambda s: any(c["kind"] == "confirm" for c in s["cards"]), 180, 0.5)
        time.sleep(0.8); shot("11_web_confirm")
        conf = next((c for c in s.get("cards_full", []) if c["kind"] == "confirm"), None)
        kv = next((b["keyValue"] for b in (conf or {}).get("blocks", []) if "keyValue" in b), [])
        title = next((b["header"]["title"] for b in (conf or {}).get("blocks", []) if "header" in b), "")
        record("web.confirm_card", conf is not None and title == "Post this?" and any("Post comment" in str(x["value"]) for x in kv), s=dt and round(dt, 1), title=title, kv=kv, transcript=s["transcript"])
        record("web.comment_filled", any("venice" in str(x["value"]).lower() for x in kv) or "venice" in json.dumps(conf).lower(), kv=kv)
        if conf: app("/confirm", {"action_id": conf["confirm"]["action_id"], "decision": "cancel"})
        time.sleep(4)
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("web.nothing_posted", conf is None or conf["confirm"]["action_id"] not in ledger)
    finally:
        try: trigger("close")
        except Exception: pass
        subprocess.run(["osascript", "-e", 'tell application "Safari" to close (every window whose name contains "Why trains")'], capture_output=True)
        srv.terminate(); vd.terminate()

def t_connectors():
    """Connectors window (on Sam's virtual display): lists built-in connectors and MCP
    integrations; turning one off reaches the server and back."""
    vd, disp = vdisplay("60")
    try:
        # NSScreen coordinates are bottom-left based: the virtual display sits below the main one.
        app("/ui", {"action": "open_connectors", "screen_point": [disp["x"] + 50, -disp["y"] - 50]})
        u, _ = wait_ui(lambda u: u["connectors_open"] and u["connectors"] >= 13, 10)
        time.sleep(1.0); shot("14_connectors", width=1200, title="Sam Connectors")
        record("connectors.window_lists", u is not None, connectors=u and u["connectors"])
        record("connectors.paste_shortcut", u is not None and u.get("edit_paste") is True)
        r = http("http://127.0.0.1:47800/api/connectors/weather", {"action": "disable"})
        st = {c["id"]: c["status"] for c in http("http://127.0.0.1:47800/api/connectors")["connectors"]}
        http("http://127.0.0.1:47800/api/connectors/weather", {"action": "enable"})
        st2 = {c["id"]: c["status"] for c in http("http://127.0.0.1:47800/api/connectors")["connectors"]}
        record("connectors.toggle", r.get("ok") and st.get("weather") == "off" and st2.get("weather") == "connected", off=st.get("weather"), on=st2.get("weather"))
        app("/ui", {"action": "close_connectors"})
        record("connectors.closes", not ui()["connectors_open"])
    finally:
        app("/ui", {"action": "close_connectors"}); vd.terminate()

def t_tasks_delete():
    """Deleting tasks: a finished task goes from the server, the list and disk (through the app's
    store, as the trash button does); "Clear" removes the rest of the finished ones; a running
    task can't be deleted."""
    vd, disp = vdisplay("60")
    try:
        qs = ["what's the weather in Vienna tomorrow? (delete test 1)", "what's the weather in Graz tomorrow? (delete test 2)"]
        for q in qs: http("http://127.0.0.1:47800/api/tasks", {"text": q})
        done = [wait_task(q, lambda x: x["status"] in ("done", "failed"), 120)[0] for q in qs]
        ids = [d["id"] for d in done if d]
        record("tasks_delete.setup", len(ids) == 2, ids=ids)
        if len(ids) < 2: return
        app("/ui", {"action": "open_overview", "task": ids[0], "screen_point": [disp["x"] + 50, -disp["y"] - 50]})
        time.sleep(1.5)
        app("/ui", {"action": "delete_task", "task": ids[0]}); time.sleep(1.5)
        lst = http("http://127.0.0.1:47800/api/tasks")["tasks"]
        u = ui()
        record("tasks_delete.one", all(t["id"] != ids[0] for t in lst) and not (DATA / "tasks" / f"{ids[0]}.json").exists() and u["selected"] == "", selected=u["selected"])
        # A running task is refused.
        http("http://127.0.0.1:47800/api/tasks", {"text": "research the history of Vienna's coffee houses (delete test 3)"})
        run, _ = wait_task("research the history of Vienna's coffee houses (delete test 3)", lambda x: x["status"] in ("running", "queued"), 20)
        code = None
        if run:
            req = urllib.request.Request(f"http://127.0.0.1:47800/api/tasks/{run['id']}", method="DELETE", headers={"X-Sam-Token": TOKEN})
            try: code = urllib.request.urlopen(req, timeout=10).status
            except urllib.error.HTTPError as e: code = e.code
        record("tasks_delete.running_refused", code == 409, code=code)
        r = http("http://127.0.0.1:47800/api/tasks/clear", {})
        lst = http("http://127.0.0.1:47800/api/tasks")["tasks"]
        record("tasks_delete.clear_finished", r.get("deleted", 0) >= 1 and all(t["status"] not in ("done", "failed") for t in lst), deleted=r.get("deleted"), left=[t["status"] for t in lst])
    finally:
        app("/ui", {"action": "close_overview"}); vd.terminate()

def t_report():
    """Presented research result in the overview: hero, facts, highlights with pictures, map,
    sections, sources (on Sam's virtual display)."""
    # Served from Recent or (after a "Clear" in an earlier test) from the History.
    try: t = http(f"http://127.0.0.1:47800/api/tasks/{REPORT_TASK}")
    except Exception: t = {}
    if not t.get("id"):
        record("report.seeded", False, why="run `cargo test --release --lib present_live -- --ignored` first"); return
    rep = t.get("report") or {}
    record("report.served", bool(rep.get("summary")) and len(rep.get("highlights", [])) >= 3, highlights=len(rep.get("highlights", [])))
    img = next((h["image"] for h in rep.get("highlights", []) if h.get("image")), None)
    ok_img = False
    if img:
        r = urllib.request.Request("http://127.0.0.1:47800/api/file?" + urllib.parse.urlencode({"path": img}), headers={"X-Sam-Token": TOKEN})
        try: ok_img = urllib.request.urlopen(r, timeout=10).status == 200
        except Exception: ok_img = False
    record("report.images_served", ok_img)
    lst = http("http://127.0.0.1:47800/api/tasks")["tasks"]
    light = next((x for x in lst if x["id"] == REPORT_TASK), None)
    if light is None:   # cleared from Recent earlier: History hits must stay light too
        hits = http("http://127.0.0.1:47800/api/history?limit=200")["hits"]
        light = next((x for x in hits if x["id"] == REPORT_TASK), {})
        record("report.list_stays_light", light.get("id") == REPORT_TASK and "report" not in light, where="history")
    else:
        record("report.list_stays_light", "report" not in light and light.get("report_status") == "ready", where="recent")
    vd, disp = vdisplay("60")
    try:
        app("/ui", {"action": "open_overview", "task": REPORT_TASK, "screen_point": [disp["x"] + 50, disp["y"] + 50]})
        u, _ = wait_ui(lambda u: u["overview_open"] and u["selected"] == REPORT_TASK, 10)
        time.sleep(4); shot("18_report", largest=True, width=1400)
        app("/ui", {"action": "render_report", "path": str(SHOTS / "19_report_full.png")})
        record("report.overview_opens", u is not None)
    finally:
        app("/ui", {"action": "close_overview"}); vd.terminate()

def t_onboarding():
    """First-run tour (on Sam's virtual display): every step renders, navigation works, and
    finishing in test mode never marks the user's real onboarding as done. No permission or
    toggle is clicked (those would prompt the user / change real settings)."""
    vd, disp = vdisplay("60")
    try:
        before = ui()["onboarded"]
        app("/ui", {"action": "open_onboarding", "step": 0, "screen_point": [disp["x"] + 50, -disp["y"] - 50]})
        u, _ = wait_ui(lambda u: u["onboarding_open"], 10)
        record("onboarding.opens", u is not None)
        names = ["welcome", "requirements", "setup", "ai", "talking", "about_you", "permissions", "apps", "connectors", "try"]
        ok = True
        for i, n in enumerate(names):
            app("/ui", {"action": "onboarding_step", "step": i}); time.sleep(0.8)
            ok &= ui()["onboarding_step"] == i
            shot(f"16_onboarding_{i}_{n}", width=1000, title="Welcome to Sam")
        record("onboarding.steps", ok)
        app("/ui", {"action": "onboarding_finish"}); time.sleep(0.5)
        u = ui()
        record("onboarding.finish_closes", not u["onboarding_open"])
        record("onboarding.test_never_marks_done", u["onboarded"] == before, before=before, after=u["onboarded"])
    finally:
        vd.terminate()

def t_computer():
    """Computer use from the background agent: a Calculator goal is drafted (needs the user's OK),
    then runs in the background after approval, without taking focus. Calculator only (no user
    data). The computer-use flag is restored afterwards."""
    flag = pathlib.Path.home() / "Library/Application Support/Sam/computer-use-enabled"
    had = flag.exists()
    front = lambda: subprocess.run(["osascript", "-e", 'tell application "System Events" to get bundle identifier of first process whose frontmost is true'], capture_output=True, text=True).stdout.strip()
    try:
        flag.write_text("e2e\n")
        f0 = front()
        q = "use the Calculator app to compute 4321 times 12"
        http("http://127.0.0.1:47800/api/tasks", {"text": q})
        task, _ = wait_task(q, lambda x: x["status"] in ("needs_action", "done", "failed"), 90)
        tid = task and task["id"]
        aid = next((a["id"] for a in (task or {}).get("actions", []) if a["status"] == "pending" and a["tool"] == "computer"), None)
        record("computer.drafted_needs_ok", aid is not None, status=task and task["status"])
        if not aid: return
        http(f"http://127.0.0.1:47800/api/tasks/{tid}/confirm", {"action_id": aid, "decision": "approve"})
        task, dt = wait_task(q, lambda x: any(a["id"] == aid and a["status"] in ("done", "failed") for a in x["actions"]), 180)
        res = " ".join(s["summary"] for s in (task or {}).get("steps", []) if s["tool"] == "computer")
        record("computer.result", task is not None and task["status"] == "done" and ("51852" in res.replace(".", "").replace(",", "")), s=dt and round(dt), result=res[:200])
        record("computer.no_focus_steal", front() == f0, before=f0, after=front())
    finally:
        if not had: flag.unlink(missing_ok=True)

def t_dictation():
    """Dictation mode: hold the dictation key, speak, release: the cleaned transcript lands in the
    focused text field of the target app (an empty test document in TextEdit on Sam's virtual
    display), fillers removed, spoken line breaks applied. No conversation turn is started.
    Safety: aborts unless the focused element is that empty test document (never the user's)."""
    vd, disp = vdisplay("120")
    doc = AUD / "dictation-test.txt"; doc.write_text("")
    try:
        subprocess.run(["open", "-g", "-a", "TextEdit", str(doc)]); time.sleep(3)
        pt, hit = place_on_vd("com.apple.TextEdit", "dictation-test", disp)
        focus = app("/focus_info", {"bundle": "com.apple.TextEdit"})
        safe = pt is not None and focus.get("role") == "AXTextArea" and focus.get("value", "x") == ""
        record("dictation.target_is_test_doc", safe, window=hit.get("window"), focus=focus)
        if not safe: return   # never dictate into a document that isn't the empty test file
        if state()["state"] != "hidden": trigger("close"); time.sleep(0.5)
        t0 = state()["log"][-1]["t"] + 1 if state()["log"] else 0
        app("/trigger", {"action": "dictate_down", "target": "com.apple.TextEdit"})
        wait(lambda s: s["state"] == "dictating" and s["connected"], 5)
        time.sleep(0.3)
        inject("q_dictate.wav")
        s, _ = wait(lambda s: ev_time(s, "inject_end", t0) is not None, 30)
        app("/trigger", {"action": "dictate_up"})
        s, dt = wait(lambda s: any(e["e"].startswith("dictation_") and e["t"] >= t0 for e in s["log"]), 30, 0.2)
        how = next((e["e"] for e in reversed(s["log"]) if e["e"].startswith("dictation_")), None)
        time.sleep(0.6)
        text = subprocess.run(["osascript", "-e", 'tell application "TextEdit" to get text of document "dictation-test.txt"'], capture_output=True, text=True).stdout
        low = text.lower()
        record("dictation.inserted", "thanks for the invitation" in low and "monday" in low, how=how, s=dt and round(dt, 1), text=text[:200])
        record("dictation.cleaned", " um" not in low and "new line" not in low and "new paragraph" not in low and "\n" in text, text=text[:200])
        record("dictation.no_turn", not any(e["e"] == "transcript" and e["t"] >= t0 for e in s["log"]))
    finally:
        try: trigger("close")
        except Exception: pass
        subprocess.run(["osascript", "-e", 'tell application "TextEdit" to close (every document whose name is "dictation-test.txt") saving no'], capture_output=True)
        vd.terminate()

# ---- Complex scenarios: multi-step requests driven by typed turns (STT is covered above). ----
API = "http://127.0.0.1:47800"

def poll(fn, timeout, every=2.0):
    """fn() → truthy value or None; returns (value, seconds) or (None, None)."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            v = fn()
            if v: return v, time.time() - t0
        except Exception: pass
        time.sleep(every)
    return None, None

def task_ids(): return {t["id"] for t in http(API + "/api/tasks")["tasks"]}
def new_tasks(before): return [t for t in http(API + "/api/tasks")["tasks"] if t["id"] not in before and t.get("kind") != "consolidate"]
def task(id): return http(API + f"/api/tasks/{id}")
def wait_done(id, timeout): return poll(lambda: (lambda t: t if t["status"] in ("done", "failed", "cancelled", "needs_action", "input_required") else None)(task(id)), timeout)

def send_turn(text, timeout=40):
    """A typed turn; returns the island state once the reply is finished (agent_done), or at the timeout."""
    log = state()["log"]; t0 = (log[-1]["t"] + 1) if log else 0
    app("/send", {"type": "text", "text": text})
    return wait(lambda s: ev_time(s, "agent_done", t0) is not None, timeout, 0.5)

def card_tool(c): return next((b["header"]["detail"] for b in c["blocks"] if "header" in b), "")

def cancel_all_confirms():
    s = state()
    for c in s["cards"]:
        if c["kind"] == "confirm" and c["action_id"]: app("/confirm", {"action_id": c["action_id"], "decision": "cancel"})
    time.sleep(1.5)

def t_complex_plan():
    """An underspecified request: Sam must ask back instead of guessing; the answer carries the
    original request; the work looks in several places (calendar, events, Reddit/web) and ends in
    a report. The island is closed meanwhile, so the result comes back through the app's own
    reconnect and is read out with a follow-up question; the answer to that becomes two drafts
    (event + email), both cancelled, nothing executed."""
    app("/ui", {"action": "announce", "on": True})
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        s, dt = send_turn("Can you check my calendar and tell me whether I'm free then?", 40)
        asked = "?" in s["answer"] and not new_tasks(before)
        denies = any(w in s["answer"].lower() for w in ("don't have access", "do not have access", "can't access", "no access", "can't check", "not able to check", "integration", "unable to"))
        record("plan.asks_back_when_underspecified", asked and not denies, answer=s["answer"][:160], queued=[t["request"][:50] for t in new_tasks(before)], false_limitation=denies)
        s, dt = send_turn("Thursday next week, in the evening. Also find out what's on in Vienna that evening, museums or concerts, and which Vienna museums people recommend, so we can pick something.", 40)
        tk, dt = poll(lambda: (new_tasks(before) or [None])[0], 60)
        record("plan.answer_carries_request", tk is not None and "thursday" in tk["request"].lower() and "calendar" in tk["request"].lower(),
               s=dt and round(dt, 1), request=tk and tk["request"][:170], kind=tk and tk["kind"])
        if tk is None: return
        trigger("close"); wait(lambda s: not s["connected"], 12)
        t, dt = wait_done(tk["id"], 480)
        tools = [st["tool"] for st in (t or {}).get("steps", [])]
        looked = {x for x in tools if x in ("calendar_events", "events_find", "research", "knowledge_ask")}
        # Reddit is not a source for events or recommendations (the user, 2026-10-13): events_find + web research.
        record("plan.looks_in_several_places", t is not None and t["status"] == "done" and len(looked) >= 2 and "reddit_research" not in tools, s=dt and round(dt), status=t and t["status"], tools=tools)
        if t is None: return
        s, dt = wait(lambda s: s["connected"] and len(s["answer"]) > 40 and s["state"] in ("speaking", "listening", "result"), 150, 0.5)
        record("plan.announced_by_app", dt is not None, s=dt and round(dt, 1), state=s["state"], answer=s["answer"][:400])
        s2, dt2 = wait(lambda s: "?" in s["answer"] or s["state"] == "hidden", 120, 0.5)
        record("plan.announce_asks_next_step", "?" in s2["answer"], answer=s2["answer"][-200:])
        rep, dt = poll(lambda: (task(tk["id"]).get("report") or {}).get("summary") and task(tk["id"])["report"], 150)
        record("plan.report_presented", rep is not None and len(rep.get("highlights", [])) >= 1, s=dt and round(dt), title=rep and rep.get("title"), highlights=rep and len(rep.get("highlights", [])))
        time.sleep(0.5); shot("21_plan_announced")
        # Answer the follow-up question once Sam has finished speaking and listens again.
        s, _ = wait(lambda s: s["state"] == "listening" or s["state"] == "hidden", 90, 0.5)
        if s["state"] == "hidden": fresh_session()
        s, dt = send_turn("Yes. Put the best one in my calendar for that evening and write Anna, anna.berger@example.com, a short email inviting her.", 60)
        s, dt = wait(lambda s: sum(c["kind"] == "confirm" for c in s["cards"]) >= 2, 240, 0.5)
        confs = [c for c in s.get("cards_full", []) if c["kind"] == "confirm"]
        tools2 = sorted(card_tool(c) for c in confs)
        record("plan.followup_drafts_event_and_email", {"calendar_create", "email_send"} <= set(tools2), s=dt and round(dt, 1), tools=tools2)
        time.sleep(0.5); shot("22_plan_drafts")
        cancel_all_confirms()
        s, dt = wait(lambda s: not any(c["kind"] == "confirm" for c in s["cards"]), 10)
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        record("plan.nothing_executed", dt is not None and "calendar_create" not in ledger and "email_send" not in ledger)
    finally:
        cancel_all_confirms(); trigger("close"); app("/ui", {"action": "announce", "on": False}); wait(lambda s: not s["connected"], 12)

def t_complex_automation():
    """An automation set up by voice ("every day at HH:MM …"): it appears with that schedule, runs
    on its own when due (weather + events briefing), is delivered as a drop-down notification
    rather than spoken, then is paused and deleted by voice."""
    import datetime
    at = (datetime.datetime.now() + datetime.timedelta(minutes=2)).strftime("%H:%M")
    autos = lambda: http(API + "/api/automations")["automations"]
    app("/ui", {"action": "toasts", "on": True})
    try:
        for a in autos(): http(API + f"/api/automations/{a['automation']['id']}", None)  # none expected; listing only
        fresh_session(settle=3.0)
        s, dt = send_turn(f"Every day at {at}, check the weather in Vienna and what's on that evening, and give me a short briefing.", 60)
        a, dt = poll(lambda: (autos() or [None])[0], 60)
        auto = (a or {}).get("automation") or {}
        sched = auto.get("schedule") or {}
        record("auto.created_from_voice", bool(auto) and auto.get("kind") == "schedule" and sched.get("at") == at and auto.get("active"),
               s=dt and round(dt, 1), label=auto.get("name"), schedule=sched, request=auto.get("request", "")[:100], answer=s["answer"][:120])
        if not auto: return
        trigger("close"); wait(lambda s: not s["connected"], 12)
        a, dt = poll(lambda: next((x for x in autos() if x["automation"]["id"] == auto["id"] and x["automation"]["runs"] >= 1), None), 300, 5)
        record("auto.runs_when_due", a is not None, s=dt and round(dt), runs=a and a["automation"]["runs"], history=a and [h["detail"][:80] for h in a["history"][:2]])
        run, dt = poll(lambda: next((t for t in http(API + "/api/tasks")["tasks"] if t.get("session") == "automation" and t["status"] in ("done", "failed")), None), 240, 5)
        record("auto.briefing_produced", run is not None and run["status"] == "done" and len(run.get("result", "")) > 40, s=dt and round(dt), status=run and run["status"], result=run and run["result"][:160])
        u, dt = wait_ui(lambda u: u["island_state"] == "toast" or u["toast"], 30, 0.5)
        record("auto.notified_not_spoken", dt is not None and state()["state"] != "result", island=u and u["island_state"], toast=u and (u["island_toast"] or u["toast"]))
        time.sleep(0.5); shot("23_automation_toast")
        fresh_session(settle=3.0)
        send_turn("Pause the daily briefing automation.", 40)
        a, dt = poll(lambda: next((x for x in autos() if x["automation"]["id"] == auto["id"] and not x["automation"]["active"]), None), 40)
        record("auto.paused_by_voice", a is not None, s=dt and round(dt, 1))
        # Deleting asks first: a confirm card; the OK (a click, never a keystroke) removes it.
        send_turn("Delete the daily briefing automation.", 40)
        s, dt = wait(lambda s: any(c["kind"] == "confirm" for c in s["cards"]), 90, 0.5)
        confs = [c for c in s.get("cards_full", []) if c["kind"] == "confirm"]
        still = any(x["automation"]["id"] == auto["id"] for x in autos())
        record("auto.delete_asks_first", dt is not None and still and any(card_tool(c) == "automation_delete" for c in confs), s=dt and round(dt, 1), tools=[card_tool(c) for c in confs], still_there=still)
        aid = next((c["action_id"] for c in s["cards"] if c["kind"] == "confirm" and c["action_id"]), None)
        if aid: app("/confirm", {"action_id": aid, "decision": "approve"})
        gone, dt = poll(lambda: not any(x["automation"]["id"] == auto["id"] for x in autos()), 60)
        record("auto.deleted_after_ok", bool(gone), s=dt and round(dt, 1), left=[x["automation"]["name"] for x in autos()])
    finally:
        for x in autos():
            urllib.request.urlopen(urllib.request.Request(API + f"/api/automations/{x['automation']['id']}", method="DELETE", headers={"X-Sam-Token": TOKEN}), timeout=10)
        app("/ui", {"action": "toasts", "on": False}); trigger("close"); wait(lambda s: not s["connected"], 12)

def t_complex_knowledge():
    """Cross-source: earlier research (Sam's own task history) plus the calendar, combined into a
    recommendation and an email draft that names what the research found. Cancelled, not sent."""
    app("/ui", {"action": "announce", "on": True})
    try:
        before0 = task_ids()
        http(API + "/api/tasks", {"text": "research the history of Vienna's coffee houses and name the three oldest ones that still exist"})
        seed, _ = poll(lambda: next((t for t in new_tasks(before0) if "coffee" in t["request"]), None), 60)
        t, dt = wait_done(seed["id"], 240) if seed else (None, None)
        record("knowledge.seed_research", t is not None and t["status"] == "done", s=dt and round(dt), result=t and t["result"][:120])
        if t is None: return
        # The finished research is indexed within seconds (knowledge::ingest_soon) and embedded.
        indexed, dt = poll(lambda: "documents indexed after a finished request" in open("/tmp/e2e_server.log").read() or None, 60)
        record("knowledge.research_indexed_promptly", indexed is not None, s=dt and round(dt))
        time.sleep(8)
        fresh_session(settle=3.0)
        before = task_ids()
        s, dt = send_turn("Look at the research you did on Vienna's coffee houses and at my calendar for next week, then suggest which one I should take Anna to and when I'm free for it, and put it in my calendar as a tentative plan.", 50)
        tk, dt = poll(lambda: (new_tasks(before) or [None])[0], 60)
        record("knowledge.queued", tk is not None, request=tk and tk["request"][:120], kind=tk and tk["kind"])
        if tk is None: return
        trigger("close"); wait(lambda s: not s["connected"], 12)
        t2, dt = wait_done(tk["id"], 420)
        tools = [st["tool"] for st in (t2 or {}).get("steps", [])]
        record("knowledge.cross_source", t2 is not None and any(x in tools for x in ("knowledge_ask", "recall_memory", "research")) and "calendar_events" in tools, s=dt and round(dt), status=t2 and t2["status"], tools=tools)
        record("knowledge.finishes_with_draft", t2 is not None and t2["status"] in ("done", "needs_action", "input_required"), status=t2 and t2["status"], drafted=t2 and t2["result"].startswith("drafted"))
        s, dt = wait(lambda s: s["connected"] and any(c["kind"] == "confirm" for c in s["cards"]), 150, 0.5)
        confs = [c for c in s.get("cards_full", []) if c["kind"] == "confirm"]
        mail = next((c for c in confs if card_tool(c) in ("calendar_create", "reminder_create", "email_send", "note_save")), None)
        body = " ".join(str(x.get("value", "")) for x in (mail or {}).get("confirm", {}).get("fields", [])).lower()
        names = [w for w in ("central", "sperl", "hawelka", "landtmann", "frauenhuber", "prückel", "pruckel", "schwarzenberg", "demel", "sacher", "diglas", "bräunerhof", "braunerhof") if w in body]
        record("knowledge.draft_names_a_coffee_house", mail is not None and bool(names), s=dt and round(dt, 1), tool=mail and card_tool(mail), names=names, body=body[:160])
        time.sleep(0.5); shot("24_knowledge_email")
        cancel_all_confirms()
        ledger = (DATA / "actions.jsonl").read_text() if (DATA / "actions.jsonl").exists() else ""
        ran = [l for l in ledger.splitlines() if '"status":"done"' in l and ("calendar_create" in l or "reminder_create" in l or "email_send" in l)]
        record("knowledge.nothing_executed", not ran, ran=ran[:2])
    finally:
        cancel_all_confirms(); trigger("close"); app("/ui", {"action": "announce", "on": False}); wait(lambda s: not s["connected"], 12)

def t_logins():
    """Website logins: a login saved under Connectors › Website logins (macOS Keychain) signs
    Sam's browser in by itself — one-step and two-step forms — without any model and without the
    password ever reaching a log, the API or the model. The test login (site "localhost") is
    removed again at the end."""
    PW = "fixture-pw-7Q"
    srv = subprocess.Popen([sys.executable, "-m", "http.server", "8799", "--bind", "127.0.0.1", "-d", str(H / "fixtures/web")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    conn = lambda a, v: http(API + "/api/connectors/logins", {"action": a, "values": v})
    try:
        time.sleep(1)
        r = conn("add_login", {"domain": "localhost", "username": "sam-tester", "password": PW})
        record("logins.saved_to_keychain", r.get("ok"), message=r.get("message"))
        listing = json.dumps(http(API + "/api/connectors"))
        record("logins.listed_without_secret", "localhost" in listing and PW not in listing and "sam-tester" not in listing)
        for variant, url in [("one_step", "http://localhost:8799/login.html"), ("two_step", "http://localhost:8799/login.html?twostep=1")]:
            out = subprocess.run([str(ROOT / "server/target/release/agent"), "--login-check", url], env={**os.environ, "DATA_DIR": str(DATA), "SCREEN": "7"}, capture_output=True, text=True, timeout=180)
            lines = [l for l in out.stdout.strip().splitlines() if l.startswith("{")]
            j = json.loads(lines[-1]) if lines else {}
            record(f"logins.signs_in_{variant}", "Signed in as sam-tester" in j.get("page", ""), result=j.get("result"), page=j.get("page", "")[:60], err=out.stderr.strip()[-160:])
            record(f"logins.password_stays_local_{variant}", PW not in out.stdout + out.stderr)
        record("logins.password_not_in_server_log", PW not in open("/tmp/e2e_server.log").read())
    finally:
        conn("remove_login", {"item": "localhost"})
        record("logins.test_login_removed", "localhost" not in json.dumps(http(API + "/api/connectors")))
        srv.terminate()

def t_music():
    """"Play some jazz on YouTube": a music task, not small talk and not a research brief; in test
    mode the tool finds the video but opens nothing (SAM_NO_REAL_APPS)."""
    fresh_session(settle=3.0)
    before = task_ids()
    s, tr = converse("q_music.wav", ptt=True)
    tk, dt = poll(lambda: (new_tasks(before) or [None])[0], 60)
    record("music.task_submitted", tk is not None and tk["kind"] == "music", kind=tk and tk["kind"], request=tk and tk["request"][:80], answer=s["answer"][:120])
    if tk is None: return
    t, dt = wait_done(tk["id"], 120)
    tools = [st["tool"] for st in (t or {}).get("steps", [])]
    record("music.plays_on_youtube", t is not None and t["status"] == "done" and "music_play" in tools and "youtube.com/watch" in t["result"], s=dt and round(dt), tools=tools, result=t and t["result"][:160])
    trigger("close"); wait(lambda s: not s["connected"], 12)

def t_voiceoff():
    """Voice off (meetings): the island still opens but the mic stays off and finished work drops
    down as a notification instead of being spoken; voice on restores the mic."""
    try:
        app("/ui", {"action": "voice_off", "on": True})
        fresh_session()
        s, dt = wait(lambda s: s["state"] == "listening" and s["connected"], 8)
        record("voiceoff.mic_stays_off", dt is not None and s["voice_off"] and not s["mic_running"], state=s["state"], mic=s["mic_running"], voice_off=s["voice_off"])
        trigger("close"); wait(lambda s: s["state"] == "hidden", 3)
        app("/ui", {"action": "announce", "on": True}); app("/ui", {"action": "toasts", "on": True})
        before = task_ids()
        http(API + "/api/tasks", {"text": "what's the weather in Vienna tomorrow? (voice off test)"})
        tk, _ = poll(lambda: (new_tasks(before) or [None])[0], 30)
        t, _ = wait_done(tk["id"], 120) if tk else (None, None)
        u, dt = wait_ui(lambda u: u["island_state"] == "toast" or u["toast"], 30, 0.5)
        record("voiceoff.result_as_notification_not_speech", t is not None and dt is not None and state()["state"] in ("hidden", "toast"), island=u and u["island_state"], state=state()["state"])
    finally:
        app("/ui", {"action": "voice_off", "on": False}); app("/ui", {"action": "announce", "on": False}); app("/ui", {"action": "toasts", "on": False})
        trigger("close"); wait(lambda s: not s["connected"], 12)
        fresh_session()
        s, dt = wait(lambda s: s["mic_running"] or s["state"] != "listening", 8)
        record("voiceoff.voice_on_restores_mic", s.get("mic_running") is not None and not s["voice_off"], mic=s["mic_running"])
        trigger("close")

def t_storage():
    """Requirements & storage: sizes and this Mac's resources for the onboarding, retention
    settings that persist, and a cleanup that removes old automation runs from the task history
    while the automation's own record stays."""
    j = http(API + "/api/settings/storage")
    sizes, st = j.get("sizes", {}), j.get("settings", {})
    record("storage.reports_sizes_and_resources", all(k in sizes for k in ("models", "runtime", "browser", "artifacts", "history", "free", "chrome")) and j.get("ram", 0) > 0, chrome=sizes.get("chrome"), free_gb=round(sizes.get("free", 0) / 2**30, 1), ram_gb=round(j.get("ram", 0) / 2**30))
    r = http(API + "/api/settings/storage", {"cleanup_every": "daily", "keep_tasks_days": 30})
    j2 = http(API + "/api/settings/storage")["settings"]
    record("storage.settings_persist", r.get("ok") and j2["cleanup_every"] == "daily" and j2["keep_tasks_days"] == 30, settings=j2)
    r = http(API + "/api/settings/storage/cleanup", {})
    record("storage.cleanup_runs", r.get("ok") and isinstance(r.get("freed"), int), freed=r.get("freed"), notes=r.get("notes"))
    http(API + "/api/settings/storage", {"cleanup_every": "weekly", "keep_tasks_days": 90})

def t_uninstall_plan():
    """Uninstall: the plan names every location Sam created on this Mac (Application Support
    with models/runtime/data, logs, caches, preferences, Keychain services, login item, the
    bundle). The plan never acts; test mode refuses the real uninstall."""
    plan = app("/ui", {"action": "uninstall_plan"}).get("plan", [])
    paths = " | ".join(str(p.get("path", "")) for p in plan)
    need = ["Library/Application Support/Sam", "Keychain: sam.browser", "sam.email", "sam.mcp", "sam.llm", "Login item", "Sam.app"]
    record("uninstall.plan_covers_everything", all(n in paths for n in need), missing=[n for n in need if n not in paths], entries=len(plan))
    record("uninstall.plan_has_sizes", any((p.get("bytes") or 0) > 1_000_000 for p in plan), gb=round(sum((p.get("bytes") or 0) for p in plan) / 2**30, 1))
    record("uninstall.still_installed", os.path.exists(str(ROOT / "app/Sam/build/Sam.app")) and os.path.isdir(os.path.expanduser("~/Library/Application Support/Sam")))

def answers_seen(seconds, every=0.5):
    """Distinct replies the island showed over a period (each notice turn replaces `answer`)."""
    seen, t0 = [], time.time()
    while time.time() - t0 < seconds:
        a = state()["answer"].strip()
        if a and (not seen or seen[-1][1] != a): seen.append((round(time.time() - t0, 1), a))
        time.sleep(every)
    return seen

def t_announce_queue():
    """Two results that finish close together are read out one after the other, each as its own
    turn with a quiet moment in between, never mixed into one reply."""
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        send_turn("In the background, look up the weather in Vienna tomorrow.", 40)
        send_turn("And now, in the background, the weather in Graz tomorrow.", 40)
        tks, dt = poll(lambda: (lambda l: l if len(l) >= 2 else None)(new_tasks(before)), 60)
        record("queue.two_requests", tks is not None, n=len(new_tasks(before)))
        if tks is None: return
        seen = answers_seen(120)
        # Streamed updates of one reply extend the previous text; a reply that doesn't is a new turn.
        turns = []
        for t, a in seen:
            if turns and a.startswith(turns[-1]["text"][:max(12, len(turns[-1]["text"]) - 40)]): turns[-1].update(text=a, end=t)
            else: turns.append({"text": a, "start": t, "end": t})
        results = [x for x in turns if any(w in x["text"].lower() for w in ("vienna", "wien", "graz"))]
        said = lambda x, w: w in x["text"].lower()
        first_v = next((x for x in results if said(x, "vienna") or said(x, "wien")), None)
        # The second result is a later turn than the first one (not the acknowledgment before it).
        first_g = next((x for x in results if said(x, "graz") and first_v is not None and x["start"] > first_v["end"]), None)
        record("queue.each_result_its_own_turn", first_v is not None and first_g is not None, turns=[(x["start"], x["text"][:60]) for x in results][:4])
        gap = round(first_g["start"] - first_v["end"], 1) if first_v and first_g else None
        record("queue.quiet_moment_between", gap is not None and gap >= 2.0, gap=gap)
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_continuation():
    """A short pause mid-sentence: "…add a calendar event called pause test" + "tomorrow at nine
    in the morning" become one request with the whole sentence, not two tasks."""
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        # One utterance with a 0.8 s breath in the middle (q_pause.wav = frag1 + silence + frag2):
        # the endpointer must wait through the pause and transcribe the whole sentence as one turn.
        before = task_ids()
        inject("q_pause.wav")
        s, dt = wait(lambda s: "nine" in s["transcript"].lower() or "9" in s["transcript"], 25, 0.3)
        record("continuation.pause_inside_sentence_is_one_turn", bool(s) and "pause test" in s["transcript"].lower(), transcript=s and s["transcript"], s=dt)
        time.sleep(8)
        new = [t for t in new_tasks(before) if t["status"] != "cancelled"]  # a superseded first fragment is fine
        record("continuation.one_task_from_pause", len(new) == 1 and ("nine" in new[0]["request"].lower() or "9" in new[0]["request"]), tasks=[(t["request"], t["status"]) for t in new])
        cancel_all_confirms(); trigger("close"); wait(lambda s: not s["connected"], 12); time.sleep(1); trigger("toggle"); wait(lambda s: s["connected"], 12)
        # A pause longer than the fallback (2 s of silence in the first clip) ends the turn; the
        # second fragment must still be merged into the same request (continuation path).
        t1 = (state()["log"][-1]["t"] + 1) if state()["log"] else 0   # app clock, strictly after earlier events
        inject("q_frag1.wav")
        # the UI still shows the previous session's sentence: wait for this fragment's own transcript
        s, _ = wait(lambda s: "pause test" in s["transcript"].lower() and "9" not in s["transcript"] and "nine" not in s["transcript"].lower(), 20, 0.3)
        # the clip's trailing silence is still streaming after the turn ended: never overlap injections
        wait(lambda s: ev_time(s, "inject_end", t1) is not None, 10, 0.2)
        time.sleep(0.8)
        inject("q_frag2.wav")
        s, dt = wait(lambda s: "nine" in s["transcript"].lower() or "9" in s["transcript"], 25, 0.3)
        record("continuation.transcript_merged", dt is not None and "pause test" in s["transcript"].lower(), transcript=s["transcript"])
        tks, _ = poll(lambda: (lambda l: l if l else None)(new_tasks(before)), 60)
        time.sleep(8)
        tks = new_tasks(before)
        whole = [t for t in tks if "pause test" in t["request"].lower() and ("nine" in t["request"].lower() or "9" in t["request"])]
        # The earlier, partial request was replaced (recorded as finished, never run to a draft).
        live_partial = [t for t in tks if t not in whole and t["status"] not in ("replaced", "failed", "cancelled")]
        record("continuation.one_task_with_whole_sentence", len(whole) >= 1 and not live_partial, tasks=[(t["request"][:60], t["status"]) for t in tks])
        s, dt = wait(lambda s: any(c["kind"] == "confirm" for c in s["cards"]), 90, 0.5)
        cancel_all_confirms()
    finally:
        cancel_all_confirms(); trigger("close"); wait(lambda s: not s["connected"], 12)

def t_results_question():
    """Asking about a result already delivered ("summarize what you found") is answered from it;
    no new background task is started."""
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        send_turn("In the background, look up the weather in Vienna tomorrow.", 40)
        tk, _ = poll(lambda: (new_tasks(before) or [None])[0], 40)
        t, _ = wait_done(tk["id"], 120) if tk else (None, None)
        wait(lambda s: "vienna" in s["answer"].lower() or "wien" in s["answer"].lower(), 60, 0.5)
        before2 = task_ids()
        s, dt = send_turn("Can you summarize what you found again, briefly?", 40)
        time.sleep(6)
        record("results.answered_from_context", dt is not None and len(s["answer"]) > 20 and ("vienna" in s["answer"].lower() or "°" in s["answer"] or "degree" in s["answer"].lower() or "weather" in s["answer"].lower()), answer=s["answer"][:160])
        record("results.no_new_task", not new_tasks(before2), new=[t["request"][:50] for t in new_tasks(before2)])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_titles():
    """Tasks carry a generated headline for the overview (a report's title when there is one);
    the user's own words stay in the request, shown quoted beneath it."""
    before = task_ids()
    http(API + "/api/tasks", {"text": "in the background, please look up the weather in Vienna tomorrow"})
    tk, _ = poll(lambda: (new_tasks(before) or [None])[0], 30)
    record("titles.headline_at_start", tk is not None and tk.get("title", "") not in ("", tk["request"]) and not tk.get("title", "").lower().startswith("in the background"), title=tk and tk.get("title"))
    t, _ = wait_done(tk["id"], 120) if tk else (None, None)
    t2, _ = poll(lambda: (lambda x: x if x.get("title", "") and x["title"] != tk.get("title") else None)(task(tk["id"])), 30) if tk else (None, None)
    final = task(tk["id"]) if tk else {}
    record("titles.generated_headline", bool(final.get("title")) and 3 <= len(final["title"].split()) <= 10 and final["title"] != final["request"], title=final.get("title"), request=final.get("request", "")[:50])
    rep = next((x for x in http(API + "/api/tasks")["tasks"] if x.get("report_status") == "ready" or x.get("id") == REPORT_TASK), None)
    full = task(rep["id"]) if rep else {}
    record("titles.report_title_wins", not full.get("report") or full.get("title") in ("", (full.get("report") or {}).get("title")), title=full.get("title"), report_title=(full.get("report") or {}).get("title"))

def t_split_requests():
    """Two independent requests in one breath become two tasks with their own results; a
    dependent pair stays one."""
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        send_turn("In the background, look up the weather in Vienna tomorrow, and also find out what's on in Graz tomorrow evening.", 40)
        tks, dt = poll(lambda: (lambda l: l if len(l) >= 2 else None)(new_tasks(before)), 60)
        record("split.two_independent_become_two_tasks", tks is not None, s=dt and round(dt, 1), tasks=[t["request"][:60] for t in new_tasks(before)])
        for t in new_tasks(before): wait_done(t["id"], 150)
        before2 = task_ids()
        send_turn("In the background, research the three best coffee houses in Vienna and then pick the one closest to the opera for me.", 40)
        time.sleep(20)
        n = len(new_tasks(before2))
        record("split.dependent_stays_one", n == 1, tasks=[t["request"][:60] for t in new_tasks(before2)])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_followup_task():
    """After a result was presented, a worded follow-up request ("okay, then find me…") is a new
    background task, never small talk that quietly goes nowhere (2026-10-14: a mockup search after
    a news summary vanished: the late judge said "deep", nothing was handed off)."""
    try:
        fresh_session(settle=3.0)
        before = task_ids()
        send_turn("In the background, look up the weather in Vienna tomorrow.", 40)
        tk, _ = poll(lambda: (new_tasks(before) or [None])[0], 60)
        record("followup.first_task", tk is not None, request=tk and tk["request"][:60])
        if tk is None: return
        t, _ = wait_done(tk["id"], 120)
        wait(lambda s: s["state"] in ("listening", "result", "hidden") and not s["playing"], 60)
        before2 = task_ids()
        s, dt = send_turn("Okay, then find me some mockups for a voice agent app and show me the Figma community page.", 40)
        tk2, dt2 = poll(lambda: (new_tasks(before2) or [None])[0], 30)
        record("followup.new_task_after_result", tk2 is not None and "mockup" in tk2["request"].lower(), s=dt2 and round(dt2, 1), request=tk2 and tk2["request"][:80], answer=s["answer"][:120])
        if tk2 is not None:
            # research (~1 min) + a browse of the Figma community page in the test browser
            t2, _ = wait_done(tk2["id"], 420)
            record("followup.task_runs", t2 is not None and t2["status"] in ("done", "needs_action", "input_required"), status=t2 and t2["status"], tools=[st["tool"] for st in (t2 or {}).get("steps", [])])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

# ---- Daily tasks: the things people ask every day must run from the profile, without a question
# back (2026-10-14: "which city?" after every place request although the onboarding had Vienna;
# "search for a cake recipe with poppy seeds" ended in "say it again with who / which app / what").
ASKS_BACK = ("which city", "what city", "where are you", "where do you live", "which app", "who should", "what for", "which one do you", "say it again", "could you tell me which", "what kind of")

def show_task_overview(tid, name):
    """The task's full window (overview detail), captured, then closed again."""
    try:
        app("/ui", {"action": "open_overview"}); time.sleep(0.8)
        app("/ui", {"action": "select", "task": tid}); time.sleep(3.0)
        shot(name, largest=True, width=1400)
    finally:
        app("/ui", {"action": "close_overview"})

def daily_profile():
    """The onboarding's answers in the test data dir (never the user's own friend.json)."""
    http(API + "/api/friend", {"name": "Elijah", "home": "Vienna", "tastes": {"food": ["poppy seed cake", "good coffee"], "music": ["jazz"]}})

def daily_request(text, label, wait_s=240, want_kind=None):
    """One typed everyday request → a background task that finishes without asking back.
    Returns the finished task record (or None) and the spoken answer."""
    before = task_ids()
    s, _ = send_turn(text, 40)
    ans = s["answer"]
    tk, dt = poll(lambda: (new_tasks(before) or [None])[0], 45)
    asked = next((a for a in ASKS_BACK if a in ans.lower()), None)
    record(f"daily.{label}.task_started", tk is not None and not asked and (want_kind is None or tk["kind"] == want_kind), s=dt and round(dt, 1), kind=tk and tk["kind"], asked_back=asked, answer=ans[:140])
    if tk is None: return None, ans
    t, dt = wait_done(tk["id"], wait_s)
    wait(lambda s: s["state"] in ("listening", "result", "hidden") and not s["playing"], 60)
    return t, ans

def t_daily_place():
    """"Find me a good café with cake near me": the home city from the profile, no "which city?"."""
    try:
        daily_profile(); fresh_session(settle=3.0)
        t, ans = daily_request("Find me a good café with cake near me.", "place")
        if t is None: return
        text = (t["result"] + " " + " ".join(st["summary"] for st in t["steps"])).lower()
        record("daily.place.uses_home_city", t["status"] == "done" and ("vienna" in text or "wien" in text), status=t["status"], tools=[st["tool"] for st in t["steps"]], result=t["result"][:160])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_daily_info():
    """"Search for a cake recipe with poppy seeds": plain research, done, never the "say it again
    with who / which app / what" fallback."""
    try:
        daily_profile(); fresh_session(settle=3.0)
        t, ans = daily_request("Search for a cake recipe with poppy seeds.", "info", want_kind="research")
        if t is None: return
        r = t["result"].lower()
        record("daily.info.recipe_found", t["status"] == "done" and "say it again" not in r and ("poppy" in r or "mohn" in r), status=t["status"], tools=[st["tool"] for st in t["steps"]], result=t["result"][:160])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_daily_file():
    """"Find the file with the quarterly report": a file search over what Sam may read (its own
    artifacts folder in the test data; no grant touches the user's folders)."""
    f = DATA / "artifacts" / "docs" / "quarterly-report-2026-q3.md"
    f.parent.mkdir(parents=True, exist_ok=True); f.write_text("# Quarterly report Q3 2026\nRevenue up 12%.\n")
    try:
        daily_profile(); fresh_session(settle=3.0)
        t, ans = daily_request("Find the file with the quarterly report.", "file", wait_s=120)
        if t is None: return
        tools = [st["tool"] for st in t["steps"]]
        record("daily.file.found", t["status"] == "done" and "find_files" in tools and "quarterly-report" in t["result"], status=t["status"], tools=tools, result=t["result"][:160])
        record("daily.file.typed_view", (task(t["id"]).get("view") or {}).get("type") == "files")
        time.sleep(1.0); shot("27_card_files")
        show_task_overview(t["id"], "28_overview_files")
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_daily_new():
    """"Find me something new to do this weekend": events or web research for the home city,
    never a question back about where."""
    try:
        daily_profile(); fresh_session(settle=3.0)
        t, ans = daily_request("Find me something new to do this weekend.", "new", wait_s=300)
        if t is None: return
        tools = [st["tool"] for st in t["steps"]]
        text = (t["result"] + " " + " ".join(st["summary"] for st in t["steps"])).lower()
        record("daily.new.suggests_for_home", t["status"] == "done" and ("events_find" in tools or "research" in tools) and ("vienna" in text or "wien" in text), status=t["status"], tools=tools, result=t["result"][:160])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

def t_daily_music():
    """"Play some jazz" starts a YouTube playlist (a mix at least), "stop the music" pauses what
    Sam started and says where it was playing; dry under SAM_NO_REAL_APPS (nothing opens)."""
    try:
        daily_profile(); fresh_session(settle=3.0)
        t, ans = daily_request("Play some jazz.", "music", wait_s=90, want_kind="music")
        if t is None: return
        tools = [st["tool"] for st in t["steps"]]
        record("daily.music.playlist_starts", t["status"] == "done" and "music_play" in tools and "youtube.com/watch" in t["result"] and "list=" in t["result"], status=t["status"], result=t["result"][:160])
        full = task(t["id"])
        record("daily.music.typed_view", (full.get("view") or {}).get("type") == "music" and (full.get("view") or {}).get("service") == "youtube", view=full.get("view"))
        time.sleep(1.0); shot("25_card_music")
        show_task_overview(t["id"], "26_overview_music")
        t2, ans2 = daily_request("Stop the music.", "stop", wait_s=90, want_kind="music")
        if t2 is None: return
        r = t2["result"].lower()
        record("daily.music.stop_knows_source", t2["status"] == "done" and "paused" in r and "youtube" in r and "pause it there" not in r, status=t2["status"], result=t2["result"][:160], answer=ans2[:120])
    finally:
        trigger("close"); wait(lambda s: not s["connected"], 12)

TESTS = [("hover", t_hover), ("handsfree", t_handsfree), ("bargein", t_bargein), ("handsfree_close", t_handsfree_close), ("ptt", t_ptt), ("fn", t_fn), ("cards", t_cards), ("confirm", t_confirm), ("close", t_close), ("point", t_point), ("point_chat", t_point_chat), ("point_doc", t_point_doc), ("point_web", t_point_web), ("voice", t_voice), ("dictation", t_dictation), ("tasks_ui", t_tasks_ui), ("connectors", t_connectors), ("tasks_delete", t_tasks_delete), ("announce", t_announce), ("complex_plan", t_complex_plan), ("complex_automation", t_complex_automation), ("complex_knowledge", t_complex_knowledge), ("logins", t_logins), ("voiceoff", t_voiceoff), ("music", t_music), ("storage", t_storage), ("uninstall_plan", t_uninstall_plan), ("announce_queue", t_announce_queue), ("continuation", t_continuation), ("results_question", t_results_question), ("titles", t_titles), ("split_requests", t_split_requests), ("followup_task", t_followup_task), ("daily_place", t_daily_place), ("daily_info", t_daily_info), ("daily_file", t_daily_file), ("daily_new", t_daily_new), ("daily_music", t_daily_music), ("you", t_you), ("threads", t_threads), ("checkin", t_checkin), ("offer_yes", t_offer_yes), ("secrets", t_secrets), ("history", t_history), ("report", t_report), ("onboarding", t_onboarding), ("computer", t_computer), ("models", t_models)]

if __name__ == "__main__":
    pick = sys.argv[1:]
    start_stack()
    try:
        for name, fn in TESTS:
            if pick and name not in pick: continue
            try: fn()
            except Exception as e: record(name, False, error=repr(e)[:200])
    finally:
        stop_stack()
    ok = sum(r["ok"] for r in RESULTS)
    rec = dict(ts=time.strftime("%Y-%m-%dT%H:%M:%S"), passed=f"{ok}/{len(RESULTS)}", results=RESULTS)
    open(LOG, "a").write(json.dumps(rec) + "\n"); print("PASSED", rec["passed"])
