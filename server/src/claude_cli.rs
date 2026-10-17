//! Claude Code as a provider: Sam runs the `claude` CLI that is already signed in on this Mac, so
//! the user's own subscription answers instead of an API key. Personal use only: Anthropic's
//! terms allow a person to use the unmodified binary with their own login, not an app that routes
//! other people through it (a Sam handed to others needs API keys). Nothing here reads credentials
//! or the Keychain: the binary runs headless from an empty working directory with every tool,
//! hook, MCP server, skill and CLAUDE.md switched off, one turn per call.

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::{AppState, llm::{Msg, Usage}};

/// The CLI: `CLAUDE_BIN`, else `claude` on PATH, else the usual install locations.
pub fn find_bin() -> Option<PathBuf> {
    if let Ok(b) = std::env::var("CLAUDE_BIN") {
        return Some(PathBuf::from(b)).filter(|p| p.is_file());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = std::env::var("PATH").unwrap_or_default().split(':').map(|d| PathBuf::from(d).join("claude")).collect();
    candidates.extend([format!("{home}/.local/bin/claude"), "/opt/homebrew/bin/claude".into(), "/usr/local/bin/claude".into(), format!("{home}/.claude/local/claude")].map(PathBuf::from));
    candidates.into_iter().find(|p| p.is_file())
}

/// `claude auth status` → signed in? (Reads nothing itself: the CLI reports.)
pub async fn auth_ok(bin: &PathBuf) -> bool {
    let out = tokio::time::timeout(Duration::from_secs(15), Command::new(bin).args(["auth", "status"]).stdin(Stdio::null()).output()).await;
    match out {
        Ok(Ok(o)) => serde_json::from_slice::<Value>(&o.stdout).map(|v| v["loggedIn"].as_bool() == Some(true)).unwrap_or(false),
        _ => false,
    }
}

/// An empty directory the CLI runs in, so no project settings, hooks or MCP servers apply.
fn cwd() -> PathBuf {
    let d = crate::thinker::data_dir().join("claude-cwd");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn command(st: &AppState, model: &str, system: &str, output: &[&str]) -> anyhow::Result<Command> {
    let bin = st.cfg.claude_bin.as_ref().context("Claude Code isn't installed (onboarding › AI provider)")?;
    let mut c = Command::new(bin);
    c.args(["-p", "--safe-mode", "--tools", "", "--disallowedTools", "mcp__*", "--strict-mcp-config", "--no-session-persistence", "--max-turns", "1", "--model", model, "--system-prompt", system])
        .args(output)
        .current_dir(cwd())
        // The choice "Claude Code" means the login, never an API key that happens to be in the environment.
        .env_remove("ANTHROPIC_API_KEY")
        // Only what the CLI needs to run and find its own login: none of Sam's other provider
        // keys or tokens (2026-10-15 audit).
        .env_clear()
        .envs(["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "TERM"].iter().filter_map(|k| std::env::var(k).ok().map(|v| (*k, v))))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(c)
}

/// One call has one prompt: the conversation is written out for it.
fn transcript(history: &[Msg]) -> String {
    if history.len() <= 1 {
        return history.first().map(|m| m.text.clone()).unwrap_or_default();
    }
    let mut s = String::from("Conversation so far (you are the assistant):\n\n");
    for m in history {
        s.push_str(if m.role == "model" { "Assistant: " } else { "User: " });
        s.push_str(&m.text);
        s.push('\n');
    }
    s.push_str("\nReply to the last user message only, as the assistant.");
    s
}

fn usage_of(v: &Value) -> Usage {
    let u = &v["usage"];
    Usage {
        prompt: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        completion: u["output_tokens"].as_u64().unwrap_or(0),
        cached: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
    }
}

/// Streams the reply's text deltas (stream-json with partial messages).
pub async fn stream(st: &AppState, model: &str, system: &str, history: &[Msg], mut on_delta: impl FnMut(&str) -> bool) -> anyhow::Result<String> {
    let mut child = command(st, model, system, &["--output-format", "stream-json", "--verbose", "--include-partial-messages"])?.spawn().context("spawn claude")?;
    let prompt = transcript(history);
    let mut stdin = child.stdin.take().context("claude stdin")?;
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let mut lines = BufReader::new(child.stdout.take().context("claude stdout")?).lines();
    let mut full = String::new();
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("stream_event") => {
                let e = &v["event"];
                if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                    let t = e["delta"]["text"].as_str().unwrap_or_default();
                    if !t.is_empty() {
                        full.push_str(t);
                        if !on_delta(t) {
                            return Ok(full);
                        }
                    }
                }
            }
            Some("result") => {
                if v["is_error"].as_bool() == Some(true) {
                    bail!("claude: {}", v["result"].as_str().unwrap_or("error"));
                }
                let u = usage_of(&v);
                tracing::info!(provider = "claude-code", prompt = u.prompt, cached = u.cached, hit = format!("{:.0}%", 100.0 * u.cached as f64 / u.prompt.max(1) as f64), "voice llm usage");
                // The deltas can be missing when the CLI falls back to a non-streaming reply.
                if full.is_empty() {
                    if let Some(r) = v["result"].as_str() {
                        full.push_str(r);
                        on_delta(r);
                    }
                }
                return Ok(full);
            }
            _ => {}
        }
    }
    let status = child.wait().await?;
    if !status.success() && full.is_empty() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut e, &mut err).await;
        }
        bail!("claude exited with {status}: {}", err.chars().take(300).collect::<String>());
    }
    Ok(full)
}

/// One non-streaming call (background work): the `result` of the JSON output.
pub async fn complete(st: &AppState, model: &str, system: &str, user: &str, timeout: Duration) -> anyhow::Result<(String, Usage)> {
    let mut child = command(st, model, system, &["--output-format", "json"])?.spawn().context("spawn claude")?;
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let prompt = user.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(o) => o?,
        Err(_) => bail!("claude timed out after {} s", timeout.as_secs()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = text.find('{').and_then(|i| serde_json::from_str(&text[i..]).ok())
        .with_context(|| format!("claude gave no JSON result (exit {}): {}", out.status, String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()))?;
    if v["is_error"].as_bool() == Some(true) {
        bail!("claude: {}", v["result"].as_str().unwrap_or("error"));
    }
    Ok((v["result"].as_str().unwrap_or_default().trim().to_string(), usage_of(&v)))
}
