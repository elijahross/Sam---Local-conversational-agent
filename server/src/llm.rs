//! LLM clients for the provider the user chose in the onboarding: MiniMax (default), OpenAI and
//! Mistral (chat-completions SSE), Claude (Anthropic Messages API), Gemini (its own API), Ollama
//! (local models, native /api/chat) and Claude Code (the signed-in CLI, claude_cli.rs).
//! Every call names a task tier — light (scoring, mapping, checks), standard (planning,
//! answers, voice), heavy (long synthesis, code) — and gets that provider's cheapest capable
//! model for it (config.rs: data/llm.json, verified against the account when the key is saved).

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::AppState;

#[derive(Clone, Debug)]
pub struct Msg {
    pub role: &'static str, // "user" | "model"
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Provider {
    Gemini,
    MiniMax,
    OpenAI,
    Anthropic,
    Mistral,
    /// Local models through the Ollama daemon: nothing leaves the Mac, no key.
    Ollama,
    /// The `claude` CLI signed in on this Mac (the user's own subscription; personal use).
    ClaudeCode,
}

impl Provider {
    pub const ALL: [Provider; 7] = [Provider::MiniMax, Provider::OpenAI, Provider::Anthropic, Provider::Gemini, Provider::Mistral, Provider::Ollama, Provider::ClaudeCode];

    /// Cloud providers take an API key; Ollama and Claude Code don't.
    pub fn needs_key(self) -> bool {
        !matches!(self, Self::Ollama | Self::ClaudeCode)
    }

    /// "minimax" | "openai" | "gemini" | "anthropic" (claude) | "mistral" (settings / env values).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "minimax" => Some(Self::MiniMax),
            "openai" | "gpt" => Some(Self::OpenAI),
            "gemini" | "google" => Some(Self::Gemini),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "mistral" => Some(Self::Mistral),
            "ollama" | "local" => Some(Self::Ollama),
            "claude-code" | "claude_code" | "claudecode" => Some(Self::ClaudeCode),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::MiniMax => "minimax",
            Self::OpenAI => "openai",
            Self::Gemini => "gemini",
            Self::Anthropic => "anthropic",
            Self::Mistral => "mistral",
            Self::Ollama => "ollama",
            Self::ClaudeCode => "claude-code",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MiniMax => "MiniMax",
            Self::OpenAI => "OpenAI",
            Self::Gemini => "Google Gemini",
            Self::Anthropic => "Claude (Anthropic)",
            Self::Mistral => "Mistral",
            Self::Ollama => "Ollama (local models, free)",
            Self::ClaudeCode => "Claude Code (your subscription)",
        }
    }
}

/// Task tier → the provider's cheapest capable model for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tier {
    /// Scoring, classification, field mapping, rule checks, short JSON.
    Light = 0,
    /// Planning, answers, voice, drafting.
    Standard = 1,
    /// Long synthesis (research briefs), code.
    Heavy = 2,
}

const ANTHROPIC: &str = "https://api.anthropic.com/v1/messages";
const MISTRAL: &str = "https://api.mistral.ai/v1/chat/completions";

