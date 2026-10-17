//! Long-term task history: every task record (and every monitor finding) stays searchable after
//! it leaves the recent list. SQLite `data/history.db`: the full record as JSON + a search text,
//! an FTS5 index (BM25) and the text's embedding (mind sidecar, Qwen3-Embedding). Search is
//! hybrid: BM25 top-100 and cosine top-100 over the filtered rows, fused by reciprocal rank
//! (k = 60) — keywords catch names/ids ("Döbling", "B0H6MPHB13"), vectors catch paraphrases
//! ("the flat near the vineyard"). Filters: status, kind, date range; sort: relevance / newest /
//! oldest. Embeddings are filled in the background (`embed_pending`), so a write never waits.

use std::{path::Path, sync::Mutex};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::tasklog::TaskRec;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  created INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  status TEXT NOT NULL,
  kind TEXT NOT NULL,
  title TEXT NOT NULL,
  text TEXT NOT NULL,
  rec TEXT NOT NULL,
  vec BLOB
);
CREATE INDEX IF NOT EXISTS tasks_created ON tasks(created);
CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(id UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2');
";

pub struct History {
    db: Mutex<Connection>,
    http: reqwest::Client,
    embed_url: String,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Query {
    #[serde(default)]
    pub q: String,
    /// Comma-separated statuses (done,failed,…); empty = all.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub kind: String,
    /// Unix ms bounds (inclusive).
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// relevance | newest | oldest (default: relevance with a query, newest without).
    #[serde(default)]
    pub sort: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub id: String,
    pub created: i64,
    pub updated: i64,
    pub status: String,
    pub kind: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

/// What a record is found by: the request, the outcome, step summaries, notes, report title.
fn search_text(r: &TaskRec) -> String {
    let mut t = format!("{}\n{}\n{}", r.request, r.result, r.current);
    for s in &r.steps {
        t.push('\n');
        t.push_str(&s.summary.chars().take(600).collect::<String>());
    }
    for n in &r.notes {
        t.push('\n');
        t.push_str(n);
    }
    if let Some(rep) = &r.report {
        t.push_str(&format!("\n{}\n{}", rep["title"].as_str().unwrap_or(""), rep["summary"].as_str().unwrap_or("")));
    }
    t.chars().take(6000).collect()
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// FTS5 query from free text: each word as a prefix term, OR-ed (BM25 ranks the overlap).
fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(|w| format!("\"{}\"*", w.to_lowercase())).collect::<Vec<_>>().join(" OR ")
}

impl History {
    pub fn open(data: &Path, embed_url: &str, http: reqwest::Client) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("history.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA).context("history schema")?;
        Ok(Self { db: Mutex::new(db), http, embed_url: embed_url.to_string() })
    }

    /// Insert or refresh a record. The embedding is reset only when the search text changed.
    pub fn upsert(&self, r: &TaskRec) {
        let text = search_text(r);
        let title: String = if r.title.trim().is_empty() { r.request.chars().take(200).collect() } else { r.title.clone() };
        let rec = serde_json::to_string(r).unwrap_or_default();
        let db = self.db.lock().unwrap();
        let old: Option<String> = db.query_row("SELECT text FROM tasks WHERE id = ?1", [&r.id], |x| x.get(0)).optional().ok().flatten();
        let res = if old.as_deref() == Some(text.as_str()) {
            db.execute("UPDATE tasks SET updated = ?2, status = ?3, rec = ?4 WHERE id = ?1", params![r.id, r.updated, r.status, rec])
        } else {
            let _ = db.execute("DELETE FROM tasks_fts WHERE id = ?1", [&r.id]);
            let _ = db.execute("INSERT INTO tasks_fts (id, text) VALUES (?1, ?2)", params![r.id, text]);
            db.execute(
                "INSERT INTO tasks (id, created, updated, status, kind, title, text, rec, vec) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
                 ON CONFLICT(id) DO UPDATE SET updated = ?3, status = ?4, kind = ?5, title = ?6, text = ?7, rec = ?8, vec = NULL",
                params![r.id, r.created, r.updated, r.status, r.kind, title, text, rec],
            )
        };
        if let Err(e) = res {
            tracing::warn!("history upsert {}: {e}", r.id);
        }
    }

    pub fn get(&self, id: &str) -> Option<TaskRec> {
        let db = self.db.lock().unwrap();
        let rec: Option<String> = db.query_row("SELECT rec FROM tasks WHERE id = ?1", [id], |x| x.get(0)).optional().ok().flatten();
        rec.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Ids of finished tasks last updated before `cutoff` (ms), for the retention cleanup.
    pub fn finished_before(&self, cutoff: i64) -> Vec<String> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT id FROM tasks WHERE updated < ?1 AND status NOT IN ('running', 'queued', 'needs_action')") else { return vec![] };
        st.query_map([cutoff], |x| x.get::<_, String>(0)).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Automation and monitor runs before `cutoff` leave the history (their outcome lives in
    /// automations.db / monitors.db). Returns how many were removed.
    pub fn delete_runs_before(&self, cutoff: i64) -> usize {
        let db = self.db.lock().unwrap();
        let filt = "updated < ?1 AND (kind IN ('automation', 'monitor') OR rec LIKE '%\"session\":\"automation\"%' OR rec LIKE '%\"session\":\"monitor\"%')";
        let _ = db.execute(&format!("DELETE FROM tasks_fts WHERE id IN (SELECT id FROM tasks WHERE {filt})"), [cutoff]);
        db.execute(&format!("DELETE FROM tasks WHERE {filt}"), [cutoff]).unwrap_or(0)
    }

    pub fn delete(&self, id: &str) -> bool {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM tasks_fts WHERE id = ?1", [id]);
        db.execute("DELETE FROM tasks WHERE id = ?1", [id]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn count(&self) -> usize {
        self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM tasks", [], |x| x.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    /// Embeds rows without a vector (finished ones first), a batch at a time.
    pub async fn embed_pending(&self) -> anyhow::Result<usize> {
        let rows: Vec<(String, String)> = {
            let db = self.db.lock().unwrap();
            let mut st = db.prepare("SELECT id, text FROM tasks WHERE vec IS NULL ORDER BY (status IN ('running','queued','needs_action')), updated DESC LIMIT 32")?;
            st.query_map([], |x| Ok((x.get(0)?, x.get(1)?)))?.flatten().collect()
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = rows.iter().map(|r| r.1.chars().take(2000).collect()).collect();
        let vecs = crate::mind::embed(&self.http, &self.embed_url, &texts, "document").await?;
        let db = self.db.lock().unwrap();
        for ((id, _), v) in rows.iter().zip(vecs) {
            db.execute("UPDATE tasks SET vec = ?2 WHERE id = ?1", params![id, to_blob(&v)])?;
        }
        Ok(rows.len())
    }

    /// Hybrid search with filters. Without a query: the filtered list in date order.
    pub async fn search(&self, q: &Query) -> anyhow::Result<(Vec<Hit>, usize)> {
        let limit = q.limit.unwrap_or(50).min(200);
        let offset = q.offset.unwrap_or(0);
        let statuses: Vec<String> = q.status.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let kinds: Vec<String> = q.kind.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let mut filt = String::from("1=1");
        if !statuses.is_empty() {
            filt.push_str(&format!(" AND t.status IN ({})", statuses.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(",")));
        }
        if !kinds.is_empty() {
            filt.push_str(&format!(" AND t.kind IN ({})", kinds.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(",")));
        }
        if let Some(f) = q.from {
            filt.push_str(&format!(" AND t.created >= {f}"));
        }
        if let Some(t) = q.to {
            filt.push_str(&format!(" AND t.created <= {t}"));
        }
        let query = q.q.trim();
        let sort = if q.sort.is_empty() { if query.is_empty() { "newest" } else { "relevance" } } else { q.sort.as_str() };
        // Query vector first (no lock held across await); search degrades to keywords without it.
        let qvec = if query.is_empty() { None } else { crate::mind::embed(&self.http, &self.embed_url, &[query.to_string()], "query").await.ok().and_then(|mut v| v.pop()) };
        let db = self.db.lock().unwrap();
        let row = |x: &rusqlite::Row| -> rusqlite::Result<Hit> {
            let text: String = x.get(6)?;
            Ok(Hit { id: x.get(0)?, created: x.get(1)?, updated: x.get(2)?, status: x.get(3)?, kind: x.get(4)?, title: x.get(5)?, snippet: text.lines().skip(1).find(|l| !l.trim().is_empty()).unwrap_or("").replace("**", "").trim_start_matches(['#', '-', ' ']).chars().take(160).collect(), score: 0.0 })
        };
        if query.is_empty() {
            let total: i64 = db.query_row(&format!("SELECT COUNT(*) FROM tasks t WHERE {filt}"), [], |x| x.get(0))?;
            let order = if sort == "oldest" { "ASC" } else { "DESC" };
            let mut st = db.prepare(&format!("SELECT t.id, t.created, t.updated, t.status, t.kind, t.title, t.text FROM tasks t WHERE {filt} ORDER BY t.created {order} LIMIT {limit} OFFSET {offset}"))?;
            let hits = st.query_map([], row)?.flatten().collect();
            return Ok((hits, total as usize));
        }
        // Keyword ranks.
        let mut ranks: std::collections::HashMap<String, f32> = Default::default();
        let fq = fts_query(query);
        if !fq.is_empty() {
            let mut st = db.prepare(&format!("SELECT t.id FROM tasks_fts f JOIN tasks t ON t.id = f.id WHERE tasks_fts MATCH ?1 AND {filt} ORDER BY bm25(tasks_fts) LIMIT 100"))?;
            for (i, id) in st.query_map([&fq], |x| x.get::<_, String>(0))?.flatten().enumerate() {
                *ranks.entry(id).or_default() += 1.0 / (60.0 + i as f32 + 1.0);
            }
        }
        // Vector ranks (brute force over the filtered rows: thousands of tasks, not millions).
        if let Some(qv) = &qvec {
            let mut st = db.prepare(&format!("SELECT t.id, t.vec FROM tasks t WHERE t.vec IS NOT NULL AND {filt}"))?;
            let mut sims: Vec<(String, f32)> = st.query_map([], |x| Ok((x.get::<_, String>(0)?, x.get::<_, Vec<u8>>(1)?)))?.flatten().map(|(id, b)| (id, crate::mind::dot(qv, &from_blob(&b)))).collect();
            sims.sort_by(|a, b| b.1.total_cmp(&a.1));
            // Only reasonably close matches count; an unrelated query must not return everything.
            for (i, (id, s)) in sims.into_iter().take(100).enumerate().filter(|(_, (_, s))| *s >= 0.35) {
                let _ = s;
                *ranks.entry(id).or_default() += 1.0 / (60.0 + i as f32 + 1.0);
            }
        }
        let total = ranks.len();
        let mut ids: Vec<(String, f32)> = ranks.into_iter().collect();
        ids.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut hits: Vec<Hit> = Vec::new();
        let mut st = db.prepare("SELECT id, created, updated, status, kind, title, text FROM tasks WHERE id = ?1")?;
        for (id, score) in &ids {
            if let Ok(mut h) = st.query_row([id], row) {
                h.score = *score;
                hits.push(h);
            }
        }
        match sort {
            "newest" => hits.sort_by(|a, b| b.created.cmp(&a.created)),
            "oldest" => hits.sort_by(|a, b| a.created.cmp(&b.created)),
            _ => {}
        }
        Ok((hits.into_iter().skip(offset).take(limit).collect(), total))
    }

    /// Records from the old JSON files (before the history existed).
    pub fn import_dir(&self, dir: &Path) -> usize {
        let mut n = 0;
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if let Some(r) = std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str::<TaskRec>(&s).ok()) {
                let exists = self.db.lock().unwrap().query_row("SELECT 1 FROM tasks WHERE id = ?1", [&r.id], |_| Ok(())).optional().ok().flatten().is_some();
                if !exists {
                    self.upsert(&r);
                    n += 1;
                }
            }
        }
        n
    }
}

pub fn hits_json(hits: &[Hit], total: usize) -> Value {
    json!({"hits": hits, "total": total})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, req: &str, status: &str, created: i64) -> TaskRec {
        let mut r: TaskRec = serde_json::from_value(json!({"id": id, "session": "s", "kind": "research", "request": req, "context": "", "status": status, "current": "",
            "plan": [], "steps": [], "actions": [], "files": [], "notes": [], "result": "", "tokens": 0, "created": created, "updated": created})).unwrap();
        r.result = format!("result for {req}");
        r
    }

    #[tokio::test]
    async fn keyword_filters_and_sort() {
        let d = std::env::temp_dir().join(format!("hist-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("history.db"));
        // No embedding server: search falls back to keywords only.
        let h = History::open(&d, "http://127.0.0.1:9", reqwest::Client::new()).unwrap();
        h.upsert(&rec("t1", "Mietwohnung in Döbling unter 1200 Euro", "done", 1000));
        h.upsert(&rec("t2", "ThinkPad X1 Carbon auf willhaben", "failed", 2000));
        h.upsert(&rec("t3", "Wohnung Döbling mit Garten", "done", 3000));
        let (hits, total) = h.search(&Query { q: "dobling".into(), ..Default::default() }).await.unwrap();
        assert_eq!(total, 2, "diacritics folded");
        assert!(hits.iter().all(|x| x.id != "t2"));
        let (hits, _) = h.search(&Query { q: "Döbling".into(), sort: "oldest".into(), ..Default::default() }).await.unwrap();
        assert_eq!(hits[0].id, "t1");
        let (hits, total) = h.search(&Query { status: "failed".into(), ..Default::default() }).await.unwrap();
        assert_eq!((total, hits[0].id.as_str()), (1, "t2"));
        let (hits, _) = h.search(&Query { from: Some(1500), to: Some(3500), ..Default::default() }).await.unwrap();
        assert_eq!(hits.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec!["t3", "t2"]);
        assert!(h.delete("t3"));
        assert_eq!(h.count(), 2);
    }
}
