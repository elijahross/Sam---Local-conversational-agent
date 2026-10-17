//! Long-term memory (lab winner, see memory lab):
//! - SQLite is the source of truth: text, timestamps, validity, full-precision embedding.
//! - turbovec (4-bit, stable u64 ids = rowid) is a derived in-process index, rebuilt at boot
//!   in ~1 s per million rows; it is never backed up.
//! - Units are extracted facts (superseded, never deleted: `valid_to`) + episodic session
//!   summaries. Raw turns are only kept in `messages` as the transcript log.
//! - Read path: Qwen3-Embedding-0.6B query (~13 ms) → SQL allowlist (optional time window)
//!   → turbovec top-100 → exact cosine rerank → z-scored relevance + 0.5·recency (half-life
//!   14 d; "lately/recently" boosts recency). Superseded facts stay retrievable at −2 z and are
//!   labelled outdated; low-value sessions (question-only) persist at −0.5 z.
//!   Lab: equal or better than no decay on every subset (old facts, history, recent).
//! - Write path (background): Laya privacy gate → Gemini extracts facts (add/update/same) and a
//!   summary → Laya proposes extra supersessions among nearest facts → one batched LLM call
//!   verifies them (8/8 caught in the lab) → Laya flags low-value sessions.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::{Datelike, Local, TimeZone};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use turbovec::id_map::IdMapIndex;

use crate::judge::{self, Judge};

#[derive(Clone, Debug)]
pub struct MemoryConfig {
    pub data_dir: PathBuf,
    pub embed_url: String,
    pub k: usize,
    pub decay_weight: f32,
    pub half_life_days: f32,
    pub timeout_ms: u64,
    /// Weight of ln(times_seen) in recall (vault: "Semantic-Reinforced-Recent Representation Blend").
    pub reinforce_weight: f32,
}

// Lab-tuned constants (memory lab, stages G–I).
const RECENT_CUE_WEIGHT: f32 = 4.0;
const RECENT_CUE_HALF_LIFE: f32 = 7.0;
const SUPERSEDED_PENALTY: f32 = 2.0;
const LOW_VALUE_PENALTY: f32 = 0.5;
const PATTERN_PENALTY: f32 = 0.5;
/// Lab J (3×36 answers): patterns in per-turn recall didn't help (76.9% vs 78.7% without);
/// they stay available to the deep recall_memory search only.
const MAX_PATTERNS: usize = 0;
/// Cosine at which a new fact counts as the same as an active one (merged, not duplicated).
const DUP_COS: f32 = 0.95;

#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub kind: String,
    pub text: String,
    pub ts: i64,
    pub valid_to: Option<i64>,
    pub low_value: bool,
    pub times_seen: i64,
}

