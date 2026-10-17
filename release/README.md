# Releases and updates

Sam updates itself in two layers. Both are signed with one Ed25519 key: the public half is in
[`update-public-key.txt`](update-public-key.txt) (built into every app), the private half lives
only in the release maker's login Keychain.

| Layer | What changes | How users get it |
|---|---|---|
| **App update** | Code, sidecars, voices, Python packages | Sparkle checks `appcast.xml` daily; *Check for Updates…* in the menu-bar drop-down. A changed `app/runtime/requirements.lock` makes the app reinstall its runtime on the next start. |
| **Model manifest** | Model ids per provider, retired models, Hugging Face repos | The app checks `manifest.json` at launch and daily, verifies the signature, downloads any new local model and restarts its services. No reinstall. |

Both files are fetched from the **latest GitHub release** of `elijahross/TTS`
(`releases/latest/download/…`), so the repository has to be public for other users.

## Model manifest ([`manifest.json`](manifest.json))

| Field | Meaning |
|---|---|
| `version` | Must increase with every change; apps ignore a version they already have. |
| `min_app_build` | Apps with a lower `CFBundleVersion` keep their manifest and say an update is needed. |
| `settings` | Model ids and reasoning levels only: `MINIMAX_MODEL`, `MINIMAX_LIGHT_MODEL`, `MINIMAX_VOICE_MODEL`, `MINIMAX_REASONING`, `OPENAI_MODEL`, `OPENAI_REASONING`, `GEMINI_MODEL`, `GEMINI_THINKING`. Anything else is ignored. |
| `models` | Per-provider tiers `[light, standard, heavy]`. |
| `replace` | Retired id (or `prefix*`) → successor, or `"default"` for the tier's current default. Also rewrites the user's saved choice. |
| `hf` | Local model repos: `tts`, `tts_steady`, `tts_tiny`, `embed`, `judge`, `turn`. |
| `message` | Optional line shown in the menu-bar drop-down. |

Precedence on a Mac: environment variables > manifest > compiled defaults. Endpoints, keys and
safety settings are never in the manifest; they ship with app updates.

**A provider retires a model:** edit `manifest.json` (new id in `settings`/`models`, old id in
`replace`), bump `version`, then `app/release.sh --manifest --publish`.

## App release

1. Bump `VERSION` and `BUILD` in [`version.env`](version.env) (`BUILD` must increase; Sparkle compares it).
2. `app/release.sh` builds and signs everything into `dist/release/`: `Sam-<version>.zip` (the
   update), `Sam-<version>.dmg` (first installs), `appcast.xml`, `manifest.json` + `.sig`.
3. `app/release.sh --publish` creates the GitHub release `v<version>` with those files.

## The signing key

- Created once with `app/vendor/Sparkle/bin/generate_keys` (stored in the login Keychain).
- **Back it up:** `app/vendor/Sparkle/bin/generate_keys -x sam-update-key.txt`, then keep that file
  offline (a password manager). Without the key, installed apps can never be updated again;
  anyone with it can ship updates to every Sam.
- Restore on a new Mac: `generate_keys -f sam-update-key.txt`.

## Testing an update locally

1. `app/package.sh`, then copy `dist/Sam.app` somewhere temporary.
2. In the copy's `Contents/Info.plist`, point `SamManifestURL` and `SUFeedURL` at `http://127.0.0.1:47890/…`, and re-sign it: `codesign --force --sign "<Sam Local Code Signing id>" --identifier one.sam.app <copy>`.
3. Serve a folder with `manifest.json` (a higher `version`), `manifest.json.sig` (`app/vendor/Sparkle/bin/sign_update -p manifest.json`) and an `appcast.xml`: `python3 -m http.server 47890 --bind 127.0.0.1`.
4. Open the copy (with your own Sam closed). Expect `data/manifest.json` to be replaced and `model manifest loaded version=N` in `~/Library/Logs/Sam/server.log`. A manifest with a bad signature or a lower version must be ignored.
5. Clean up: quit the copy and delete `~/Library/Application Support/Sam/data/manifest.json`, or installed apps ignore lower published versions.

## Caveats

- Builds are signed with the local identity (`sidecar/computer/signing.sh`). Updates keep macOS
  permissions only when made with the same identity. For distribution to other people, sign with
  a Developer ID and notarize.
