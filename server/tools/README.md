# Tools

One folder per tool:
- `tool.json`: `name`, a one-line `summary` (the only text the planner always sees), `kinds` (Laya kinds it serves), `effects` (`read` | `write`), `args` and `runner` (`builtin` or `mcp`).
- `instructions.md`: loaded only when the tool is chosen.

MCP servers are configured in `server/mcp.json`. Their tools are listed once, cached in `data/mcp/<server>.json` and merged into the same registry. Their full input schema loads only for the chosen tool.

A `write` tool (sends, submits, buys, deletes) never runs directly. It produces a pending action that the user must confirm.
