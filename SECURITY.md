# Security policy

Sam hears you, remembers you, reads what you share from your screen and can act on your accounts, so security reports get priority.

## Reporting a vulnerability

Please **don't open a public issue**. Report it privately through [GitHub's security advisories](https://github.com/elijahross/TTS/security/advisories/new) with:
- what an attacker can do, and what they need (a web page you visit, a message you receive, local access, …);
- steps to reproduce, ideally against the dev stack (`./run.sh`);
- the Sam version or commit.

You'll get an answer within a week. Fixes ship as an app update (or a model manifest update), and the advisory is published once users have had time to update. Credit is given unless you'd rather not.

## What's in scope

- Bypassing an approval: a write that runs without the user's OK, or runs something other than what the card showed
- Prompt injection from pages, emails, chats or files that leads to data leaving the Mac or an unwanted action
- Secrets (Keychain items, API keys, passwords on screen) reaching a model, a log or a third party
- The local services (`127.0.0.1:47800–47804`) accepting requests from web pages or other users
- The update channels: accepting an unsigned or downgraded app update or model manifest
- The code sandbox: escaping the VM, network access, reading files outside the grant

Out of scope: attacks that need the user's unlocked account and admin rights, and model quality issues (bad answers) without a security impact.

## Built-in defences

See [docs/ARCHITECTURE.md › Security guardrails](docs/ARCHITECTURE.md#security-guardrails) and the regression suite `app/Tests/Security/attack_tests.py`.
