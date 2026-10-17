"""Checks release/manifest.json against the rules the app and server apply (server/src/manifest.rs,
app/Sam/Sources/Updates.swift), so a manifest that would be silently ignored fails CI instead.

Usage: python3 tools/check_manifest.py [manifest.json] [--base previous.json]
  --base: the manifest on the target branch; a changed manifest must raise `version`.
"""
import json, re, sys

SETTINGS = {"MINIMAX_MODEL", "MINIMAX_LIGHT_MODEL", "MINIMAX_VOICE_MODEL", "MINIMAX_REASONING",
            "OPENAI_MODEL", "OPENAI_REASONING", "GEMINI_MODEL", "GEMINI_THINKING"}
HF_KEYS = {"tts", "tts_steady", "tts_tiny", "embed", "judge", "turn"}
PLAIN = re.compile(r"^[A-Za-z0-9._:/-]{1,100}$")
REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")


def check(m):
    errs = []
    if not isinstance(m.get("version"), int) or m["version"] < 1:
        errs.append("version: a positive integer is required")
    if not isinstance(m.get("min_app_build", 0), int):
        errs.append("min_app_build: integer")
    for k, v in (m.get("settings") or {}).items():
        if k not in SETTINGS:
            errs.append(f"settings.{k}: not an allowed setting (allowed: {', '.join(sorted(SETTINGS))})")
        elif not isinstance(v, str) or not PLAIN.match(v):
            errs.append(f"settings.{k}: plain model id expected, got {v!r}")
    for k, v in (m.get("models") or {}).items():
        if not (isinstance(v, list) and len(v) == 3 and all(isinstance(t, str) and PLAIN.match(t) for t in v)):
            errs.append(f"models.{k}: three plain ids [light, standard, heavy] expected")
    for k, v in (m.get("replace") or {}).items():
        if not PLAIN.match(k.rstrip("*")) or not isinstance(v, str) or not PLAIN.match(v):
            errs.append(f"replace.{k}: plain id (or prefix*) → plain id or \"default\"")
    for k, v in (m.get("hf") or {}).items():
        if k not in HF_KEYS:
            errs.append(f"hf.{k}: unknown key (known: {', '.join(sorted(HF_KEYS))})")
        elif not isinstance(v, str) or not REPO.match(v):
            errs.append(f"hf.{k}: 'owner/name' expected, got {v!r}")
    if m.get("message") is not None and (not isinstance(m["message"], str) or len(m["message"]) > 200):
        errs.append("message: null or a string of at most 200 characters")
    return errs


def main(argv):
    path = next((a for a in argv if not a.startswith("--") and argv[argv.index(a) - 1] != "--base"), "release/manifest.json")
    m = json.load(open(path))
    errs = check(m)
    if "--base" in argv:
        base = json.load(open(argv[argv.index("--base") + 1]))
        if base != m and m.get("version", 0) <= base.get("version", 0):
            errs.append(f"version: the manifest changed, so raise version above {base.get('version')} (installed apps ignore versions they have)")
    for e in errs:
        print(f"::error file={path}::{e}")
    print(f"{path}: {'ok' if not errs else f'{len(errs)} problem(s)'} (version {m.get('version')})")
    return 1 if errs else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
