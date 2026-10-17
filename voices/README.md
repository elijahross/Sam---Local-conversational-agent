# Voices

Each voice is a reference clip that the Qwen3-TTS **Base** model clones (in-context learning). Every reply is conditioned on the same clip, so the voice stays the same across sentences and messages.

```
voices/<id>/ref.wav       10–18 s, one speaker, no music; 24 kHz mono (other formats are converted on load)
voices/<id>/ref.txt       the exact transcript of ref.wav
voices/<id>/voice.json    output gain, so every voice plays at the same level
voices/<id>/fillers/      short verified filler takes ("Let me think,") + fillers.json
```

| id | Character | Reference |
|---|---|---|
| `sam-male` | Sam (male) | built from Sam's own takes that a human listener approved, 15.8 s |
| `sam-female` | Sam (female) | a "golden" reference: Sam's own cleanest takes, 17.4 s |

Both voices started from synthetic (not real-person) source clips. The current references are Sam's own best takes cloned from those clips: cleaner than the source, and steadier across replies.

**To add a voice:**
1. Create `voices/<id>/` with a clean recording (`ref.wav`) and its exact transcript (`ref.txt`). Expressiveness follows the reference: a calm clip gives a calm voice. Only clone a voice you have the right to use; never clone a real person without their consent.
2. Run `.venv/bin/python tools/prepare_voice.py <id>` (about 1.5 min per voice). It normalizes the reference to −20 dBFS, measures the output gain into `voice.json`, and renders each filler 6 times, keeping the cleanest take (speaker match, UTMOS, transcript). Lone hums never clone cleanly and are left out.
3. Listen to a few replies. Automatic scores catch garbled takes, but only a human ear reliably catches a take that drifts into a different voice.