/// A chat-completions request (MiniMax and OpenAI speak the same protocol; the knobs differ).
fn chat_request(st: &AppState, provider: Provider, model: &str, messages: Vec<Value>, max_tokens: u32, temperature: f32, stream: bool, json_mode: bool) -> anyhow::Result<reqwest::RequestBuilder> {
    let cfg = &st.cfg;
    Ok(match provider {
        Provider::Mistral => {
            let mut b = json!({"model": model, "stream": stream, "max_tokens": max_tokens, "temperature": temperature, "messages": messages});
            if json_mode {
                b["response_format"] = json!({"type": "json_object"});
            }
            st.http.post(MISTRAL).bearer_auth(cfg.mistral_key.as_deref().context("no Mistral API key (onboarding › AI provider)")?).json(&b)
        }
        Provider::MiniMax => st
            .http
            .post("https://api.minimax.io/v1/chat/completions")
            .bearer_auth(cfg.minimax_key.as_deref().context("MINIMAX_API_KEY missing")?)
            // Thinking goes to `reasoning_details` instead of <think> tags in content.
            .json(&{
                let mut b = json!({"model": model, "stream": stream, "max_tokens": max_tokens, "temperature": temperature, "reasoning_split": true, "messages": messages});
                // M3.1 thinks at "max" effort unless told otherwise and can't switch thinking off;
                // low keeps a voice turn at ~0.5 s to first token (llm-cost lab).
                if model.contains("M3.1") {
                    b["reasoning_effort"] = json!(cfg.minimax_reasoning);
                }
                if stream {
                    b["stream_options"] = json!({"include_usage": true});
                }
                b
            }),
        Provider::OpenAI => {
            // GPT-5.x reasoning models take `max_completion_tokens` and only the default temperature.
            let mut body = json!({"model": model, "stream": stream, "max_completion_tokens": max_tokens, "messages": messages});
            // Only reasoning models (gpt-5.x / gpt-6.x / o-series) accept `reasoning_effort`.
            let m = model;
            if !cfg.openai_reasoning.is_empty() && (m.starts_with("gpt-5") || m.starts_with("gpt-6") || m.starts_with('o')) {
                body["reasoning_effort"] = json!(cfg.openai_reasoning);
            }
            if json_mode {
                // MiniMax has no JSON mode; `complete` extracts and repairs JSON for both.
                body["response_format"] = json!({"type": "json_object"});
            }
            if stream {
                body["stream_options"] = json!({"include_usage": true});
            }
            st.http
                .post(format!("{}/chat/completions", cfg.openai_url.trim_end_matches('/')))
                .bearer_auth(cfg.openai_key.as_deref().context("OPENAI_API_KEY missing")?)
                .json(&body)
        }
        Provider::Gemini | Provider::Anthropic | Provider::Ollama | Provider::ClaudeCode => unreachable!("own API"),
    })
}

/// Claude: the Messages API (system apart, max_tokens required, no JSON mode — `complete`
/// extracts and repairs JSON). Long stable system prompts are marked cacheable.
fn anthropic_request(st: &AppState, model: &str, system: &str, messages: Vec<Value>, max_tokens: u32, temperature: f32, stream: bool) -> anyhow::Result<reqwest::RequestBuilder> {
    let key = st.cfg.anthropic_key.as_deref().context("no Claude API key (onboarding › AI provider)")?;
    let sys = if system.len() > 4000 { json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]) } else { json!(system) };
    Ok(st.http.post(ANTHROPIC).header("x-api-key", key).header("anthropic-version", "2023-06-01")
        .json(&json!({"model": model, "max_tokens": max_tokens, "temperature": temperature, "system": sys, "messages": messages, "stream": stream})))
}

// ---------------------------------------------------------------------------------------------
// Ollama: the native /api/chat (NDJSON when streaming). The payload follows what the model can
// do (`/api/show` capabilities, cached): `think: false` only on thinking models (others reject
// the field), `format: "json"` for JSON answers, the context window and token budget in
// `options`, and `keep_alive` so the model stays loaded between turns.

/// Models the onboarding offers first: name → what to expect (sizes are the download).
pub const OLLAMA_RECOMMENDED: [(&str, &str); 4] = [
    ("qwen3.5:9b", "Qwen 3.5 9B — 7 GB, 201 languages, good all-rounder (24 GB Mac or more)"),
    ("qwen3.5:4b", "Qwen 3.5 4B — 4 GB, for 16 GB Macs; simpler answers"),
    ("gemma4:12b", "Gemma 4 12B — 8 GB, Google; strong writing (24 GB Mac or more)"),
    ("gemma4:e4b", "Gemma 4 E4B — 7 GB, Google; small and quick"),
];

static OLLAMA_THINKS: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String, bool>>> = std::sync::OnceLock::new();

/// Does this model take the `think` field? (`/api/show` capabilities, asked once per model.)
async fn ollama_thinks(st: &AppState, model: &str) -> bool {
    let cache = OLLAMA_THINKS.get_or_init(Default::default);
    if let Some(&t) = cache.lock().await.get(model) {
        return t;
    }
    let v: Value = match st.http.post(format!("{}/api/show", st.cfg.ollama_url.trim_end_matches('/'))).json(&json!({"model": model})).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("ollama show {model}: {e}");
            return false;
        }
    };
    let t = v["capabilities"].as_array().into_iter().flatten().any(|c| c == "thinking");
    cache.lock().await.insert(model.to_string(), t);
    t
}

