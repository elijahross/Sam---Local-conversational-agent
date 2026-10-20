# Releases and updates

Sam updates itself in two layers. Both are signed with one Ed25519 key: the public half is in
[`update-public-key.txt`](update-public-key.txt) (built into every app), the private half lives
only in the release maker's login Keychain.

| Layer | What changes | How users get it |
|---|---|---|
| **App update** | Code, sidecars, voices, Python packages | Sparkle checks `appcast.xml` daily; *Check for Updates…* in the menu-bar drop-down. A changed `app/runtime/requirements.lock` makes the app reinstall its runtime on the next start. |
| **Model manifest** | Model ids per provider, retired models, Hugging Face repos | The app checks `manifest.json` at launch and daily, verifies the signature, downloads any new local model and restarts its services. No reinstall. |

Both files are fetched from the **latest GitHub release** of `elijahross/Sam---Local-conversational-agent`
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
2. `app/release.sh` builds and signs everything into `dist/release/`: `Sam-<version>-update.zip` (the
   Sparkle update), `Sam-<version>.dmg` (first installs), `appcast.xml`, `manifest.json` + `.sig`.
3. `app/release.sh --publish` creates the GitHub release `v<version>` (or `$TAG`) with those files, marked as latest.
   Don't mark it as a pre-release: `releases/latest` skips pre-releases, so installed apps would find no updates.

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

## Signing and notarization

`app/package.sh` (and so `app/release.sh`) signs with a **Developer ID Application** certificate
when the keychain has one (or `SIGN_ID=...`): hardened runtime, secure timestamp and entitlements
(`app/entitlements/`: microphone + Apple Events for the app; Apple Events for the server and the
computer-use helper). It then notarizes and staples the app and the dmg, and checks both with
`spctl`. Without the certificate it falls back to the local identity
(`sidecar/computer/signing.sh`), with no notarization: fine for your own Mac only.

One-time setup on the release Mac:
1. Xcode › Settings › Accounts › your team › Manage Certificates › + › **Developer ID Application**
   (the Account Holder can create it). `security find-identity -v -p codesigning` then lists it.
2. Create an app-specific password at appleid.apple.com, then store the notarization profile:
   `xcrun notarytool store-credentials sam-notary --apple-id <your Apple ID> --team-id <TEAM ID>`
   (other profile names: `NOTARY_PROFILE=...`).

The first Developer-ID build has a new code signature, so macOS asks again **once** for microphone,
Accessibility and the other grants. Sparkle accepts the change because updates are trusted
through the EdDSA key, not the code signature.
