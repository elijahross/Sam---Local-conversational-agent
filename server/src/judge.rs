//! Laya micro-judgments (local, ~20-30 ms, calibrated probabilities) via the mind sidecar.
//!
//! Same pattern as Bops' TypeSafe "Jev" layer: typed questions about a state, each caller picks
//! its own threshold and its own behaviour when the judge is down (fail open, never block).
//! The checkpoint "sam" is Laya fine-tuned on Gemini-labelled decisions for exactly the
//! questions below, generated from laya lab (regenerate with laya lab).
//!
//! Held-out accuracy (laya lab): routing 88%, end of turn 90%, secret 88%,
//! DOM relevance 80%, supersession 89% (proposal only, an LLM verifies).

use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone)]
pub struct Judge {
    http: reqwest::Client,
    url: String,
}

pub fn question(id: &str) -> Value {
    match id {
        "route" => json!({"type": "choice", "instructions": "What should a voice assistant do with this request?", "criteria": {"chat": "just talk: small talk, feelings, opinions, jokes, simple quick facts", "memory": "recall something the user told the assistant before", "task": "do work in the background with tools: email, web research, browser actions, files and documents", "deep": "think carefully: planning, decisions with tradeoffs, detailed explanations or comparisons"}}),
        "kind" => json!({"type": "choice", "instructions": "If this is a background task, which kind?", "criteria": {"email": "read, send or reply to emails", "research": "look things up on the web and summarize", "browser": "operate a website: book, buy, fill forms, check prices on a specific site", "documents": "work with local files, PDFs, spreadsheets, photos"}}),
        "done" => json!({"type": "noul", "instructions": "Has the speaker finished their turn, rather than stopping mid-thought (trailing conjunction, filler, unfinished phrase) with more to come?"}),
        "missing" => json!({"type": "noul", "instructions": "To carry out this request, would an assistant have to guess a detail the speaker did not give — which day or time, which person, which item or place — rather than being able to start right away?"}),
        "secret" => json!({"type": "noul", "instructions": "Does the user ask to keep this private, secret, off the record, or not to be remembered?"}),
        "rel" => json!({"type": "choice", "instructions": "How does the new fact relate to the known fact?", "criteria": {"same": "says the same thing as the known fact (a paraphrase or duplicate)", "replaces": "updates, contradicts or supersedes the known fact, so the known fact is no longer fully true", "unrelated": "a different fact; both can be true at the same time"}}),
        "low" => json!({"type": "noul", "instructions": "Is this conversation only small talk or general quick questions, with nothing new or personal learned about the user?"}),
        "relevant" => json!({"type": "noul", "instructions": "Does this web page element help achieve the goal (an element to click, fill in, or read)?"}),
        "page_state" => json!({"type": "choice", "instructions": "What state is this web page in for an agent trying to get something done?", "criteria": {"content": "normal readable page content", "results": "a list of search results, listings or products", "form": "a form to fill in is the main content", "login_wall": "requires signing in or creating an account to continue", "captcha": "a bot check or captcha blocks the page", "cookie_banner": "a cookie or consent dialog covers the page", "error": "error page, not found, server error or access denied", "confirmation": "an action completed: booking, order or submission confirmed"}}),
        "action_ok" => json!({"type": "noul", "instructions": "Did the browser action succeed and move the task forward?"}),
        "needs_visual" => json!({"type": "noul", "instructions": "Would a picture shown next to this reply make it clearer or more useful, for example showing what a place, dish, product, animal or thing looks like?"}),
        "image_match" => json!({"type": "noul", "instructions": "Does this image fit the reply and illustrate what it talks about?"}),
        "note_intent" => json!({"type": "choice", "instructions": "Does the user want something written down, and how?", "criteria": {"none": "no note needed, just conversation", "create": "draft or write a new note, message or summary", "append": "add something to an existing note or list", "todo": "a to-do item to remember to do", "reminder": "a reminder at a specific time or place"}}),
        "doc_relevant" => json!({"type": "noul", "instructions": "Is this page relevant to the research question, with content that helps answer it?"}),
        _ => Value::Null,
    }
}

