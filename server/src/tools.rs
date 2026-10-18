//! Tool registry with progressive disclosure (skills pattern):
//! - every tool has a one-line `summary`; the planner only ever sees the summaries of the top-k
//!   tools for the request (Laya kind + word overlap + embedding similarity);
//! - a tool's `instructions.md` / MCP input schema is loaded only once it's chosen.
//! Built-in tools: `server/tools/<name>/tool.json`. MCP tools: `server/mcp.json`, listed once
//! and cached in `data/mcp/<server>.json` (refresh: delete the cache file).
//!
//! `effects: "write"` tools never run directly: they become a pending action the user must
//! confirm (see `Pending`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mcp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    /// "read" | "write"
    #[serde(default = "read")]
    pub effects: String,
    #[serde(default)]
    pub args: Value,
    /// "builtin" | "mcp"
    #[serde(default = "builtin")]
    pub runner: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub mcp_tool: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// "always_allow" | "ask_once" | "ask_always" | "never" (default: read → always_allow,
    /// write → ask_always). Vault: "Tri-State Tool Result with Input-Required Interrupt".
    #[serde(default)]
    pub approval: Option<String>,
    /// Set at load time: why the tool can't run right now ("not set up: …").
    #[serde(skip)]
    pub unavailable: Option<String>,
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

impl ToolSpec {
    pub fn approval(&self) -> &str {
        self.approval.as_deref().unwrap_or(if self.effects == "write" { "ask_always" } else { "always_allow" })
    }
}

fn read() -> String {
    "read".into()
}
fn builtin() -> String {
    "builtin".into()
}

pub struct Registry {
    pub tools: Vec<ToolSpec>,
    pub mcp: mcp::Config,
}