/// Installed Ollama models (None: the daemon isn't reachable at `OLLAMA_URL`).
pub async fn ollama_models(st: &AppState) -> Option<Vec<String>> {
    let v: Value = st.http.get(format!("{}/api/tags", st.cfg.ollama_url.trim_end_matches('/'))).timeout(std::time::Duration::from_secs(2)).send().await.ok()?.json().await.ok()?;
    Some(v["models"].as_array()?.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
}

async fn ollama_request(st: &AppState, model: &str, system: &str, mut messages: Vec<Value>, max_tokens: u32, temperature: f32, stream: bool, json_mode: bool) -> reqwest::RequestBuilder {
    let cfg = &st.cfg;
    messages.insert(0, json!({"role": "system", "content": system}));
    let mut b = json!({"model": model, "messages": messages, "stream": stream, "keep_alive": cfg.ollama_keep_alive,
        "options": {"num_predict": max_tokens, "temperature": temperature, "num_ctx": cfg.ollama_ctx}});
    if json_mode {
        b["format"] = json!("json");
    }
    if ollama_thinks(st, model).await {
        // Off by default: thinking multiplies the time to the first word on a laptop.
        b["think"] = json!(cfg.ollama_think);
    }
    st.http.post(format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/'))).json(&b)
}

async fn ollama_stream(st: &AppState, model: &str, system: &str, history: &[Msg], mut on_delta: impl FnMut(&str) -> bool) -> anyhow::Result<String> {
    let messages: Vec<Value> = history.iter().map(|m| json!({"role": if m.role == "model" { "assistant" } else { "user" }, "content": m.text})).collect();
    let resp = ollama_request(st, model, system, messages, 1500, 0.9, true, false).await.send().await.context("ollama request (is Ollama running?)")?;
    if !resp.status().is_success() {
        let status = resp.status();
        bail!("Ollama {status}: {}", resp.text().await.unwrap_or_default());
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut full = String::new();
    let mut think = ThinkFilter::default();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(e) = v["error"].as_str() {
                bail!("Ollama: {e}");
            }
            let text = think.feed(v["message"]["content"].as_str().unwrap_or_default());
            if !text.is_empty() {
                full.push_str(&text);
                if !on_delta(&text) {
                    return Ok(full);
                }
            }
            if v["done"].as_bool() == Some(true) {
                tracing::info!(provider = "ollama", model, prompt = v["prompt_eval_count"].as_u64().unwrap_or(0), completion = v["eval_count"].as_u64().unwrap_or(0),
                    load_ms = v["load_duration"].as_u64().unwrap_or(0) / 1_000_000, "voice llm usage");
                return Ok(full);
            }
        }
    }
    Ok(full)
}

/// Streams a reply, calling `on_delta` for every visible text delta (thinking is skipped).
/// Return `false` from `on_delta` to stop early. Returns the visible text received.
pub async fn stream_reply(
    st: &AppState,
    provider: Provider,
    system: &str,
    history: &[Msg],
    mut on_delta: impl FnMut(&str) -> bool,
) -> anyhow::Result<String> {
    let cfg = &st.cfg;
    // Nothing that looks like a credential leaves the Mac (scrub_for): the system prompt and the
    // whole history, the user's own words and the window they point at included.
    let scrubbed = scrub_for(provider, system);
    let system: &str = &scrubbed;
    let clean: Vec<Msg> = history.iter().map(|m| Msg { role: m.role, text: scrub_for(provider, &m.text).into_owned() }).collect();
    let history: &[Msg] = &clean;
    match provider {
        Provider::ClaudeCode => return crate::claude_cli::stream(st, cfg.model(provider, Tier::Standard), system, history, on_delta).await,
        Provider::Ollama => return ollama_stream(st, cfg.model(provider, Tier::Standard), system, history, on_delta).await,
        _ => {}
    }
    let req = match provider {
        Provider::Ollama | Provider::ClaudeCode => unreachable!("handled above"),
        Provider::Gemini => {
            let contents: Vec<Value> =
                history.iter().map(|m| json!({"role": m.role, "parts": [{"text": m.text}]})).collect();
            st.http
                .post(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
                    cfg.gemini_model
                ))
                .header("x-goog-api-key", cfg.gemini_key.as_deref().context("GEMINI_API_KEY missing")?)
                .json(&json!({
                    "systemInstruction": {"parts": [{"text": system}]},
                    "contents": contents,
                    "generationConfig": {
                        "maxOutputTokens": 300,
                        "temperature": 0.9,
                        "thinkingConfig": {"thinkingLevel": cfg.thinking_level},
                    },
                }))
        }
        Provider::Anthropic => {
            let messages: Vec<Value> = history.iter().map(|m| json!({"role": if m.role == "model" { "assistant" } else { "user" }, "content": m.text})).collect();
            anthropic_request(st, cfg.model(provider, Tier::Standard), system, messages, 600, 0.9, true)?
        }
        Provider::MiniMax | Provider::OpenAI | Provider::Mistral => {
            let mut messages = vec![json!({"role": "system", "content": system})];
            messages.extend(history.iter().map(|m| {
                json!({"role": if m.role == "model" { "assistant" } else { "user" }, "content": m.text})
            }));
            // Reasoning tokens count against the cap; MiniMax M-series and GPT-5.x always think a little.
            chat_request(st, provider, cfg.voice_model(provider), messages, 1500, 0.9, true, false)?
        }
    };
    let resp = req.send().await.context("llm request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        bail!("{provider:?} {status}: {}", resp.text().await.unwrap_or_default());
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut full = String::new();
    let mut think = ThinkFilter::default();
    // The last SSE chunk carries token usage (prompt / cached): logged per voice turn so the
    // prompt-cache hit rate is visible.
    let log_usage = |v: &Value| {
        let u = if v["usage"].is_object() { &v["usage"] } else { &v["usageMetadata"] };
        if let Some(p) = u["prompt_tokens"].as_u64().or(u["promptTokenCount"].as_u64()) {
            let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().or(u["cachedContentTokenCount"].as_u64()).unwrap_or(0);
            tracing::info!(?provider, model = cfg.voice_model(provider), prompt = p, cached, hit = format!("{:.0}%", 100.0 * cached as f64 / p.max(1) as f64), "voice llm usage");
        }
    };
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Some(data) = std::str::from_utf8(&line).ok().and_then(|l| l.trim().strip_prefix("data:")) else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" || data.contains("\"type\":\"message_stop\"") {
                return Ok(full);
            }
            let v: Value = serde_json::from_str(data).context("sse json")?;
            if v["usage"].is_object() && v["usage"]["prompt_tokens"].is_u64() {
                log_usage(&v);
            }
            let mut text = String::new();
            match provider {
                Provider::Ollama | Provider::ClaudeCode => unreachable!("handled above"),
                Provider::Gemini => {
                    for p in v["candidates"][0]["content"]["parts"].as_array().into_iter().flatten() {
                        if p["thought"].as_bool() != Some(true) {
                            text.push_str(p["text"].as_str().unwrap_or_default());
                        }
                    }
                }
                Provider::Anthropic => {
                    if v["type"] == "content_block_delta" {
                        text.push_str(v["delta"]["text"].as_str().unwrap_or_default());
                    }
                }
                Provider::MiniMax | Provider::OpenAI | Provider::Mistral => {
                    text.push_str(&think.feed(v["choices"][0]["delta"]["content"].as_str().unwrap_or_default()));
                }
            }
            if !text.is_empty() {
                full.push_str(&text);
                if !on_delta(&text) {
                    return Ok(full);
                }
            }
        }
    }
    Ok(full)
}