pub struct Memory {
    cfg: MemoryConfig,
    db: Mutex<Connection>,
    index: RwLock<IdMapIndex>,
    model: String,
    dim: usize,
    http: reqwest::Client,
    /// Active facts changed since the last card refresh (the card is built from facts only).
    /// Starts true: one refresh after boot. A refresh after "nothing to consolidate" cost a
    /// 2–8k-token call and, by rewriting the card, broke the voice prompt's stable prefix.
    card_dirty: std::sync::atomic::AtomicBool,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS memories (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,              -- fact | summary | media (later: image/video captions + embedding)
  text TEXT NOT NULL,
  ts INTEGER NOT NULL,             -- unix seconds the memory was formed
  session_id TEXT,
  valid_to INTEGER,                -- set when superseded; never deleted
  superseded_by INTEGER,
  media_path TEXT,
  low_value INTEGER NOT NULL DEFAULT 0,  -- question-only / small-talk sessions: kept, ranked lower
  embed_model TEXT NOT NULL,
  embedding BLOB NOT NULL          -- f32 little-endian, unit norm
);
CREATE INDEX IF NOT EXISTS memories_active ON memories(valid_to, ts);
CREATE INDEX IF NOT EXISTS memories_session ON memories(session_id);
CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL,
  text TEXT NOT NULL,
  ts INTEGER NOT NULL,
  consolidated INTEGER NOT NULL DEFAULT 0,
  private INTEGER NOT NULL DEFAULT 0     -- user asked to keep it secret: redacted, never consolidated
);
CREATE INDEX IF NOT EXISTS messages_pending ON messages(consolidated, session_id);
-- Background agent work (memory consolidation today; research, email, browser later).
-- Results live as files under data/artifacts/<id>/; the row only points at them.
-- Always-in-context profile (vault: Prefix-Typed Identity Card), at most 40 typed lines.
CREATE TABLE IF NOT EXISTS user_card (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  text TEXT NOT NULL,
  updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Open threads: pending things in their life Sam should ask about later ('the interview on
-- Thursday'). Found and closed by the consolidation's own extraction call; never deleted.
CREATE TABLE IF NOT EXISTS threads (
  id INTEGER PRIMARY KEY,
  text TEXT NOT NULL,
  due TEXT,                        -- YYYY-MM-DD when a day was said
  created INTEGER NOT NULL,
  session_id TEXT,
  status TEXT NOT NULL DEFAULT 'open',  -- open | offered | resolved | expired | dropped
  offered_at INTEGER,
  closed_at INTEGER
);
CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,            -- queued | running | done | failed | cancelled
  input TEXT NOT NULL,
  summary TEXT,
  artifact_dir TEXT,
  created INTEGER NOT NULL,
  updated INTEGER NOT NULL
);
";

fn now() -> i64 {
    Local::now().timestamp()
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Memory {
    pub async fn open(cfg: MemoryConfig, http: reqwest::Client) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let health: Value = http
            .get(format!("{}/health", cfg.embed_url))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .context("embedding sidecar not reachable")?
            .json()
            .await?;
        let model = health["model"].as_str().context("embed model")?.to_string();
        let dim = health["dim"].as_u64().context("embed dim")? as usize;

        let db = Connection::open(cfg.data_dir.join("memory.sqlite"))?;
        db.execute_batch(SCHEMA)?;
        // Additive migrations for databases created before these columns existed.
        for (table, col, def) in [
            ("memories", "low_value", "INTEGER NOT NULL DEFAULT 0"),
            ("messages", "private", "INTEGER NOT NULL DEFAULT 0"),
            ("memories", "times_seen", "INTEGER NOT NULL DEFAULT 1"),
            ("memories", "sources", "TEXT"),
        ] {
            let has: bool = db.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?.exists([col])?;
            if !has {
                db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {def}"))?;
            }
        }
        let mem = Self {
            cfg,
            db: Mutex::new(db),
            index: RwLock::new(IdMapIndex::new(dim, 4).map_err(|e| anyhow::anyhow!("{e:?}"))?),
            model,
            dim,
            http,
            card_dirty: std::sync::atomic::AtomicBool::new(true),
        };
        mem.reembed_stale().await?;
        mem.rebuild_index()?;
        Ok(mem)
    }

    /// Boot guard (vault: "Vector-Dimension Boot Guard"): rows embedded with another model
    /// or dim live in an incompatible space, so they are re-embedded before indexing.
    async fn reembed_stale(&self) -> anyhow::Result<()> {
        let stale: Vec<(i64, String)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, text FROM memories WHERE embed_model != ?1")?;
            q.query_map([&self.model], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        if stale.is_empty() {
            return Ok(());
        }
        tracing::warn!(n = stale.len(), model = self.model, "re-embedding memories for new embedding model");
        for chunk in stale.chunks(64) {
            let vecs = self.embed(&chunk.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(), "doc").await?;
            let db = self.db.lock().unwrap();
            for ((id, _), v) in chunk.iter().zip(vecs) {
                db.execute("UPDATE memories SET embedding = ?1, embed_model = ?2 WHERE id = ?3", params![to_blob(&v), self.model, id])?;
            }
        }
        Ok(())
    }

    fn rebuild_index(&self) -> anyhow::Result<()> {
        let (ids, flat) = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, embedding FROM memories")?;
            let mut ids = Vec::new();
            let mut flat = Vec::new();
            for row in q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))? {
                let (id, b) = row?;
                ids.push(id as u64);
                flat.extend(from_blob(&b));
            }
            (ids, flat)
        };
        let mut ix = IdMapIndex::new(self.dim, 4).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        if !ids.is_empty() {
            ix.add_with_ids(&flat, &ids).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        tracing::info!(n = ids.len(), model = self.model, "memory index built");
        *self.index.write().unwrap() = ix;
        Ok(())
    }

    async fn embed(&self, texts: &[String], kind: &str) -> anyhow::Result<Vec<Vec<f32>>> {
        let r: Value = self
            .http
            .post(format!("{}/embed", self.cfg.embed_url))
            .json(&json!({"texts": texts, "kind": kind}))
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let vecs: Vec<Vec<f32>> = serde_json::from_value(r["vectors"].clone()).context("embed vectors")?;
        if vecs.iter().any(|v| v.len() != self.dim) {
            bail!("embedding dim mismatch");
        }
        Ok(vecs)
    }

    /// Read path. Bounded by `timeout_ms` (Bops "time-boxed memory injection"): a slow
    /// embedder means no memory this turn, never a slower reply.
    pub async fn recall(&self, query: &str) -> Vec<Item> {
        self.recall_k(query, self.cfg.k).await
    }

    /// Same as `recall` with a custom depth (memory questions get a deeper prefetch).
    pub async fn recall_k(&self, query: &str, k: usize) -> Vec<Item> {
        match tokio::time::timeout(Duration::from_millis(self.cfg.timeout_ms), self.recall_inner(query, k, None)).await {
            Ok(Ok(items)) => items,
            Ok(Err(e)) => {
                tracing::warn!("memory recall failed: {e:#}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("memory recall timed out");
                Vec::new()
            }
        }
    }

    async fn recall_inner(&self, query: &str, k: usize, range: Option<(i64, i64)>) -> anyhow::Result<Vec<Item>> {
        let qv = self.embed(&[query.to_string()], "query").await?.remove(0);
        let window = match range {
            Some((a, b)) => Window::Range(a, b),
            None => time_window(query),
        };
        let recent_cue = matches!(window, Window::Recent);
        let (allow, rows) = {
            let db = self.db.lock().unwrap();
            let allow: Vec<u64> = match window {
                Window::None | Window::Recent => {
                    let mut q = db.prepare_cached("SELECT id FROM memories")?;
                    q.query_map([], |r| r.get::<_, i64>(0))?.map(|r| r.map(|x| x as u64)).collect::<Result<_, _>>()?
                }
                Window::Range(from, to) => {
                    let mut q = db.prepare_cached("SELECT id FROM memories WHERE ts >= ?1 AND ts < ?2")?;
                    q.query_map([from, to], |r| r.get::<_, i64>(0))?.map(|r| r.map(|x| x as u64)).collect::<Result<_, _>>()?
                }
                Window::Earliest => {
                    let mut q = db.prepare_cached(
                        "SELECT id FROM memories WHERE session_id = (SELECT session_id FROM memories ORDER BY ts LIMIT 1)",
                    )?;
                    q.query_map([], |r| r.get::<_, i64>(0))?.map(|r| r.map(|x| x as u64)).collect::<Result<_, _>>()?
                }
            };
            if allow.is_empty() {
                return Ok(Vec::new());
            }
            let (_, cand) = self
                .index
                .read()
                .unwrap()
                .search_with_allowlist(&qv, 100, Some(&allow))
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let mut rows = Vec::with_capacity(cand.len());
            let mut q = db.prepare_cached("SELECT id, kind, text, ts, valid_to, low_value, embedding, times_seen FROM memories WHERE id = ?1")?;
            for id in &cand {
                rows.push(q.query_row([*id as i64], |r| {
                    Ok((
                        Item {
                            id: r.get(0)?,
                            kind: r.get(1)?,
                            text: r.get(2)?,
                            ts: r.get(3)?,
                            valid_to: r.get(4)?,
                            low_value: r.get::<_, i64>(5)? != 0,
                            times_seen: r.get(7)?,
                        },
                        r.get::<_, Vec<u8>>(6)?,
                    ))
                })?);
            }
            (allow, rows)
        };
        let _ = allow;
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // Exact rerank on full-precision vectors, then the lab's scoring. Relevance is z-scored
        // over the candidates: scale-free, so recency can't swamp it (min-max + additive decay
        // lost 7 recall points and all history questions in the lab).
        let rel: Vec<f32> = rows.iter().map(|(_, b)| dot(&qv, &from_blob(b))).collect();
        let n = rel.len() as f32;
        let mean = rel.iter().sum::<f32>() / n;
        let std = (rel.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n).sqrt() + 1e-6;
        let (w, hl) = if recent_cue { (RECENT_CUE_WEIGHT, RECENT_CUE_HALF_LIFE) } else { (self.cfg.decay_weight, self.cfg.half_life_days) };
        let now = now();
        let mut scored: Vec<(f32, Item)> = rows
            .into_iter()
            .zip(&rel)
            .map(|((item, _), &r)| {
                let z = (r - mean) / std;
                let mut score = if matches!(window, Window::Range(..) | Window::Earliest) {
                    z // explicit time window: relevance inside it
                } else {
                    let age_days = (now - item.ts).max(0) as f32 / 86_400.0;
                    z + w * 0.5f32.powf(age_days / hl)
                };
                if item.valid_to.is_some() {
                    score -= SUPERSEDED_PENALTY;
                }
                if item.low_value {
                    score -= LOW_VALUE_PENALTY;
                }
                // Facts the user keeps coming back to surface even off-topic (capped).
                score += self.cfg.reinforce_weight * (item.times_seen.clamp(1, 20) as f32).ln();
                // Inferred patterns rank below observed facts (lab J: unpenalized, they pushed
                // facts out of the top-k and cost 5/36 answers).
                if item.kind == "pattern" {
                    score -= PATTERN_PENALTY;
                }
                (score, item)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Separate buckets (vault: Honcho dialectic prefetch): at most MAX_PATTERNS derived
        // conclusions per recall, none inside an explicit time window; deep search (k ≥ 15)
        // keeps them all.
        let cap = if k >= 15 { k } else if matches!(window, Window::Range(..) | Window::Earliest) { 0 } else { MAX_PATTERNS };
        let mut n_pat = 0;
        Ok(scored
            .into_iter()
            .filter(|(_, i)| {
                if i.kind != "pattern" {
                    return true;
                }
                n_pat += 1;
                n_pat <= cap
            })
            .take(k)
            .map(|(_, i)| i)
            .collect())
    }

    /// Prompt block for the working memory (empty string when nothing is remembered).
    pub fn render(items: &[Item]) -> String {
        if items.is_empty() {
            return String::new();
        }
        let now = now();
        let lines: Vec<String> = items
            .iter()
            .map(|i| {
                let kind = match i.kind.as_str() {
                    "summary" => "conversation",
                    "pattern" => "inferred pattern",
                    _ => "fact",
                };
                let old = i.valid_to.map(|t| format!(", OUTDATED since {}", ago(now - t))).unwrap_or_default();
                format!("- ({kind}, {}{old}) {}", ago(now - i.ts), crate::secrets::mask_contacts(&i.text))
            })
            .collect();
        format!(
            "\n\n# What you remember about them\nFrom past conversations. It can be out of date or wrong; newer beats older, \
and what they say now wins. Use it naturally and don't recite it back.\n{}",
            lines.join("\n")
        )
    }

    /// Lab/import: log a message with its original timestamp.
    pub fn log_message_at(&self, session: &str, role: &str, text: &str, ts: i64) {
        let _ = self.db.lock().unwrap().execute("INSERT INTO messages (session_id, role, text, ts) VALUES (?1, ?2, ?3, ?4)", params![session, role, text, ts]);
    }

    pub fn log_message(&self, session: &str, role: &str, text: &str) {
        let db = self.db.lock().unwrap();
        if let Err(e) =
            db.execute("INSERT INTO messages (session_id, role, text, ts) VALUES (?1, ?2, ?3, ?4)", params![session, role, text, now()])
        {
            tracing::warn!("log message: {e}");
        }
    }

    /// Write path: turn not-yet-consolidated messages of a session into facts + a summary.
    pub async fn consolidate(&self, gemini: &JsonLlm<'_>, judge: Option<&Judge>, session: &str) -> anyhow::Result<String> {
        let msgs: Vec<(i64, String, String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare(
                "SELECT id, role, text, ts FROM messages WHERE session_id = ?1 AND consolidated = 0 AND private = 0 ORDER BY id",
            )?;
            q.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
        };
        if msgs.len() < 2 {
            return Ok("nothing to consolidate".into());
        }

        // 1. Privacy gate (Bops: regex first, then the judge at secret ≥ 0.4). Private user
        //    messages and the reply to them are redacted in the transcript and never extracted.
        let user_msgs: Vec<&(i64, String, String, i64)> = msgs.iter().filter(|m| m.1 == "user").collect();
        let judged = match judge {
            Some(j) => j.batch(user_msgs.iter().map(|m| (json!(m.2), judge::questions(&["secret"]))).collect(), 30_000).await,
            None => None,
        };
        let mut private: Vec<i64> = Vec::new();
        for (k, m) in user_msgs.iter().enumerate() {
            let p = judged.as_ref().and_then(|a| judge::noul(&a[k], "secret")).unwrap_or(0.0);
            if judge::secret_regex(&m.2) || p >= 0.4 {
                private.push(m.0);
                if let Some(reply) = msgs.iter().find(|x| x.0 > m.0 && x.1 == "model") {
                    private.push(reply.0);
                }
            }
        }
        if !private.is_empty() {
            let db = self.db.lock().unwrap();
            for id in &private {
                db.execute("UPDATE messages SET private = 1, consolidated = 1, text = '[private]' WHERE id = ?1", [id])?;
            }
        }
        let kept: Vec<&(i64, String, String, i64)> = msgs.iter().filter(|m| !private.contains(&m.0)).collect();
        let msg_ids: Vec<i64> = kept.iter().map(|m| m.0).collect();
        let started = msgs[0].3;
        if kept.len() < 2 {
            let db = self.db.lock().unwrap();
            for id in &msg_ids {
                db.execute("UPDATE messages SET consolidated = 1 WHERE id = ?1", [id])?;
            }
            return Ok(format!("{} private messages redacted, nothing else to consolidate", private.len()));
        }
        let convo: String = kept
            .iter()
            .map(|(_, role, text, _)| format!("{}: {text}", if role == "user" { "User" } else { "Sam" }))
            .collect::<Vec<_>>()
            .join("\n");
        let active: Vec<(i64, String)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, text FROM memories WHERE kind = 'fact' AND valid_to IS NULL ORDER BY ts DESC LIMIT 60")?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };

        // 2. LLM extraction: facts with add/update/same against active facts + episodic summary.
        let known = if active.is_empty() {
            "(none)".to_string()
        } else {
            active.iter().map(|(id, t)| format!("#{id}: {t}")).collect::<Vec<_>>().join("\n")
        };
        let when = Local.timestamp_opt(started, 0).single().unwrap_or_else(Local::now);
        let r = gemini
            .call(
                &EXTRACT
                    .replace("{ts}", &when.format("%A, %B %-d, %Y %H:%M").to_string())
                    .replace("{known}", &known)
                    .replace("{threads}", &self.threads_prompt())
                    .replace("{conv}", &convo),
            )
            .await?;
        let (n_threads, n_closed) = self.apply_threads(&r, session, started).unwrap_or_else(|e| {
            tracing::warn!("open threads not updated: {e:#}");
            (0, 0)
        });

        let mut new_texts = Vec::new();
        let mut replaces = Vec::new();
        let mut seen_again: Vec<i64> = Vec::new();
        for f in r["facts"].as_array().into_iter().flatten() {
            let op = f["op"].as_str().unwrap_or("add");
            let Some(text) = f["text"].as_str().filter(|t| !t.trim().is_empty()) else { continue };
            // Cited ids are only trusted if they resolve to an active fact (vault: "Grounded
            // Provenance Filter for Derived Conclusions").
            let old = f["replaces"].as_str().and_then(|s| s.trim_start_matches('#').parse::<i64>().ok()).or(f["replaces"].as_i64()).filter(|id| active.iter().any(|(a, _)| a == id));
            if op == "same" {
                seen_again.extend(old);
                continue;
            }
            new_texts.push(("fact", text.to_string()));
            replaces.push(if op == "update" { old } else { None });
        }
        let summary = r["summary"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_string);
        // 3. Low-value sessions (question-only, small talk) persist, flagged to rank lower.
        let low_value = match (&summary, judge) {
            (Some(s), Some(j)) => j.ask(json!(s), judge::questions(&["low"]), 5_000).await.and_then(|a| judge::noul(&a, "low")).is_some_and(|p| p >= 0.8),
            _ => false,
        };
        if let Some(s) = &summary {
            new_texts.push(("summary", s.clone()));
            replaces.push(None);
        }
        let vecs = if new_texts.is_empty() {
            Vec::new()
        } else {
            self.embed(&new_texts.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(), "doc").await?
        };

        // Write-time near-duplicate merge (vault: "Guarded Memory Consolidation"): an "add"
        // that is almost identical to an active fact reinforces it instead of duplicating it.
        let mut skip = vec![false; new_texts.len()];
        {
            let ix = self.index.read().unwrap();
            let db = self.db.lock().unwrap();
            for (i, ((kind, _), v)) in new_texts.iter().zip(&vecs).enumerate() {
                if *kind != "fact" || replaces[i].is_some() {
                    continue;
                }
                let Ok((_, cand)) = ix.search_with_allowlist(v, 3, None) else { continue };
                for id in cand {
                    let row: Option<(Vec<u8>, Option<i64>, String)> = db.query_row("SELECT embedding, valid_to, kind FROM memories WHERE id = ?1", [id as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok();
                    if let Some((b, None, k)) = row {
                        if k == "fact" && dot(v, &from_blob(&b)) >= DUP_COS {
                            seen_again.push(id as i64);
                            skip[i] = true;
                            break;
                        }
                    }
                }
            }
        }
        let mut added = Vec::new();
        {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            for id in &seen_again {
                tx.execute("UPDATE memories SET times_seen = times_seen + 1 WHERE id = ?1 AND valid_to IS NULL", [id])?;
            }
            for (i, (((kind, text), v), old)) in new_texts.iter().zip(&vecs).zip(&replaces).enumerate() {
                if skip[i] {
                    continue;
                }
                tx.execute(
                    "INSERT INTO memories (kind, text, ts, session_id, low_value, embed_model, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![kind, text, started, session, (*kind == "summary" && low_value) as i64, self.model, to_blob(v)],
                )?;
                let id = tx.last_insert_rowid();
                if let Some(old) = old {
                    // An update continues the old fact's history: it inherits its reinforcement.
                    tx.execute("UPDATE memories SET times_seen = (SELECT times_seen FROM memories WHERE id = ?1) WHERE id = ?2", params![old, id])?;
                    tx.execute(
                        "UPDATE memories SET valid_to = ?1, superseded_by = ?2 WHERE id = ?3 AND valid_to IS NULL",
                        params![started, id, old],
                    )?;
                }
                added.push((id as u64, v.clone(), *kind, text.clone()));
            }
            let mut q = tx.prepare("UPDATE messages SET consolidated = 1 WHERE id = ?1")?;
            for id in &msg_ids {
                q.execute([id])?;
            }
            drop(q);
            tx.commit()?;
        }
        {
            let mut ix = self.index.write().unwrap();
            let ids: Vec<u64> = added.iter().map(|a| a.0).collect();
            let flat: Vec<f32> = added.iter().flat_map(|a| a.1.iter().copied()).collect();
            if !ids.is_empty() {
                ix.add_with_ids(&flat, &ids).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
        }

        // 4. Supersessions the extractor missed: Laya proposes among each new fact's nearest
        //    active facts, one batched LLM call verifies (lab: 5/8 → 8/8 caught).
        let llm_superseded = replaces.iter().filter(|r| r.is_some()).count();
        let extra = match judge {
            Some(j) => self.propose_and_verify(j, gemini, &added, session, started).await.unwrap_or_else(|e| {
                tracing::warn!("supersession check failed: {e:#}");
                0
            }),
            None => 0,
        };
        if extra > 0 || added.iter().any(|a| a.2 == "fact") {
            self.card_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(format!(
            "{} memories ({} superseded by extraction, {} more by Laya+verify, {} reinforced), {} private redacted, low_value={low_value}, {n_threads} new threads, {n_closed} closed, from {} messages",
            added.len(),
            llm_superseded,
            extra,
            seen_again.len(),
            private.len(),
            msg_ids.len()
        ))
    }

    async fn propose_and_verify(
        &self,
        judge: &Judge,
        gemini: &JsonLlm<'_>,
        added: &[(u64, Vec<f32>, &str, String)],
        session: &str,
        started: i64,
    ) -> anyhow::Result<usize> {
        let older: Vec<(i64, String, Vec<f32>)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, text, embedding FROM memories WHERE kind = 'fact' AND valid_to IS NULL AND session_id != ?1")?;
            q.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, from_blob(&r.get::<_, Vec<u8>>(2)?))))?.collect::<Result<_, _>>()?
        };
        let mut pairs: Vec<(i64, i64, String, String)> = Vec::new(); // (old id, new id, old text, new text)
        for (new_id, v, kind, text) in added.iter().filter(|a| a.2 == "fact") {
            let _ = kind;
            let mut near: Vec<(f32, &(i64, String, Vec<f32>))> = older.iter().map(|o| (dot(v, &o.2), o)).collect();
            near.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, (old_id, old_text, _)) in near.into_iter().take(3) {
                pairs.push((*old_id, *new_id as i64, old_text.clone(), text.clone()));
            }
        }
        if pairs.is_empty() {
            return Ok(0);
        }
        let answers = judge
            .batch(pairs.iter().map(|p| (json!({"known_fact": p.2, "new_fact": p.3}), judge::questions(&["rel"]))).collect(), 60_000)
            .await
            .unwrap_or_default();
        let proposals: Vec<&(i64, i64, String, String)> =
            pairs.iter().zip(&answers).filter(|(_, a)| judge::option_p(a, "rel", "replaces") >= 0.7).map(|(p, _)| p).collect();
        if proposals.is_empty() {
            return Ok(0);
        }
        let list = proposals.iter().enumerate().map(|(i, p)| format!("{i}: OLD: \"{}\" | NEW: \"{}\"", p.2, p.3)).collect::<Vec<_>>().join("\n");
        let v = gemini
            .call(&format!(
                "For each pair, does the NEW fact make the OLD fact no longer true (an update or replacement)? \
Compatible or merely related facts are NOT replacements.\n{list}\nReturn ONLY JSON {{\"replaces\": [true or false per pair, in order]}}"
            ))
            .await?;
        let verdicts: Vec<bool> = v["replaces"].as_array().map(|a| a.iter().map(|x| x.as_bool().unwrap_or(false)).collect()).unwrap_or_default();
        let db = self.db.lock().unwrap();
        let mut n = 0;
        for (p, ok) in proposals.iter().zip(verdicts) {
            if ok {
                n += db.execute(
                    "UPDATE memories SET valid_to = ?1, superseded_by = ?2 WHERE id = ?3 AND valid_to IS NULL",
                    params![started, p.1, p.0],
                )?;
            }
        }
        Ok(n)
    }

    // --- background task bookkeeping (see tasks.rs) ---

    /// The user card (always in context), if one was built.
    pub fn card(&self) -> Option<String> {
        self.db.lock().unwrap().query_row("SELECT text FROM user_card WHERE id = 1", [], |r| r.get(0)).ok()
    }

    /// The card as the user edited it (a deleted line).
    pub fn set_card(&self, text: &str) -> anyhow::Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO user_card (id, text, updated) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET text = ?1, updated = ?2", params![text, now()])?;
        Ok(())
    }

    /// Rewrites the user card from the strongest active facts (vault: "Prefix-Typed Identity
    /// Card"). The model returns the whole card; lines without a valid prefix are dropped, and
    /// if nothing valid comes back the old card stays (a bad call can't wipe it).
    // ── open threads ─────────────────────────────────────────────────────────────────────────

    /// Threads still worth asking about or closing (open, or offered and not yet answered).
    pub fn open_threads(&self) -> Vec<Thread> {
        let db = self.db.lock().unwrap();
        let Ok(mut q) = db.prepare("SELECT id, text, due, created, status FROM threads WHERE status IN ('open', 'offered') ORDER BY created DESC LIMIT 30") else { return vec![] };
        q.query_map([], |r| {
            Ok(Thread { id: r.get(0)?, text: r.get(1)?, due: r.get::<_, Option<String>>(2)?.and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()), created: r.get(3)?, status: r.get(4)? })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    fn threads_prompt(&self) -> String {
        let t = self.open_threads();
        if t.is_empty() {
            return "(none)".into();
        }
        t.iter().map(|t| format!("#{}: {} ({})", t.id, t.text, t.due.map_or("no day".into(), |d| d.format("%Y-%m-%d").to_string()))).collect::<Vec<_>>().join("\n")
    }

    /// New threads and closed ones from the extraction result; then the expiry sweep.
    fn apply_threads(&self, r: &Value, session: &str, started: i64) -> anyhow::Result<(usize, usize)> {
        let known: Vec<i64> = self.open_threads().iter().map(|t| t.id).collect();
        let db = self.db.lock().unwrap();
        let mut closed = 0;
        for c in r["closed"].as_array().into_iter().flatten() {
            let id = c.as_str().and_then(|s| s.trim_start_matches('#').parse::<i64>().ok()).or(c.as_i64());
            if let Some(id) = id.filter(|id| known.contains(id)) {
                closed += db.execute("UPDATE threads SET status = 'resolved', closed_at = ?1 WHERE id = ?2", params![started, id])?;
            }
        }
        let mut added = 0;
        for t in r["threads"].as_array().into_iter().flatten() {
            let Some(text) = t["text"].as_str().map(str::trim).filter(|x| !x.is_empty() && x.len() < 200) else { continue };
            let due = t["due"].as_str().and_then(|d| chrono::NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()).map(|d| d.format("%Y-%m-%d").to_string());
            let dup: bool = db.query_row("SELECT 1 FROM threads WHERE status IN ('open', 'offered') AND lower(text) = lower(?1)", [text], |_| Ok(())).is_ok();
            if !dup {
                db.execute("INSERT INTO threads (text, due, created, session_id) VALUES (?1, ?2, ?3, ?4)", params![text, due, started, session])?;
                added += 1;
            }
        }
        // A dated thread nobody brought up within a week of its day, or an undated one after a
        // month, is history; an offered one that was never answered closes after two weeks.
        // Dated from the conversation itself (= now in live use; a replayed history in the lab).
        let today = Local.timestamp_opt(started, 0).single().unwrap_or_else(Local::now).date_naive();
        let week_ago = (today - chrono::Duration::days(THREAD_LATE_DAYS + 2)).format("%Y-%m-%d").to_string();
        let t = started;
        db.execute("UPDATE threads SET status = 'expired', closed_at = ?1 WHERE status = 'open' AND due IS NOT NULL AND due < ?2", params![t, week_ago])?;
        db.execute("UPDATE threads SET status = 'expired', closed_at = ?1 WHERE status = 'open' AND due IS NULL AND created < ?2", params![t, t - 30 * 86_400])?;
        db.execute("UPDATE threads SET status = 'expired', closed_at = ?1 WHERE status = 'offered' AND offered_at < ?2", params![t, t - 14 * 86_400])?;
        Ok((added, closed))
    }

    /// Every thread with its status (the You tab, labs).
    pub fn all_threads(&self) -> Vec<Thread> {
        let db = self.db.lock().unwrap();
        let Ok(mut q) = db.prepare("SELECT id, text, due, created, status FROM threads ORDER BY id") else { return vec![] };
        q.query_map([], |r| {
            Ok(Thread { id: r.get(0)?, text: r.get(1)?, due: r.get::<_, Option<String>>(2)?.and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()), created: r.get(3)?, status: r.get(4)? })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    /// The thread to bring up now, if one is due (see `pick_thread`).
    pub fn thread_to_raise(&self) -> Option<Thread> {
        pick_thread(&self.open_threads(), Local::now().date_naive(), now())
    }

    /// `offered` (Sam was given it this session; never offered twice) or `dropped` (the user
    /// deleted it on the You tab).
    pub fn mark_thread(&self, id: i64, status: &str) {
        let db = self.db.lock().unwrap();
        let col = if status == "offered" { "offered_at" } else { "closed_at" };
        let _ = db.execute(&format!("UPDATE threads SET status = ?1, {col} = ?2 WHERE id = ?3"), params![status, now(), id]);
    }

    /// True (and reset) when facts changed since the last check: refresh the card only then.
    pub fn take_card_dirty(&self) -> bool {
        self.card_dirty.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn refresh_card(&self, gemini: &JsonLlm<'_>) -> anyhow::Result<usize> {
        let facts: Vec<(i64, String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, text, times_seen FROM memories WHERE kind = 'fact' AND valid_to IS NULL ORDER BY times_seen DESC, ts DESC LIMIT 80")?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
        };
        if facts.is_empty() {
            return Ok(0);
        }
        let old = self.card().unwrap_or_else(|| "(empty)".into());
        let listing = facts.iter().map(|(_, t, n)| format!("- ({n}x) {t}")).collect::<Vec<_>>().join("\n");
        let r = gemini.call(&CARD.replace("{card}", &old).replace("{facts}", &listing)).await?;
        let lines: Vec<String> = r["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str())
            .map(|l| l.trim().to_string())
            .filter(|l| CARD_PREFIXES.iter().any(|p| l.starts_with(p)) && l.chars().count() <= 200 && !judge::secret_regex(l))
            .take(40)
            .collect();
        if lines.is_empty() {
            return Ok(0);
        }
        self.db.lock().unwrap().execute(
            "INSERT INTO user_card (id, text, updated) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET text = ?1, updated = ?2",
            params![lines.join("\n"), now()],
        )?;
        Ok(lines.len())
    }

    /// Deep search for the recall_memory tool: more results, an optional explicit date range,
    /// and raw transcript lines (for "what exactly did I say about…").
    pub async fn search(&self, query: &str, k: usize, range: Option<(i64, i64)>, transcripts: bool) -> anyhow::Result<(Vec<Item>, Vec<String>)> {
        let items = self.recall_inner(query, k, range).await?;
        let mut lines = Vec::new();
        if transcripts {
            let words: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 4).map(|w| w.to_lowercase()).take(4).collect();
            let db = self.db.lock().unwrap();
            let mut q = db.prepare_cached("SELECT session_id, role, text, ts FROM messages WHERE private = 0 AND lower(text) LIKE ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts DESC LIMIT 8")?;
            let (a, b) = range.unwrap_or((0, i64::MAX));
            let mut seen = std::collections::HashSet::new();
            for w in &words {
                for row in q.query_map(params![format!("%{w}%"), a, b], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
                    let (sess, role, text, ts) = row?;
                    if seen.insert((sess.clone(), ts, role.clone())) {
                        let when = Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                        lines.push(format!("[{when}] {}: {}", if role == "user" { "User" } else { "Sam" }, text.chars().take(240).collect::<String>()));
                    }
                }
            }
            lines.truncate(12);
        }
        Ok((items, lines))
    }

    /// Facts added since the last dream (the dream gate counts these).
    pub fn facts_since_dream(&self) -> (i64, i64) {
        let db = self.db.lock().unwrap();
        let last: i64 = db.query_row("SELECT value FROM meta WHERE key = 'last_dream'", [], |r| r.get::<_, String>(0)).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let n: i64 = db.query_row("SELECT count(*) FROM memories WHERE kind = 'fact' AND rowid > (SELECT coalesce((SELECT value FROM meta WHERE key = 'last_dream_rowid'), 0))", [], |r| r.get(0)).unwrap_or(0);
        (n, last)
    }

    /// "Dreaming" (vault: "Layered Deduction-Induction Dream Specialists"): deductions (what
    /// follows from ≥1 facts) and patterns (≥2 facts) become `pattern` memories that cite their
    /// sources. Create-only: nothing is deleted or superseded here. Every cited id must resolve
    /// to an active fact, else the conclusion is dropped (grounded provenance).
    pub async fn dream(&self, gemini: &JsonLlm<'_>) -> anyhow::Result<String> {
        let facts: Vec<(i64, String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare("SELECT id, text, ts FROM memories WHERE kind = 'fact' AND valid_to IS NULL ORDER BY ts DESC LIMIT 150")?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
        };
        let max_rowid: i64 = self.db.lock().unwrap().query_row("SELECT coalesce(max(id), 0) FROM memories", [], |r| r.get(0))?;
        if facts.len() < 5 {
            return Ok("too few facts to dream".into());
        }
        let listing = facts
            .iter()
            .map(|(id, t, ts)| format!("#{id} ({}): {t}", Local.timestamp_opt(*ts, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        let r = gemini.call(&DREAM.replace("{facts}", &listing)).await?;
        let ids: std::collections::HashSet<i64> = facts.iter().map(|f| f.0).collect();
        let mut keep: Vec<(String, Vec<i64>)> = Vec::new();
        let mut dropped = 0;
        for (key, min_src) in [("deductions", 1usize), ("patterns", 2usize)] {
            for c in r[key].as_array().into_iter().flatten() {
                let Some(text) = c["text"].as_str().filter(|t| !t.trim().is_empty()) else { continue };
                let src: Vec<i64> = c["sources"].as_array().into_iter().flatten().filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim_start_matches('#').parse().ok()))).filter(|id| ids.contains(id)).collect();
                if src.len() < min_src {
                    dropped += 1;
                    continue;
                }
                keep.push((text.to_string(), src));
            }
        }
        let mut added = 0;
        if !keep.is_empty() {
            let vecs = self.embed(&keep.iter().map(|k| k.0.clone()).collect::<Vec<_>>(), "doc").await?;
            let mut new_ids = Vec::new();
            {
                let ix = self.index.read().unwrap();
                let db = self.db.lock().unwrap();
                for ((text, src), v) in keep.iter().zip(&vecs) {
                    // Already concluded before? Reinforce instead of duplicating.
                    let mut dup = None;
                    if let Ok((_, cand)) = ix.search_with_allowlist(v, 3, None) {
                        for id in cand {
                            if let Ok((b, k)) = db.query_row("SELECT embedding, kind FROM memories WHERE id = ?1 AND valid_to IS NULL", [id as i64], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))) {
                                if dot(v, &from_blob(&b)) >= 0.92 && (k == "pattern" || k == "fact") {
                                    dup = Some(id as i64);
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(id) = dup {
                        db.execute("UPDATE memories SET times_seen = times_seen + 1 WHERE id = ?1", [id])?;
                        continue;
                    }
                    db.execute(
                        "INSERT INTO memories (kind, text, ts, session_id, embed_model, embedding, sources) VALUES ('pattern', ?1, ?2, 'dream', ?3, ?4, ?5)",
                        params![text, now(), self.model, to_blob(v), src.iter().map(i64::to_string).collect::<Vec<_>>().join(",")],
                    )?;
                    new_ids.push((db.last_insert_rowid() as u64, v.clone()));
                    added += 1;
                }
            }
            if !new_ids.is_empty() {
                let mut ix = self.index.write().unwrap();
                let ids: Vec<u64> = new_ids.iter().map(|a| a.0).collect();
                let flat: Vec<f32> = new_ids.iter().flat_map(|a| a.1.iter().copied()).collect();
                ix.add_with_ids(&flat, &ids).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
        }
        {
            let db = self.db.lock().unwrap();
            db.execute("INSERT INTO meta (key, value) VALUES ('last_dream', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1", [now().to_string()])?;
            db.execute("INSERT INTO meta (key, value) VALUES ('last_dream_rowid', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1", [max_rowid.to_string()])?;
        }
        Ok(format!("dream: {added} new conclusions, {} reinforced/duplicate, {dropped} dropped (ungrounded)", keep.len() - added))
    }

    pub fn task_start(&self, kind: &str, input: &str) -> i64 {
        let db = self.db.lock().unwrap();
        let t = now();
        let _ = db.execute(
            "INSERT INTO tasks (kind, status, input, created, updated) VALUES (?1, 'running', ?2, ?3, ?3)",
            params![kind, input, t],
        );
        db.last_insert_rowid()
    }

    pub fn task_finish(&self, id: i64, status: &str, summary: &str, artifact_dir: Option<&Path>) {
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE tasks SET status = ?1, summary = ?2, artifact_dir = ?3, updated = ?4 WHERE id = ?5",
            params![status, summary, artifact_dir.map(|p| p.display().to_string()), now(), id],
        );
    }

    pub fn data_dir(&self) -> &Path {
        &self.cfg.data_dir
    }
}

/// Minimal non-streaming Gemini JSON call for background jobs.
/// JSON calls for memory work (extraction, card, dreams) on the configured provider.
pub struct JsonLlm<'a> {
    pub app: &'a crate::AppState,
    pub provider: crate::llm::Provider,
}

impl JsonLlm<'_> {
    pub fn new(app: &crate::AppState) -> JsonLlm<'_> {
        JsonLlm { app, provider: app.cfg.provider }
    }

    pub async fn call(&self, prompt: &str) -> anyhow::Result<Value> {
        // Room for the reasoning MiniMax / GPT-5.x always do before the JSON.
        let (text, _) = crate::llm::complete(self.app, self.provider, "Reply ONLY with the JSON object asked for.", prompt, true, 6000).await?;
        serde_json::from_str(&text).context("extraction json")
    }
}

const EXTRACT: &str = "You maintain the long-term memory of Sam, a voice companion, about the user.
Date/time this conversation started: {ts}.
Known active facts about the user (#id: text):
{known}

New conversation:
{conv}

Tasks:
1. Extract durable facts about the user from this conversation: who they are, people and pets in their life, preferences, plans, health, work, places, notable events. Third person (use their name if known, else \"The user\"), one fact per item, specific, under 25 words, with absolute dates when timing matters. Skip small talk, Sam's own opinions and trivia. Never store anything they asked to keep secret.
2. For each extracted fact decide against the known facts: \"add\" (new), \"update\" (it changes or replaces a known fact: put that fact's #id in \"replaces\"), or \"same\" (already known: put that known fact's #id in \"replaces\" too, so it counts as mentioned again).
3. Write a 2-3 sentence episodic summary of this conversation (what was discussed, their mood).
4. Open threads: things in their life that are still pending and that a friend would ask about later: a result they're waiting for, an interview, an exam, an appointment, a trip, a hard conversation, a presentation, someone's surgery or baby. Each with the day it happens or is due as an absolute date (YYYY-MM-DD) when one was said or clearly implied (\"next Monday\" from the date above), else null. Short (under 12 words), from Sam's view: \"Lena's design review at Helio\", \"hearing back from Studio Nord\". Not routine plans, not tasks or reminders Sam was asked to do, nothing already among the known open threads.
Known open threads (#id: text, due):
{threads}
5. List the #ids of known open threads this conversation closed (they said how it went, it was cancelled, or it's no longer pending).
Return ONLY JSON: {\"facts\":[{\"op\":\"add|update|same\",\"text\":\"...\",\"replaces\":\"#id or null\"}],\"summary\":\"...\",\"threads\":[{\"text\":\"...\",\"due\":\"YYYY-MM-DD or null\"}],\"closed\":[\"#id\"]}";

/// Open threads (THREAD in the turn context): pending things in their life Sam asks about once.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Thread {
    pub id: i64,
    pub text: String,
    pub due: Option<chrono::NaiveDate>,
    pub created: i64,
    pub status: String,
}

/// A dated thread is raised from its day until this many days after it.
const THREAD_LATE_DAYS: i64 = 5;

/// Which open thread to bring up now: one whose day has come (today, or up to
/// THREAD_LATE_DAYS ago, the most recent first); else an undated one at least two days old and
/// under three weeks (how did it go?). Offered threads are never raised again. A thread made
/// today is never raised the same day unless its day is today (it was just talked about).
pub fn pick_thread(threads: &[Thread], today: chrono::NaiveDate, now: i64) -> Option<Thread> {
    let open = threads.iter().filter(|t| t.status == "open");
    let mut dated: Vec<&Thread> = open.clone().filter(|t| t.due.is_some_and(|d| d <= today && (today - d).num_days() <= THREAD_LATE_DAYS)).collect();
    dated.sort_by_key(|t| std::cmp::Reverse(t.due));
    if let Some(t) = dated.first().filter(|t| t.due != Some(today) || now - t.created > 6 * 3600) {
        return Some((*t).clone());
    }
    open.filter(|t| t.due.is_none() && now - t.created >= 2 * 86_400 && now - t.created <= 21 * 86_400).max_by_key(|t| t.created).cloned()
}

impl Thread {
    /// "Lena's design review at Helio (today)" / "(yesterday)" / "(on Monday)" / "(you talked
    /// about it on Tuesday)".
    pub fn spoken(&self, today: chrono::NaiveDate) -> String {
        let when = match self.due {
            Some(d) if d == today => "today".to_string(),
            Some(d) if (today - d).num_days() == 1 => "yesterday".to_string(),
            Some(d) => format!("on {}", d.format("%A")),
            None => {
                let said = Local.timestamp_opt(self.created, 0).single().map(|c| c.date_naive()).unwrap_or(today);
                match (today - said).num_days() {
                    n if n < 7 => format!("they mentioned it on {}", said.format("%A")),
                    n if n < 14 => "they mentioned it last week".to_string(),
                    n => format!("they mentioned it {} weeks ago", n / 7),
                }
            }
        };
        format!("{} ({when})", self.text)
    }
}

const CARD_PREFIXES: &[&str] = &["IDENTITY:", "ATTRIBUTE:", "RELATIONSHIP:", "INSTRUCTION:", "TASTE:", "HABIT:", "DATE:", "PSYCHE:"];

const CARD: &str = "You maintain a short profile card about the user of Sam, a voice companion.
Current card:
{card}

Active facts, most often mentioned first ((n x) = times mentioned):
{facts}

Write the COMPLETE new card: at most 40 short lines, each starting with exactly one prefix:
IDENTITY: (name, age, where they live, work), ATTRIBUTE: (stable traits, health), RELATIONSHIP: (people and pets in their life, with names),
INSTRUCTION: (how they want Sam to behave), TASTE: (food, music, places, activities, humour they like or dislike), HABIT: (routines and how they drift: sleep, sport, screen time),
DATE: (birthdays and anniversaries of people in their life, with the day), PSYCHE: (a hypothesis about how they tick, e.g. 'wants to be heard before advice', with a confidence 0-1 in brackets; only when several facts point the same way).
Only stable, still-true information; no one-off events, no secrets, no speculation. Keep what's still true from the current card.
Return ONLY JSON {\"lines\": [\"IDENTITY: ...\", ...]}";

const DREAM: &str = "You reflect on what a voice companion knows about its user, to understand them better.
Active facts (#id (date): text):
{facts}

1. deductions: conclusions that follow with near certainty from one or more facts (e.g. ages from birth years, what a plan implies).
2. patterns: recurring tendencies supported by at least TWO different facts (habits, values, what energizes or stresses them).
Each item: one sentence in third person, under 30 words, citing the #ids it rests on. Only what the facts support; no guesses about secrets or health beyond the facts. At most 6 of each.
Return ONLY JSON {\"deductions\": [{\"text\": \"...\", \"sources\": [12, 34]}], \"patterns\": [{\"text\": \"...\", \"sources\": [5, 9]}]}";

#[derive(Clone, Copy, Debug)]
enum Window {
    None,
    Range(i64, i64),
    Earliest,
    /// "lately", "recently", "these days": no filter, stronger recency.
    Recent,
}

/// Explicit time references in the query become a SQL time filter (lab: +7 pts recall).
fn time_window(q: &str) -> Window {
    let q = q.to_lowercase();
    let midnight = |days_back: i64| {
        let d = Local::now().date_naive() - chrono::Duration::days(days_back);
        Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).single().map(|t| t.timestamp()).unwrap_or(0)
    };
    if q.contains("yesterday") {
        Window::Range(midnight(1), midnight(0))
    } else if ["earlier today", "this morning", "told you today", "asked you today", "talked about today"].iter().any(|p| q.contains(p)) {
        Window::Range(midnight(0), midnight(-1))
    } else if q.contains("last week") {
        let back = 7 + Local::now().weekday().num_days_from_monday() as i64;
        Window::Range(midnight(back), midnight(back - 7))
    } else if ["first chat", "first time we", "first conversation", "very first"].iter().any(|p| q.contains(p)) {
        Window::Earliest
    } else if ["lately", "recently", "these days", "new in my life", "what's new", "been up to"].iter().any(|p| q.contains(p)) {
        Window::Recent
    } else {
        Window::None
    }
}

fn ago(secs: i64) -> String {
    let d = secs as f64 / 86_400.0;
    if d < 0.75 {
        "today".into()
    } else if d < 1.75 {
        "yesterday".into()
    } else if d < 13.5 {
        format!("{} days ago", d.round())
    } else if d < 60.0 {
        format!("{} weeks ago", (d / 7.0).round())
    } else {
        format!("{} months ago", (d / 30.0).round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert!(matches!(time_window("What did we talk about yesterday?"), Window::Range(..)));
        assert!(matches!(time_window("what did I say in our very first chat"), Window::Earliest));
        assert!(matches!(time_window("Where do I live?"), Window::None));
        assert!(matches!(time_window("What's been going on with me lately?"), Window::Recent));
        assert!(matches!(time_window("What did I ask you earlier today?"), Window::Range(..)));
        if let Window::Range(a, b) = time_window("last week?") {
            assert_eq!(b - a, 7 * 86_400);
        }
    }

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.25f32, -1.5, 3.0];
        assert_eq!(from_blob(&to_blob(&v)), v);
    }
}

#[cfg(test)]
mod thread_tests {
    use super::*;
    fn t(id: i64, due: Option<&str>, created_days_ago: i64, status: &str, now: i64) -> Thread {
        Thread { id, text: format!("thread {id}"), due: due.map(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap()), created: now - created_days_ago * 86_400, status: status.into() }
    }

    #[test]
    fn picks_due_then_undated() {
        let now = 1_800_000_000;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        // Due in the future: not yet. Due 2 days ago: yes, before an undated one.
        let ts = vec![t(1, Some("2026-10-20"), 3, "open", now), t(2, Some("2026-10-13"), 4, "open", now), t(3, None, 3, "open", now)];
        assert_eq!(pick_thread(&ts, today, now).map(|x| x.id), Some(2));
        // Too late (more than THREAD_LATE_DAYS after its day) and offered ones are skipped.
        let ts = vec![t(1, Some("2026-10-01"), 20, "open", now), t(2, Some("2026-10-14"), 4, "offered", now), t(3, None, 3, "open", now)];
        assert_eq!(pick_thread(&ts, today, now).map(|x| x.id), Some(3));
        // Undated: not the day after it was mentioned, not after three weeks.
        let ts = vec![t(1, None, 1, "open", now), t(2, None, 30, "open", now)];
        assert!(pick_thread(&ts, today, now).is_none());
        // Due today but mentioned an hour ago (same conversation): not yet.
        let mut x = t(1, Some("2026-10-15"), 0, "open", now);
        x.created = now - 3600;
        assert!(pick_thread(&[x], today, now).is_none());
    }

    #[test]
    fn spoken_day() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let mut x = t(1, Some("2026-10-15"), 1, "open", 0);
        assert!(x.spoken(today).ends_with("(today)"));
        x.due = chrono::NaiveDate::from_ymd_opt(2026, 10, 14);
        assert!(x.spoken(today).ends_with("(yesterday)"));
        x.due = chrono::NaiveDate::from_ymd_opt(2026, 10, 12);
        assert!(x.spoken(today).ends_with("(on Monday)"));
    }
}