impl Registry {
    /// Built-in tool folders + MCP tools (cached listing; servers are started only to list once).
    pub async fn load(tools_dir: &Path, mcp_cfg: &Path, cache_dir: &Path) -> Self {
        let mut tools = Vec::new();
        if let Ok(rd) = std::fs::read_dir(tools_dir) {
            for e in rd.flatten() {
                let p = e.path().join("tool.json");
                if let Some(mut t) = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<ToolSpec>(&s).ok()) {
                    t.dir = Some(e.path());
                    tools.push(t);
                }
            }
        }
        let cfg = mcp::Config::load(mcp_cfg);
        for (name, sc) in cfg.servers.iter().filter(|(_, s)| s.enabled) {
            let cache = cache_dir.join(format!("{name}.json"));
            let listed: Option<Vec<Value>> = match std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                Some(v) => Some(v),
                None => match list_server(name, sc).await {
                    Ok(v) => {
                        let _ = std::fs::create_dir_all(cache_dir);
                        let _ = std::fs::write(&cache, serde_json::to_string_pretty(&v).unwrap_or_default());
                        Some(v)
                    }
                    Err(e) => {
                        tracing::warn!(server = name, "MCP listing failed: {e:#}");
                        None
                    }
                },
            };
            for t in listed.into_iter().flatten() {
                let tool = t["name"].as_str().unwrap_or("").to_string();
                let ro = sc.read_tools.contains(&tool) || (sc.trust_hints && t["annotations"]["readOnlyHint"].as_bool() == Some(true));
                let mut schema = t["inputSchema"].clone();
                // Modifier stage 1 (vault: "Three-Stage Tool Modifier Pipeline"): pinned and
                // hidden args never reach the model's view of the schema.
                if let Some(props) = schema["properties"].as_object_mut() {
                    for k in sc.pin.get(&tool).and_then(|p| p.as_object()).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default().iter().chain(sc.hide.iter()) {
                        props.remove(k);
                    }
                }
                tools.push(ToolSpec {
                    approval: sc.approval.get(&tool).cloned(),
                    unavailable: None,
                    name: format!("{name}.{tool}"),
                    summary: first_sentence(t["description"].as_str().unwrap_or(&tool)),
                    kinds: Vec::new(),
                    effects: if ro { "read".into() } else { "write".into() },
                    args: arg_names(&schema),
                    runner: "mcp".into(),
                    server: Some(name.clone()),
                    mcp_tool: Some(tool),
                    input_schema: Some(schema),
                    dir: None,
                });
            }
        }
        // Destructive names (delete_*, remove_*, drop…) always need the user's OK: config can't
        // mark them read-only or pre-approved.
        for t in tools.iter_mut().filter(|t| t.runner == "mcp" && destructive_name(&t.name)) {
            t.effects = "write".into();
            t.approval = Some("ask_always".into());
        }
        // Connection status (vault: "Meta-Tool Session…": search results carry availability).
        let email_ready = crate::email::Account::load().is_some();
        for t in tools.iter_mut() {
            // Sending also works through Mail.app; searching/reading needs the IMAP account.
            let mail_app = t.name == "email_send" && crate::apps::mail_app_ready();
            if t.name.starts_with("email_") && !email_ready && !mail_app {
                t.unavailable = Some("not set up: run `agent --email-setup`".into());
            }
            if let Some(c) = crate::connectors::disabled_for(&t.name) {
                t.unavailable = Some(format!("turned off in Connectors ({c})"));
            }
            if t.name == "recall_memory" && std::env::var("MEMORY").is_ok_and(|v| v == "0") {
                t.unavailable = Some("memory is off".into());
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Self { tools, mcp: cfg }
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Generic tools every plan may need; the routing lab (L8) showed keyword-rich specialised
    /// tools pushing them out of the top-k ("What does a new Leica Q3 cost?" never saw research).
    pub const CORE: [&'static str; 5] = ["research", "scrape", "web_fetch", "web_task", "browse"];

    /// The core tools (those present) plus the `k` best of the rest.
    pub fn select_with_core(&self, request: &str, kind: Option<&str>, sims: Option<&[f32]>, k: usize) -> Vec<&ToolSpec> {
        let mut out: Vec<&ToolSpec> = self.tools.iter().filter(|t| Self::CORE.contains(&t.name.as_str())).collect();
        for t in self.select(request, kind, sims, k + out.len()) {
            if out.len() >= k + Self::CORE.len() {
                break;
            }
            if !out.iter().any(|x| x.name == t.name) {
                out.push(t);
            }
        }
        out
    }

    /// Top-k tools for a request. `sims` = optional embedding similarity per tool (same order
    /// as `self.tools`), computed by the caller with the mind sidecar.
    pub fn select(&self, request: &str, kind: Option<&str>, sims: Option<&[f32]>, k: usize) -> Vec<&ToolSpec> {
        // The user's own words only: pointed-at window text would flood the overlap.
        let own = request.split("\n\nContext").next().unwrap_or(request);
        let words: Vec<String> = own.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_string).collect();
        let mut scored: Vec<(f32, &ToolSpec)> = self
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let text = format!("{} {}", t.name, t.summary).to_lowercase();
                let overlap = words.iter().filter(|w| text.contains(w.as_str())).count() as f32;
                let kind_hit = kind.is_some_and(|k| t.kinds.iter().any(|x| x == k)) as u8 as f32;
                let sim = sims.and_then(|s| s.get(i)).copied().unwrap_or(0.0);
                // A word naming the tool itself ("weather", "reminder", "message" → chat) counts most.
                let stem = t.name.split('_').next().unwrap_or(&t.name);
                let named = words.iter().any(|w| w.starts_with(stem) || (stem == "chat" && ["message", "nachricht", "whatsapp", "telegram", "slack", "text"].contains(&w.as_str()))) as u8 as f32;
                (2.0 * kind_hit + 0.5 * overlap + 4.0 * sim + 3.0 * named, t)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(k).map(|(_, t)| t).collect()
    }

    /// The planner's view: one line per tool.
    pub fn one_liners(tools: &[&ToolSpec]) -> String {
        tools
            .iter()
            .map(|t| {
                let args = t.args.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
                let gate = match (t.effects.as_str(), t.approval()) {
                    (_, "never") => " [disabled]",
                    ("write", "always_allow") => "",
                    ("write", _) => " [needs user confirmation]",
                    _ => "",
                };
                let na = t.unavailable.as_ref().map(|u| format!(" [UNAVAILABLE: {u}]")).unwrap_or_default();
                format!("- {}({args}){gate}{na}: {}", t.name, t.summary)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full detail for a chosen tool: instructions.md and/or the MCP input schema.
    pub fn details(&self, t: &ToolSpec) -> String {
        let ins = t.dir.as_ref().and_then(|d| std::fs::read_to_string(d.join("instructions.md")).ok()).unwrap_or_default();
        let schema = t.input_schema.as_ref().map(|s| format!("\nInput schema: {s}")).unwrap_or_default();
        let args = if t.input_schema.is_none() { format!("\nArgs: {}", t.args) } else { String::new() };
        format!("Tool {}: {}\n{ins}{args}{schema}", t.name, t.summary)
    }
}

pub async fn list_server(name: &str, sc: &mcp::ServerCfg) -> anyhow::Result<Vec<Value>> {
    let mut c = mcp::Client::start(name, sc).await?;
    let r = c.list_tools().await;
    c.shutdown().await;
    r
}

fn first_sentence(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len()).min(160);
    s.chars().take(end).collect()
}

fn arg_names(schema: &Value) -> Value {
    let req: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    json!(schema["properties"].as_object().map(|o| o.keys().map(|k| (k.clone(), json!(if req.contains(&k.as_str()) { "required" } else { "optional" }))).collect::<serde_json::Map<_, _>>()).unwrap_or_default())
}

/// Write actions waiting for the user's OK: `data/pending/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: String,
    pub tool: String,
    pub args: Value,
    pub request: String,
    pub summary: String,
    pub created: i64,
}

pub struct Pending {
    dir: PathBuf,
}

impl Pending {
    pub fn new(data: &Path) -> Self {
        Self { dir: data.join("pending") }
    }

    pub fn add(&self, tool: &str, args: &Value, request: &str, summary: &str) -> anyhow::Result<PendingAction> {
        std::fs::create_dir_all(&self.dir)?;
        // Random 5-hex id with at least one digit (so spoken / typed ids are recognized), never
        // reusing a waiting action's file.
        let id = loop {
            let id = format!("{:05x}", rand::random::<u32>() % 0xfffff);
            if id.chars().any(|c| c.is_ascii_digit()) && !self.dir.join(format!("{id}.json")).exists() {
                break id;
            }
        };
        let a = PendingAction { id: id.clone(), tool: tool.into(), args: args.clone(), request: request.into(), summary: summary.into(), created: chrono::Local::now().timestamp() };
        std::fs::write(self.dir.join(format!("{id}.json")), serde_json::to_string_pretty(&a)?)?;
        Ok(a)
    }

    /// Open actions from the last hour (older ones expire).
    pub fn open(&self) -> Vec<PendingAction> {
        let now = chrono::Local::now().timestamp();
        let mut v: Vec<PendingAction> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str(&s).ok()))
            .filter(|a: &PendingAction| now - a.created < 3600)
            .collect();
        v.sort_by_key(|a| a.created);
        v
    }

    pub fn get(&self, id: &str) -> Option<PendingAction> {
        if !valid_action_id(id) {
            return None;
        }
        std::fs::read_to_string(self.dir.join(format!("{id}.json"))).ok().and_then(|s| serde_json::from_str(&s).ok())
    }

    /// The island's confirmation card let the user edit fields: merge them into the drafted
    /// args (only keys that already exist; the user authored the values).
    pub fn update_args(&self, id: &str, edits: &Value) {
        let Some(mut a) = self.get(id) else { return };
        // Only what the card shows as editable: never a chat's verified recipient, a parked
        // page, a send route or the like (those would bypass the checks they were drafted with).
        let editable: &[&str] = match a.tool.as_str() {
            "chat_send" => &["text"],
            "web_task" => &[],
            "email_send" => &["to", "cc", "subject", "body"],
            _ => &["title", "text", "start", "end", "due", "location", "notes", "path"],
        };
        if let (Some(dst), Some(src)) = (a.args.as_object_mut(), edits.as_object()) {
            for (k, v) in src {
                if dst.contains_key(k) && editable.contains(&k.as_str()) {
                    dst.insert(k.clone(), v.clone());
                }
            }
        }
        let _ = std::fs::write(self.dir.join(format!("{id}.json")), serde_json::to_string_pretty(&a).unwrap_or_default());
    }

    pub fn take(&self, id: &str) -> Option<PendingAction> {
        let p = self.dir.join(format!("{id}.json"));
        let a = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok());
        let _ = std::fs::remove_file(p);
        a
    }
}