/// Defensive: drops `<think>…</think>` spans in case a model inlines its reasoning.
#[derive(Default)]
struct ThinkFilter {
    inside: bool,
    pending: String,
}

impl ThinkFilter {
    fn feed(&mut self, s: &str) -> String {
        self.pending.push_str(s);
        let mut out = String::new();
        loop {
            let tag = if self.inside { "</think>" } else { "<think>" };
            match self.pending.find(tag) {
                Some(i) => {
                    if !self.inside {
                        out.push_str(&self.pending[..i]);
                    }
                    self.pending.drain(..i + tag.len());
                    self.inside = !self.inside;
                }
                None => {
                    // Keep a possible partial tag at the end for the next delta.
                    let keep = (1..tag.len()).rev().find(|&k| self.pending.ends_with(&tag[..k])).unwrap_or(0);
                    let cut = self.pending.len() - keep;
                    if !self.inside {
                        out.push_str(&self.pending[..cut]);
                    }
                    self.pending.drain(..cut);
                    return out;
                }
            }
        }
    }
}

/// Opens the pooled TLS connection to the configured provider(s) up front so the first turn
/// doesn't pay the handshake.
pub async fn warm(st: &AppState) {
    let cfg = &st.cfg;
    let mut providers = vec![cfg.provider];
    if cfg.llm_mode == crate::config::LlmMode::Delegate {
        providers = vec![Provider::Gemini, Provider::MiniMax];
    }
    for p in providers {
        if p == Provider::ClaudeCode {
            match &cfg.claude_bin {
                Some(b) => tracing::info!("Claude Code at {}: signed in = {}", b.display(), crate::claude_cli::auth_ok(b).await),
                None => tracing::warn!("Claude Code: `claude` not found"),
            }
            continue;
        }
        let req = match p {
            // Loads the model now (and keeps it loaded) so the first turn doesn't wait for it.
            Provider::Ollama => Some(st.http.post(format!("{}/api/generate", cfg.ollama_url.trim_end_matches('/'))).json(&json!({"model": cfg.model(p, Tier::Standard), "keep_alive": cfg.ollama_keep_alive})).timeout(std::time::Duration::from_secs(120))),
            Provider::ClaudeCode => None,
            Provider::Gemini => cfg.gemini_key.as_ref().map(|k| st.http.get(format!("https://generativelanguage.googleapis.com/v1beta/models/{}", cfg.gemini_model)).header("x-goog-api-key", k)),
            Provider::MiniMax => cfg.minimax_key.as_ref().map(|k| st.http.get("https://api.minimax.io/v1/models").bearer_auth(k)),
            Provider::OpenAI => cfg.openai_key.as_ref().map(|k| st.http.get(format!("{}/models", cfg.openai_url.trim_end_matches('/'))).bearer_auth(k)),
            Provider::Anthropic => cfg.anthropic_key.as_ref().map(|k| st.http.get("https://api.anthropic.com/v1/models").header("x-api-key", k).header("anthropic-version", "2023-06-01")),
            Provider::Mistral => cfg.mistral_key.as_ref().map(|k| st.http.get("https://api.mistral.ai/v1/models").bearer_auth(k)),
        };
        match req.map(|r| r.send()) {
            Some(f) => match f.await {
                Ok(r) => tracing::info!("{p:?} connection warm: {}", r.status()),
                Err(e) => tracing::warn!("{p:?} warmup failed: {e}"),
            },
            None => tracing::warn!("{p:?}: no API key"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ThinkFilter;

    #[test]
    fn think_filter_drops_split_tags() {
        let mut f = ThinkFilter::default();
        let mut out = String::new();
        for d in ["Hi <thi", "nk>secret</th", "ink> there"] {
            out.push_str(&f.feed(d));
        }
        assert_eq!(out, "Hi  there");
    }
}

/// Token usage of one call (completion includes reasoning tokens where the provider counts them).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
    /// Prompt tokens served from the provider's prompt cache.
    pub cached: u64,
}

/// Non-streaming call for background work. `json` asks for a JSON object; on a parse failure
/// the single repair turn carries only the error and the first 400 chars of the bad output
/// (RawKit: retries never resend the whole transcript, so context can't grow per attempt).
/// An empty reply (a reasoning model that spent its budget thinking) is retried with more room;
/// dropped connections, timeouts, 429 and 5xx are retried with a pause (send_retrying).
pub async fn complete(
    st: &AppState,
    provider: Provider,
    system: &str,
    user: &str,
    json: bool,
    max_tokens: u32,
) -> anyhow::Result<(String, Usage)> {
    complete_with(st, provider, Tier::Standard, system, user, json, max_tokens).await
}

/// `complete` on the chosen provider's model for a task tier (light: scoring, mapping, checks).
pub async fn complete_tier(st: &AppState, tier: Tier, system: &str, user: &str, json: bool, max_tokens: u32) -> anyhow::Result<(String, Usage)> {
    complete_with(st, st.cfg.provider, tier, system, user, json, max_tokens).await
}

pub async fn complete_with(
    st: &AppState,
    provider: Provider,
    tier: Tier,
    system: &str,
    user: &str,
    json: bool,
    max_tokens: u32,
) -> anyhow::Result<(String, Usage)> {
    let model = st.cfg.model(provider, tier).to_string();
    let mut usage = Usage::default();
    let mut prompt = user.to_string();
    let mut budget = max_tokens;
    for attempt in 0..2 {
        let (text, u) = send_retrying(st, provider, &model, system, &prompt, json, budget).await?;
        usage.prompt += u.prompt;
        usage.completion += u.completion;
        usage.cached += u.cached;
        if !json {
            return Ok((text, usage));
        }
        let candidate = extract_json(&text);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Ok((candidate, usage));
        }
        let fixed = strip_trailing_commas(&candidate);
        if serde_json::from_str::<Value>(&fixed).is_ok() {
            return Ok((fixed, usage));
        }
        if attempt == 0 {
            prompt = if text.trim().is_empty() {
                budget = budget.saturating_mul(3) / 2;
                format!("{user}\n\nYour previous reply was empty: you ran out of room while thinking. Think briefly, keep every item short, and reply with ONLY the JSON object.")
            } else {
                let err = serde_json::from_str::<Value>(&candidate).err().map(|e| e.to_string()).unwrap_or_default();
                let head: String = text.chars().take(400).collect();
                format!("{user}\n\nYour previous reply was not valid JSON ({err}; it began: {head:?}). Escape quotes inside strings. Reply again with ONLY the JSON object.")
            };
        }
    }
    bail!("model did not return valid JSON")
}

/// complete_once with up to 3 tries on transient failures (network, timeout, 429, 5xx).
async fn send_retrying(st: &AppState, provider: Provider, model: &str, system: &str, user: &str, json: bool, max_tokens: u32) -> anyhow::Result<(String, Usage)> {
    let mut last = None;
    for attempt in 0..3u64 {
        match complete_once(st, provider, model, system, user, json, max_tokens).await {
            Ok(r) => return Ok(r),
            Err(e) if transient(&e) && attempt < 2 => {
                tracing::info!("llm {provider:?} transient error, retrying: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(3 + attempt * 6)).await;
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("llm retries exhausted")))
}

fn transient(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<reqwest::Error>()).any(|r| {
        r.is_timeout() || r.is_connect() || r.is_request() || r.is_body() || r.is_decode()
            || r.status().is_some_and(|s| s.as_u16() == 429 || s.is_server_error())
    })
}

/// `[1, 2,]` / `{"a": 1,}` → valid JSON (a common slip); commas inside strings are left alone.
fn strip_trailing_commas(s: &str) -> String {
    let (mut out, mut in_str, mut esc) = (String::with_capacity(s.len()), false, false);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            let escaped = esc;
            esc = !escaped && c == '\\';
            if c == '"' && !escaped {
                in_str = false;
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        if c == ',' && chars[i + 1..].iter().find(|x| !x.is_whitespace()).is_some_and(|n| *n == '}' || *n == ']') {
            continue;
        }
        out.push(c);
    }
    out
}

fn extract_json(s: &str) -> String {
    let s = s.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}

/// Long prompts on reasoning models (Reddit extraction, reports) can take minutes at MiniMax.
const BACKGROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

/// Every request to a provider outside this Mac is scrubbed of credentials first
/// (secrets::redact: passwords, keys, tokens, one-time and card codes, card numbers, IBANs).
/// Ollama runs locally and gets the text as it is. Rule: passwords never leave the PC.
pub fn scrub_for(provider: Provider, text: &str) -> std::borrow::Cow<'_, str> {
    if provider == Provider::Ollama {
        return std::borrow::Cow::Borrowed(text);
    }
    let r = crate::secrets::redact(text);
    if r == text { std::borrow::Cow::Borrowed(text) } else {
        tracing::info!(provider = ?provider, "secrets masked before a model request");
        std::borrow::Cow::Owned(r)
    }
}

async fn complete_once(st: &AppState, provider: Provider, model: &str, system: &str, user: &str, json: bool, max_tokens: u32) -> anyhow::Result<(String, Usage)> {
    let cfg = &st.cfg;
    let (system_c, user_c) = (scrub_for(provider, system), scrub_for(provider, user));
    let (system, user): (&str, &str) = (&system_c, &user_c);
    match provider {
        Provider::ClaudeCode => {
            // The CLI has no token budget flag; the prompt's own length limits apply.
            let user = if json { format!("{user}\n\nReply with ONLY the JSON object, no prose.") } else { user.to_string() };
            crate::claude_cli::complete(st, model, system, &user, BACKGROUND_TIMEOUT).await
        }
        Provider::Ollama => {
            let resp = ollama_request(st, model, system, vec![json!({"role": "user", "content": user})], max_tokens, 0.2, false, json).await
                .timeout(BACKGROUND_TIMEOUT).send().await.context("ollama request (is Ollama running?)")?;
            if !resp.status().is_success() {
                let status = resp.status();
                bail!("Ollama {status}: {}", resp.text().await.unwrap_or_default());
            }
            let v: Value = resp.json().await?;
            let mut f = ThinkFilter::default();
            let text = f.feed(v["message"]["content"].as_str().unwrap_or(""));
            Ok((text.trim().to_string(), Usage { prompt: v["prompt_eval_count"].as_u64().unwrap_or(0), completion: v["eval_count"].as_u64().unwrap_or(0), cached: 0 }))
        }
        Provider::Gemini => {
            let mut gen_cfg = json!({"maxOutputTokens": max_tokens, "temperature": 0.2, "thinkingConfig": {"thinkingLevel": cfg.thinking_level}});
            if json {
                gen_cfg["responseMimeType"] = json!("application/json");
            }
            let v: Value = st
                .http
                .post(format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"))
                .header("x-goog-api-key", cfg.gemini_key.as_deref().context("GEMINI_API_KEY missing")?)
                .json(&json!({"systemInstruction": {"parts": [{"text": system}]}, "contents": [{"role": "user", "parts": [{"text": user}]}], "generationConfig": gen_cfg}))
                .timeout(BACKGROUND_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let text: String = v["candidates"][0]["content"]["parts"].as_array().into_iter().flatten().filter(|p| p["thought"].as_bool() != Some(true)).filter_map(|p| p["text"].as_str()).collect();
            let m = &v["usageMetadata"];
            Ok((text, Usage { prompt: m["promptTokenCount"].as_u64().unwrap_or(0), completion: m["candidatesTokenCount"].as_u64().unwrap_or(0) + m["thoughtsTokenCount"].as_u64().unwrap_or(0), cached: m["cachedContentTokenCount"].as_u64().unwrap_or(0) }))
        }
        Provider::Anthropic => {
            let v: Value = anthropic_request(st, model, system, vec![json!({"role": "user", "content": user})], max_tokens, 0.2, false)?
                .timeout(BACKGROUND_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let text: String = v["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let u = &v["usage"];
            Ok((text.trim().to_string(), Usage { prompt: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0), completion: u["output_tokens"].as_u64().unwrap_or(0), cached: u["cache_read_input_tokens"].as_u64().unwrap_or(0) }))
        }
        Provider::MiniMax | Provider::OpenAI | Provider::Mistral => {
            let v: Value = chat_request(st, provider, model, vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": user})], max_tokens, 0.2, false, json)?
                .timeout(BACKGROUND_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let mut f = ThinkFilter::default();
            let text = f.feed(v["choices"][0]["message"]["content"].as_str().unwrap_or(""));
            let u = &v["usage"];
            Ok((text.trim().to_string(), Usage { prompt: u["prompt_tokens"].as_u64().unwrap_or(0), completion: u["completion_tokens"].as_u64().unwrap_or(0), cached: u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0) }))
        }
    }
}


#[cfg(test)]
mod cache_probe {
    /// Does the provider serve a repeated long prefix from its cache? (`cached` on the 2nd call)
    /// `cargo test --release --lib cache_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn cache_probe() {
        let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
        let app = crate::AppState::from_env().await.unwrap();
        let system: String = (0..400).map(|i| format!("Rule {i}: answer briefly and stay friendly, never invent facts about item {i}. ")).collect();
        for p in [super::Provider::MiniMax, super::Provider::OpenAI] {
            for q in ["What is 2+2?", "What is 3+3?", "What is 4+4?"] {
                match super::complete(&app, p, &system, q, false, 50).await {
                    Ok((_, u)) => println!("{p:?} {q}: prompt {} cached {}", u.prompt, u.cached),
                    Err(e) => println!("{p:?}: {e:#}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod json_repair {
    #[test]
    fn trailing_commas() {
        let f = super::strip_trailing_commas;
        assert_eq!(f(r#"{"a": [1, 2,], "b": {"c": 1 ,}, }"#), r#"{"a": [1, 2], "b": {"c": 1 } }"#);
        assert_eq!(f(r#"{"q": "x,]", "e": "say \"hi,}\"",}"#), r#"{"q": "x,]", "e": "say \"hi,}\""}"#);
    }
}

// ---------------------------------------------------------------------------------------------
// Key check for the onboarding: list the account's models, keep each tier's model only if the
// account has it (else the standard one), and make one tiny call with the standard model.

/// → the verified [light, standard, heavy] models. Ollama: `key` is unused, the models must be
/// installed (the settings handler pulls the standard one first). Claude Code: the CLI must be
/// signed in.
pub async fn check_key(st: &AppState, p: Provider, key: &str, wanted: [String; 3]) -> anyhow::Result<[String; 3]> {
    let http = &st.http;
    match p {
        Provider::Ollama => {
            let installed = ollama_models(st).await.context("Ollama isn't running: install it from ollama.com, open it, then try again")?;
            let have = |m: &str| installed.iter().any(|i| i == m || i.trim_end_matches(":latest") == m.trim_end_matches(":latest"));
            if !have(&wanted[1]) {
                bail!("{} isn't downloaded yet", wanted[1]);
            }
            let models = [if have(&wanted[0]) { wanted[0].clone() } else { wanted[1].clone() }, wanted[1].clone(), if have(&wanted[2]) { wanted[2].clone() } else { wanted[1].clone() }];
            let (text, _) = complete_once(st, p, &models[1], "You are a terse assistant.", "Reply with the word ok.", false, 50).await.context("test call failed")?;
            tracing::info!(model = models[1], reply = text, "ollama test call");
            return Ok(models);
        }
        Provider::ClaudeCode => {
            let bin = st.cfg.claude_bin.as_ref().context("Claude Code isn't installed on this Mac (claude.ai/code)")?;
            if !crate::claude_cli::auth_ok(bin).await {
                bail!("Claude Code isn't signed in: open Terminal, run `claude`, then `/login`");
            }
            let (text, _) = crate::claude_cli::complete(st, &wanted[1], "You are a terse assistant.", "Reply with the word ok.", std::time::Duration::from_secs(60)).await.context("test call failed")?;
            tracing::info!(model = wanted[1], reply = text, "claude code test call");
            return Ok(wanted);
        }
        _ => {}
    }
    let list: Value = match p {
        Provider::MiniMax => http.get("https://api.minimax.io/v1/models").bearer_auth(key),
        Provider::OpenAI => http.get("https://api.openai.com/v1/models").bearer_auth(key),
        Provider::Mistral => http.get("https://api.mistral.ai/v1/models").bearer_auth(key),
        Provider::Anthropic => http.get("https://api.anthropic.com/v1/models?limit=100").header("x-api-key", key).header("anthropic-version", "2023-06-01"),
        Provider::Gemini => http.get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=200").header("x-goog-api-key", key),
        Provider::Ollama | Provider::ClaudeCode => unreachable!(),
    }
    .timeout(std::time::Duration::from_secs(20)).send().await?.error_for_status().context("the key was rejected")?.json().await.unwrap_or_default();
    let ids: Vec<String> = list["data"].as_array().or(list["models"].as_array()).into_iter().flatten()
        .filter_map(|m| m["id"].as_str().or(m["name"].as_str()).map(|s| s.trim_start_matches("models/").to_string())).collect();
    let have = |m: &str| ids.is_empty() || ids.iter().any(|i| i == m);
    let standard = if have(&wanted[1]) { wanted[1].clone() } else { ids.iter().find(|i| !i.contains("embed") && !i.contains("tts") && !i.contains("image")).cloned().context("no usable chat model on this account")? };
    let pick = |m: &String| if have(m) { m.clone() } else { standard.clone() };
    let models = [pick(&wanted[0]), standard.clone(), pick(&wanted[2])];
    // One tiny real call with the standard model.
    let msg = json!([{"role": "user", "content": "Reply with the word ok."}]);
    let r = match p {
        Provider::Anthropic => http.post(ANTHROPIC).header("x-api-key", key).header("anthropic-version", "2023-06-01").json(&json!({"model": models[1], "max_tokens": 16, "messages": msg})),
        Provider::Gemini => http.post(format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", models[1])).header("x-goog-api-key", key)
            .json(&json!({"contents": [{"role": "user", "parts": [{"text": "Reply with the word ok."}]}]})),
        Provider::MiniMax => http.post("https://api.minimax.io/v1/chat/completions").bearer_auth(key).json(&json!({"model": models[1], "max_tokens": 300, "messages": msg})),
        Provider::Mistral => http.post(MISTRAL).bearer_auth(key).json(&json!({"model": models[1], "max_tokens": 16, "messages": msg})),
        Provider::OpenAI => http.post("https://api.openai.com/v1/chat/completions").bearer_auth(key).json(&json!({"model": models[1], "max_completion_tokens": 300, "messages": msg})),
        Provider::Ollama | Provider::ClaudeCode => unreachable!(),
    };
    let resp = r.timeout(std::time::Duration::from_secs(60)).send().await?;
    if !resp.status().is_success() {
        bail!("test call failed ({}): {}", resp.status(), resp.text().await.unwrap_or_default().chars().take(200).collect::<String>());
    }
    Ok(models)
}