pub fn questions(ids: &[&str]) -> Value {
    Value::Object(ids.iter().map(|id| (id.to_string(), question(id))).collect())
}

/// Probability of "yes" for a noul answer.
pub fn noul(answers: &Value, id: &str) -> Option<f32> {
    answers[id]["noul"].as_f64().map(|p| p as f32)
}

/// (choice, probability of that choice) for a choice answer.
pub fn choice(answers: &Value, id: &str) -> Option<(String, f32)> {
    let c = answers[id]["choice"].as_str()?.to_string();
    let p = answers[id]["probabilities"][&c].as_f64().unwrap_or(0.0) as f32;
    Some((c, p))
}

/// Probability of one specific option of a choice answer.
pub fn option_p(answers: &Value, id: &str, option: &str) -> f32 {
    answers[id]["probabilities"][option].as_f64().unwrap_or(0.0) as f32
}

impl Judge {
    pub fn new(http: reqwest::Client, url: String) -> Self {
        Self { http, url }
    }

    /// One state, several questions, one forward pass. `None` = judge unavailable or too slow.
    /// Fire-and-forget: start loading embeddings + Laya (they unload when idle).
    pub fn warm(&self) {
        let (http, url) = (self.http.clone(), self.url.clone());
        tokio::spawn(async move {
            let _ = http.post(format!("{url}/warm")).json(&json!({})).timeout(Duration::from_secs(3)).send().await;
        });
    }

    pub async fn ask(&self, state: Value, qs: Value, timeout_ms: u64) -> Option<Value> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let r = self
                .http
                .post(format!("{}/judge", self.url))
                .json(&json!({"state": state, "questions": qs}))
                .timeout(left.max(Duration::from_millis(50)))
                .send()
                .await;
            let v: Value = match r {
                Ok(r) => r.json().await.ok()?,
                Err(e) => {
                    tracing::debug!("judge unavailable: {e}");
                    return None;
                }
            };
            // The model is (re)loading after idle: within the budget, ask again shortly rather
            // than giving up (2026-10-14: the first request after a quiet spell got "no answer"
            // at once, and "find the file…" went nowhere).
            if v.get("answers").is_none() && v.get("retry").is_some() {
                if deadline.saturating_duration_since(std::time::Instant::now()) > Duration::from_millis(400) {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                return None;
            }
            if v.get("answers").is_none() {
                tracing::warn!("judge error: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("no answers"));
            }
            return v.get("answers").cloned();
        }
    }

    /// Many (state, questions) items in one request; background use.
    pub async fn batch(&self, items: Vec<(Value, Value)>, timeout_ms: u64) -> Option<Vec<Value>> {
        if items.is_empty() {
            return Some(Vec::new());
        }
        let items: Vec<Value> = items.into_iter().map(|(s, q)| json!({"state": s, "questions": q})).collect();
        let r = self
            .http
            .post(format!("{}/judge_batch", self.url))
            .json(&json!({"items": items}))
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .ok()?;
        let v: Value = r.json().await.ok()?;
        serde_json::from_value(v.get("results")?.clone()).ok()
    }
}

/// Bops' regex pre-check: obvious privacy requests never even reach the judge.
pub fn secret_regex(text: &str) -> bool {
    let t = text.to_lowercase();
    ["don't tell", "dont tell", "keep this between us", "between you and me", "off the record", "keep it private", "keep this private",
     "don't remember", "dont remember", "forget i said", "forget what i said", "keep it secret", "keep this secret", "promise you won't bring"]
        .iter()
        .any(|p| t.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse() {
        let a = json!({"route": {"choice": "task", "probabilities": {"task": 0.9, "chat": 0.1}}, "done": {"noul": 0.2}});
        assert_eq!(choice(&a, "route"), Some(("task".into(), 0.9)));
        assert_eq!(noul(&a, "done"), Some(0.2));
        assert_eq!(option_p(&a, "route", "chat"), 0.1);
        assert!(secret_regex("Okay, this is off the record"));
        assert!(!secret_regex("Tell me a secret about octopuses"));
    }
}