/// Deterministic gate for executing a pending write: the user's own words must confirm.
/// Phrases match as phrases, single words only as whole words; any negation blocks.
pub fn is_confirmation(text: &str) -> bool {
    let t = format!(" {} ", text.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '\'' && c != 'ä', " "));
    let has = |p: &str| t.contains(&format!(" {p} "));
    let neg = ["don't", "do not", "not yet", "cancel", "stop", "no", "nein", "nicht", "wait"];
    let pos = ["yes", "yeah", "yep", "confirm", "confirmed", "go ahead", "send it", "do it", "approved", "ja", "bestätigt", "bestätige", "passt", "mach es"];
    !neg.iter().any(|n| has(n)) && pos.iter().any(|p| has(p))
}

/// Action ids named in the user's words ("yes, go ahead 3fa2c"): 4–6 hex characters that
/// contain a digit (so plain words like "face" or "added" never count as ids).
pub fn action_ids(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| (4..=6).contains(&w.len()) && w.chars().all(|c| c.is_ascii_hexdigit()) && w.chars().any(|c| c.is_ascii_digit())).map(|w| w.to_lowercase()).collect()
}

/// A valid pending-action id (hex): anything else is never used as a file name.
pub fn valid_action_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 8 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// Actions that spend money (orders, payments, bookings): only a click on the card confirms them.
pub fn high_risk(a: &PendingAction) -> bool {
    if a.tool != "web_task" {
        return false;
    }
    let i = &a.args["info"];
    let label = i["label"].as_str().unwrap_or("").to_lowercase();
    i["total"].as_str().is_some_and(|t| !t.is_empty())
        || ["order", "buy", "pay", "purchase", "book", "checkout", "bestell", "kauf", "bezahl", "buchen", "zahlungspflichtig"].iter().any(|w| label.contains(w))
}

