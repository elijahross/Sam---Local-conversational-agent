<!-- Thanks for contributing! See CONTRIBUTING.md for what each section needs. Delete what doesn't apply. -->

## Why

<!-- The problem or opportunity, from the user's point of view. Who is affected, how often, and why this change is worth it. Link the issue: "Closes #123". -->

## What changes for the user

<!-- Functional change: what someone using Sam will notice (new behaviour, changed defaults, new settings or permissions). Screenshots or a short screen recording for UI changes; an audio clip for voice changes. "Nothing visible" is a fine answer for refactors. -->

## How it works

<!-- Technical change: the approach, the main files touched, alternatives you considered and why you didn't pick them. Call out anything that changes a protocol, a stored format (data/*.json, SQLite tables), the model manifest or the update path. -->

## Privacy, safety and cost

- [ ] No new data leaves the Mac, **or** I describe below what is sent, to whom, and how the user controls it
- [ ] Writes (send, post, order, delete, …) still go through a drafted action and the user's approval
- [ ] Secrets stay in the Keychain and pass `secrets::redact` before any model, log or speech sees them
- [ ] No new always-on cost (LLM calls per turn, background polling) **or** I measured it below

<!-- Details: -->

## How I tested it

<!-- Commands you ran and what you checked by hand. Be specific: "said 'remind me to call Anna at five' with hands-free on; the confirm card showed 17:00". -->

- [ ] `cd server && cargo test --release --lib`
- [ ] `./app/Sam/build.sh` (and the helpers, if touched)
- [ ] E2E scenarios: <!-- e.g. `.venv/bin/python app/Tests/E2E/e2e.py confirm cards` -->
- [ ] Security regression, if anything near approvals, local services or the browser changed: `.venv/bin/python app/Tests/Security/attack_tests.py`
- [ ] Manual check on a real Mac: <!-- what you did -->
- [ ] Model, prompt or voice change: before/after numbers (see CONTRIBUTING.md › Measuring)

## Docs

- [ ] README / docs/ARCHITECTURE.md / docs/BENCHMARKS.md updated, or not needed
- [ ] `release/manifest.json` version raised, if the manifest changed
