//! Minimal MCP client (stdio transport, JSON-RPC 2.0): initialize → tools/list → tools/call.
//!
//! Servers are configured in `server/mcp.json`:
//! `{"servers": {"time": {"command": "uvx", "args": ["mcp-server-time"], "env": {"X": "keychain:svc/account"}, "read_tools": []}}}`
//! - The child gets a minimal environment: PATH, HOME and only the env vars configured for it
//!   (values `keychain:<service>/<account>` are read via /usr/bin/security) — never our .env.
//! - Tool effects: write (needs the user's confirmation) unless listed in `read_tools`, or the
//!   server is marked `trust_hints` and declares `readOnlyHint: true` (annotations are untrusted hints).
//! - Every call has a timeout; results are capped before any model sees them.

use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const PROTOCOL: &str = "2025-06-18";

#[derive(Debug, Clone, Deserialize)]
pub struct ServerCfg {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Tools to treat as read-only even without a readOnlyHint.
    #[serde(default)]
    pub read_tools: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Trust the server's own readOnlyHint annotations (they are self-declared; off by default,
    /// so unlisted tools need the user's confirmation).
    #[serde(default)]
    pub trust_hints: bool,
    /// Per-tool approval override (always_allow | ask_once | ask_always | never).
    #[serde(default)]
    pub approval: HashMap<String, String>,
    /// Per-tool pinned arguments: merged over the model's args, hidden from its schema.
    #[serde(default)]
    pub pin: HashMap<String, Value>,
    /// Argument names hidden from the model for every tool of this server.
    #[serde(default)]
    pub hide: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub servers: HashMap<String, ServerCfg>,
}

impl Config {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
}

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    pub async fn start(name: &str, cfg: &ServerCfg) -> anyhow::Result<Self> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            let val = match v.strip_prefix("keychain:") {
                Some(r) => {
                    let (svc, acct) = r.split_once('/').context("keychain:<service>/<account>")?;
                    crate::secrets::kc_get(svc, acct).with_context(|| format!("keychain item {svc}/{acct} for MCP server {name}"))?
                }
                None => v.clone(),
            };
            cmd.env(k, val);
        }
        let mut child = cmd.spawn().with_context(|| format!("start MCP server {name}"))?;
        let stdin = child.stdin.take().context("stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("stdout")?);
        let mut c = Self { child, stdin, stdout, next: 1 };
        c.request("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "sam", "version": "0.1"}}), Duration::from_secs(60)).await?;
        c.notify("notifications/initialized").await?;
        Ok(c)
    }

    async fn notify(&mut self, method: &str) -> anyhow::Result<()> {
        let m = json!({"jsonrpc": "2.0", "method": method}).to_string() + "\n";
        self.stdin.write_all(m.as_bytes()).await?;
        Ok(self.stdin.flush().await?)
    }

    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let id = self.next;
        self.next += 1;
        let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string() + "\n";
        self.stdin.write_all(m.as_bytes()).await?;
        self.stdin.flush().await?;
        let read = async {
            let mut line = String::new();
            loop {
                line.clear();
                if self.stdout.read_line(&mut line).await? == 0 {
                    bail!("MCP server closed the connection");
                }
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                if v["id"].as_u64() == Some(id) {
                    if let Some(e) = v.get("error") {
                        bail!("{method}: {e}");
                    }
                    return Ok(v["result"].clone());
                }
                // Server-initiated requests (sampling, roots…) aren't supported: answer with an error.
                if v.get("method").is_some() && v.get("id").is_some() {
                    let r = json!({"jsonrpc": "2.0", "id": v["id"], "error": {"code": -32601, "message": "not supported by this client"}}).to_string() + "\n";
                    self.stdin.write_all(r.as_bytes()).await?;
                    self.stdin.flush().await?;
                }
            }
        };
        tokio::time::timeout(timeout, read).await.with_context(|| format!("{method} timed out"))?
    }

    pub async fn list_tools(&mut self) -> anyhow::Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = cursor.as_ref().map(|c| json!({"cursor": c})).unwrap_or(json!({}));
            let r = self.request("tools/list", params, Duration::from_secs(30)).await?;
            out.extend(r["tools"].as_array().cloned().unwrap_or_default());
            cursor = r["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Returns (text, is_error). Text parts are joined; non-text parts are summarized.
    pub async fn call(&mut self, tool: &str, args: Value, timeout: Duration) -> anyhow::Result<(String, bool)> {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}), timeout).await?;
        let mut parts = Vec::new();
        for c in r["content"].as_array().into_iter().flatten() {
            match c["type"].as_str() {
                Some("text") => parts.push(c["text"].as_str().unwrap_or("").to_string()),
                Some(t) => parts.push(format!("[{t} content omitted]")),
                None => {}
            }
        }
        if parts.is_empty() {
            if let Some(s) = r.get("structuredContent") {
                parts.push(s.to_string());
            }
        }
        Ok((parts.join("\n"), r["isError"].as_bool().unwrap_or(false)))
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}