/// Destructive tool names (delete, remove…): such tools always ask, whatever their config says.
pub fn destructive_name(name: &str) -> bool {
    let n = name.to_lowercase();
    ["delete", "remove", "destroy", "drop", "purge", "erase", "wipe", "truncate", "archive", "trash", "unsubscribe", "revoke", "cancel", "close_account", "deactivate", "reset", "overwrite", "force_push", "kill"]
        .iter().any(|w| n.contains(w))
}

/// "No / cancel / don't" in a short reply: drop the pending action.
pub fn is_cancellation(text: &str) -> bool {
    let t = format!(" {} ", text.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '\'', " "));
    text.split_whitespace().count() <= 8 && ["no", "don't", "cancel", "stop", "nein", "abbrechen", "never mind"].iter().any(|p| t.contains(&format!(" {p} ")))
}

#[cfg(test)]
mod tests {
    use super::is_confirmation;

    #[test]
    fn confirmation_gate() {
        for y in ["Yes, send it", "go ahead", "confirm", "ja, passt", "Yep do it."] {
            assert!(is_confirmation(y), "{y}");
        }
        for n in ["no, don't send it", "not yet", "what's in the jacket?", "machine learning", "yes but wait", "cancel that", "send it later? no"] {
            assert!(!is_confirmation(n), "{n}");
        }
    }
}

/// Executed write actions (vault: "Never-Retry Tool Execution", "Idempotent Action Ledger"):
/// append-only `data/actions.jsonl`, also the audit log. A pending action id runs at most once.
pub struct Ledger {
    path: PathBuf,
    approvals: PathBuf,
}

impl Ledger {
    pub fn new(data: &Path) -> Self {
        Self { path: data.join("actions.jsonl"), approvals: data.join("approvals.json") }
    }

    fn append(&self, v: Value) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{v}");
        }
    }

    /// True if this action id already started (done, failed after sending, or unknown).
    pub fn seen(&self, action_id: &str) -> bool {
        std::fs::read_to_string(&self.path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|v| v["action"] == action_id)
    }

    pub fn start(&self, action_id: &str, tool: &str, args: &Value) {
        self.append(json!({"ts": chrono::Local::now().to_rfc3339(), "action": action_id, "tool": tool, "args": crate::secrets::redact(&args.to_string()), "status": "started"}));
    }

    pub fn finish(&self, action_id: &str, status: &str, detail: &str) {
        self.append(json!({"ts": chrono::Local::now().to_rfc3339(), "action": action_id, "status": status, "detail": crate::secrets::redact(detail)}));
    }

    /// ask_once: approval remembered per tool + target (e.g. the same recipients).
    /// Remembered for 7 days, per exact target; destructive tools are never remembered.
    pub fn approved(&self, key: &str) -> bool {
        let now = chrono::Local::now().timestamp();
        !destructive_name(key) && std::fs::read_to_string(&self.approvals).ok().and_then(|s| serde_json::from_str::<Vec<(String, i64)>>(&s).ok())
            .is_some_and(|v| v.iter().any(|(k, at)| k == key && now - at < 7 * 86_400))
    }

    pub fn remember(&self, key: &str) {
        let now = chrono::Local::now().timestamp();
        let mut v: Vec<(String, i64)> = std::fs::read_to_string(&self.approvals).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        v.retain(|(k, at)| k != key && now - at < 7 * 86_400);
        v.push((key.to_string(), now));
        let _ = std::fs::write(&self.approvals, serde_json::to_string_pretty(&v).unwrap_or_default());
    }
}

/// The ask_once key for a write: tool + its target (recipients for mail, else the tool).
pub fn approval_key(tool: &str, args: &Value) -> String {
    let mut to: Vec<String> = ["to", "cc"].iter().flat_map(|k| args[*k].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_str().map(|s| s.to_lowercase())).collect();
    to.sort();
    // Without recipients the key covers the exact arguments (not "every call of this tool").
    if to.is_empty() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&args.to_string(), &mut h);
        format!("{tool}#{:x}", std::hash::Hasher::finish(&h))
    } else {
        format!("{tool}:{}", to.join(","))
    }
}