//! Cross-source knowledge (knowledge lab): one local index over the user's working world —
//! calendar, reminders, recent mail, notes, files in granted folders, Sam's task history — and
//! answers that connect them, with citations.
//!
//! Lab winner (25 graded questions over a synthetic workspace): hybrid retrieval (FTS5 BM25 +
//! embeddings, reciprocal-rank fusion) top 8 + "records" (sheets / contracts / master files about
//! the entities the question names: where stale values hide) + a computed status digest
//! (overdue / open tasks, unanswered emails, upcoming events — what no search finds) + documents
//! shown oldest → newest with "newer supersedes older": 0.93 of gold facts, 0 stale answers,
//! ~3k tokens per question, one LLM call. Query rewriting, agentic follow-up searches and more
//! documents cost more without helping.
//! Entities: names collected cheaply, clustered by ONE LLM call per batch (aliases, emails,
//! abbreviations; look-alike companies kept apart; a person keeps only emails that fit the name).

use std::{path::Path, sync::Mutex};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AppState, llm};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Doc {
    /// Stable id: "<source>:<native key>" (email uid, file path, event id…).
    pub id: String,
    /// email | note | calendar | task | file | sheet | contract | chat | history
    pub source: String,
    pub title: String,
    /// When it was written / created ('YYYY-MM-DDTHH:MM').
    pub date: String,
    #[serde(default)]
    pub people: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub meta: Value,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS docs (id TEXT PRIMARY KEY, n INTEGER NOT NULL UNIQUE, source TEXT NOT NULL, title TEXT NOT NULL, date TEXT NOT NULL, people TEXT NOT NULL,
  text TEXT NOT NULL, meta TEXT NOT NULL, hash TEXT NOT NULL, vec BLOB, entities_done INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS docs_source ON docs(source, date);
CREATE VIRTUAL TABLE IF NOT EXISTS docs_fts USING fts5(id UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS entities (name TEXT PRIMARY KEY, type TEXT NOT NULL, aliases TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub struct Index {
    db: Mutex<Connection>,
    http: reqwest::Client,
    embed_url: String,
}

fn hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn unblob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// One line per document for the answer prompt (head + useful metadata + capped text).
pub fn doc_text(d: &Doc, chars: usize) -> String {
    let m = &d.meta;
    let extra: serde_json::Map<String, Value> = ["from", "to", "start", "end", "due", "done", "version", "status", "assignee", "path"].iter()
        .filter_map(|k| m.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect();
    format!("[{}] {} · {} · {}{}\n{}", d.id, d.source, d.date, d.title, if extra.is_empty() { String::new() } else { format!(" · {}", Value::Object(extra)) }, d.text.chars().take(chars).collect::<String>())
}

static INDEX: std::sync::OnceLock<Option<std::sync::Arc<Index>>> = std::sync::OnceLock::new();

pub fn index() -> Option<std::sync::Arc<Index>> {
    INDEX.get_or_init(|| Index::open(&crate::thinker::data_dir(), &std::env::var("MIND_URL").unwrap_or_else(|_| "http://127.0.0.1:47802".into()))
        .map_err(|e| tracing::warn!("knowledge index unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

impl Index {
    pub fn open(data: &Path, embed_url: &str) -> anyhow::Result<Self> {
        Self::open_at(&data.join("knowledge.db"), embed_url)
    }

    pub fn open_at(path: &Path, embed_url: &str) -> anyhow::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db), http: reqwest::Client::new(), embed_url: embed_url.into() })
    }

    /// Insert or update; unchanged documents are skipped (hash), changed ones re-embedded.
    pub fn upsert(&self, d: &Doc) -> bool {
        let h = hash(&format!("{}{}{}{}", d.title, d.date, d.text, d.meta));
        let db = self.db.lock().unwrap();
        let old: Option<String> = db.query_row("SELECT hash FROM docs WHERE id = ?1", [&d.id], |x| x.get(0)).optional().ok().flatten();
        if old.as_deref() == Some(h.as_str()) {
            return false;
        }
        let n: i64 = db.query_row("SELECT COALESCE((SELECT n FROM docs WHERE id = ?1), (SELECT COALESCE(MAX(n), 0) + 1 FROM docs))", [&d.id], |x| x.get(0)).unwrap_or(1);
        let _ = db.execute("DELETE FROM docs_fts WHERE id = ?1", [&d.id]);
        // Indexed as the answer prompt shows it (head + metadata + text): lab parity.
        let _ = db.execute("INSERT INTO docs_fts (id, text) VALUES (?1, ?2)", params![d.id, format!("{}\n{}", doc_text(d, 2000), d.people.join(" "))]);
        db.execute("INSERT INTO docs (id, n, source, title, date, people, text, meta, hash, vec, entities_done) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 0)
                    ON CONFLICT(id) DO UPDATE SET source = ?3, title = ?4, date = ?5, people = ?6, text = ?7, meta = ?8, hash = ?9, vec = NULL, entities_done = 0",
            params![d.id, n, d.source, d.title, d.date, serde_json::to_string(&d.people).unwrap_or_default(), d.text, d.meta.to_string(), h]).is_ok()
    }

    pub fn remove(&self, id: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM docs_fts WHERE id = ?1", [id]);
        let _ = db.execute("DELETE FROM docs WHERE id = ?1", [id]);
    }

    fn row(x: &rusqlite::Row) -> rusqlite::Result<Doc> {
        Ok(Doc { id: x.get(0)?, source: x.get(1)?, title: x.get(2)?, date: x.get(3)?, people: serde_json::from_str(&x.get::<_, String>(4)?).unwrap_or_default(),
                 text: x.get(5)?, meta: serde_json::from_str(&x.get::<_, String>(6)?).unwrap_or_default() })
    }

    pub fn get(&self, id: &str) -> Option<Doc> {
        self.db.lock().unwrap().query_row("SELECT id, source, title, date, people, text, meta FROM docs WHERE id = ?1", [id], Self::row).optional().ok().flatten()
    }

    pub fn count(&self) -> usize {
        self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM docs", [], |x| x.get::<_, i64>(0)).unwrap_or(0) as usize
    }

    pub async fn embed_pending(&self) -> anyhow::Result<usize> {
        let rows: Vec<(String, String)> = {
            let db = self.db.lock().unwrap();
            let mut st = db.prepare("SELECT id, source, title, date, people, text, meta FROM docs WHERE vec IS NULL LIMIT 32")?;
            st.query_map([], Self::row)?.flatten().map(|d| (d.id.clone(), doc_text(&d, 2000))).collect()
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = rows.iter().map(|r| r.1.chars().take(2000).collect()).collect();
        let vecs = crate::mind::embed(&self.http, &self.embed_url, &texts, "document").await?;
        let db = self.db.lock().unwrap();
        for ((id, _), v) in rows.iter().zip(vecs) {
            db.execute("UPDATE docs SET vec = ?2 WHERE id = ?1", params![id, blob(&v)])?;
        }
        Ok(rows.len())
    }

    // ----- entities -----

    pub fn entities(&self) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT name, type, aliases FROM entities") else { return vec![] };
        st.query_map([], |x| Ok(json!({"name": x.get::<_, String>(0)?, "type": x.get::<_, String>(1)?, "aliases": serde_json::from_str::<Value>(&x.get::<_, String>(2)?).unwrap_or_default()})))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Names in documents not yet processed → merged into the entity table with ONE LLM call.
    pub async fn update_entities(&self, app: &AppState) -> anyhow::Result<u64> {
        let docs: Vec<(String, String)> = {
            let db = self.db.lock().unwrap();
            let mut st = db.prepare("SELECT id, title || ' ' || people || ' ' || substr(text, 1, 3000) FROM docs WHERE entities_done = 0 LIMIT 200")?;
            st.query_map([], |x| Ok((x.get(0)?, x.get(1)?)))?.flatten().collect()
        };
        if docs.is_empty() {
            return Ok(0);
        }
        let name_re = regex::Regex::new(r"\b[A-ZÄÖÜ][\w&.-]+(?:[ \t]+(?:[A-ZÄÖÜ][\w&.-]+|GmbH|AG|KG|OG)){0,3}|[\w.+-]+@[\w-]+\.[\w.]+|\b[A-Z]{2,4}\b").unwrap();
        let mut cand: std::collections::HashMap<String, usize> = Default::default();
        for (_, t) in &docs {
            for m in name_re.find_iter(t) {
                let s = m.as_str().trim().to_string();
                if s.len() >= 2 {
                    *cand.entry(s).or_default() += 1;
                }
            }
        }
        let mut names: Vec<(String, usize)> = cand.into_iter().filter(|(n, c)| *c >= 2 || n.contains('@')).collect();
        names.sort_by(|a, b| b.1.cmp(&a.1));
        let names: Vec<String> = names.into_iter().take(400).map(|(n, _)| n).collect();
        let known = self.entities();
        let sys = "Group names from a person's work documents into real-world entities (organisations, people, projects). Merge aliases, abbreviations, \
email addresses and spellings of the SAME entity; keep look-alikes that are different entities apart (two companies sharing a word, two people sharing a surname). \
Keep every known entity (add new aliases to it). Ignore generic words. Reply ONLY with JSON {\"entities\": [{\"name\": \"canonical\", \"type\": \"org|person|project\", \"aliases\": [\"...\"]}]}.";
        let (txt, u) = llm::complete_with(app, app.cfg.provider, crate::llm::Tier::Light, sys, &format!("Known entities: {}\nNew names: {}", json!(known), json!(names)), true, 12000).await?;
        let v: Value = serde_json::from_str(&txt)?;
        let fits = |name: &str, mail: &str| {
            let parts: Vec<String> = regex::Regex::new(r"[a-zäöüß]+").unwrap().find_iter(&name.to_lowercase()).map(|m| m.as_str().to_string()).filter(|p| !["dr", "mag", "frau", "herr"].contains(&p.as_str())).collect();
            if parts.len() < 2 {
                return true;
            }
            let local: String = mail.split('@').next().unwrap_or("").to_lowercase().chars().filter(|c| c.is_ascii_alphabetic()).collect();
            let (first, last) = (&parts[0], &parts[parts.len() - 1]);
            local.contains(last.as_str()) && (local.starts_with(first.chars().next().unwrap_or('#')) || local.contains(first.as_str()) || local == *last)
        };
        let db = self.db.lock().unwrap();
        for e in v["entities"].as_array().into_iter().flatten() {
            let name = e["name"].as_str().unwrap_or("").trim().to_string();
            if name.is_empty() {
                continue;
            }
            let ty = e["type"].as_str().unwrap_or("org");
            let aliases: Vec<String> = e["aliases"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(|s| s.trim().to_string()))
                .filter(|a| !a.contains('\n') && (ty != "person" || !a.contains('@') || fits(&name, a))).collect();
            let old: Vec<String> = db.query_row("SELECT aliases FROM entities WHERE lower(name) = lower(?1)", [&name], |x| x.get::<_, String>(0)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            let mut all: Vec<String> = old.into_iter().chain(aliases).collect();
            all.sort();
            all.dedup();
            let _ = db.execute("INSERT INTO entities (name, type, aliases) VALUES (?1, ?2, ?3) ON CONFLICT(name) DO UPDATE SET type = ?2, aliases = ?3", params![name, ty, json!(all).to_string()]);
        }
        for (id, _) in &docs {
            let _ = db.execute("UPDATE docs SET entities_done = 1 WHERE id = ?1", [id]);
        }
        Ok(u.prompt + u.completion)
    }

    /// Entities a text names (canonical name or an alias, whole words).
    fn named(&self, text: &str) -> Vec<Value> {
        let tl = text.to_lowercase();
        self.entities().into_iter().filter(|e| {
            std::iter::once(e["name"].as_str().unwrap_or("").to_string()).chain(e["aliases"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)))
                .filter(|a| a.chars().count() > 1)
                .any(|a| regex::Regex::new(&format!(r"(?:^|[^\w]){}(?:[^\w]|$)", regex::escape(&a.to_lowercase()))).is_ok_and(|r| r.is_match(&tl)))
        }).collect()
    }

    // ----- retrieval -----

    pub async fn retrieve(&self, q: &str, k: usize, records: bool) -> anyhow::Result<Vec<Doc>> {
        let qv = crate::mind::embed(&self.http, &self.embed_url, &[q.to_string()], "query").await.ok().and_then(|mut v| v.pop());
        let mut ranks: std::collections::HashMap<String, f32> = Default::default();
        {
            let db = self.db.lock().unwrap();
            let fq: String = q.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(|w| format!("\"{}\"", w.to_lowercase())).collect::<Vec<_>>().join(" OR ");
            if !fq.is_empty() {
                let mut st = db.prepare("SELECT id FROM docs_fts WHERE docs_fts MATCH ?1 ORDER BY bm25(docs_fts) LIMIT 30")?;
                for (i, id) in st.query_map([&fq], |x| x.get::<_, String>(0))?.flatten().enumerate() {
                    *ranks.entry(id).or_default() += 1.0 / (60.0 + i as f32);
                }
            }
            if let Some(qv) = &qv {
                let mut st = db.prepare("SELECT id, vec FROM docs WHERE vec IS NOT NULL")?;
                let mut sims: Vec<(String, f32)> = st.query_map([], |x| Ok((x.get::<_, String>(0)?, x.get::<_, Vec<u8>>(1)?)))?.flatten().map(|(id, b)| (id, crate::mind::dot(qv, &unblob(&b)))).collect();
                sims.sort_by(|a, b| b.1.total_cmp(&a.1));
                for (i, (id, _)) in sims.into_iter().take(30).enumerate() {
                    *ranks.entry(id).or_default() += 1.0 / (60.0 + i as f32);
                }
            }
        }
        let mut ids: Vec<(String, f32)> = ranks.into_iter().collect();
        ids.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut top: Vec<Doc> = ids.into_iter().take(k).filter_map(|(id, _)| self.get(&id)).collect();
        if records {
            // Records about the entities the question names (stale values hide there).
            let names: Vec<String> = self.named(q).iter().flat_map(|e| std::iter::once(e["name"].as_str().unwrap_or("").to_string()).chain(e["aliases"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_string)))).filter(|a| a.chars().count() > 2).collect();
            if !names.is_empty() {
                let cands: Vec<Doc> = {
                    let db = self.db.lock().unwrap();
                    let mut st = db.prepare("SELECT id, source, title, date, people, text, meta FROM docs WHERE source IN ('sheet', 'contract', 'file') ORDER BY date DESC LIMIT 500")?;
                    st.query_map([], Self::row)?.flatten().collect()
                };
                let have: std::collections::HashSet<String> = top.iter().map(|d| d.id.clone()).collect();
                let recs: Vec<Doc> = cands.into_iter().filter(|d| !have.contains(&d.id)).filter(|d| {
                    let t = format!("{} {}", d.title, d.text).to_lowercase();
                    names.iter().any(|n| t.contains(&n.to_lowercase()))
                }).take(4).collect();
                top.extend(recs);
            }
        }
        Ok(top)
    }

    /// Computed state no search finds: overdue / open tasks, unanswered inbound emails,
    /// upcoming events (from metadata, no LLM).
    pub fn status(&self, now: &str, me: &[String]) -> String {
        let db = self.db.lock().unwrap();
        let all: Vec<Doc> = db.prepare("SELECT id, source, title, date, people, text, meta FROM docs WHERE source IN ('task', 'email', 'calendar')")
            .and_then(|mut st| st.query_map([], Self::row).map(|r| r.flatten().collect())).unwrap_or_default();
        drop(db);
        let today = &now[..now.len().min(10)];
        let mut lines = vec![format!("[status] Workspace status as of {now} (computed from task / email / calendar metadata):")];
        let mut tasks: Vec<&Doc> = all.iter().filter(|d| d.source == "task" && d.meta["done"] != true).collect();
        tasks.sort_by(|a, b| a.meta["due"].as_str().unwrap_or("9999").cmp(b.meta["due"].as_str().unwrap_or("9999")));
        for d in tasks.iter().take(15) {
            let due = d.meta["due"].as_str().unwrap_or("");
            let tag = if !due.is_empty() && &due[..due.len().min(10)] < today { "OVERDUE" } else { "open" };
            lines.push(format!("- task {tag}: {} (due {}{}) [{}]", d.title, if due.is_empty() { "—" } else { due }, d.meta["assignee"].as_str().map(|a| format!(", {a}")).unwrap_or_default(), d.id));
        }
        let mine = |s: &str| me.iter().any(|m| !m.is_empty() && s.to_lowercase().contains(&m.to_lowercase()));
        let mut mails: Vec<&Doc> = all.iter().filter(|d| d.source == "email" && d.meta["replied"] == false && !mine(d.meta["from"].as_str().unwrap_or("")) && !["newsletter", "notice"].contains(&d.meta["category"].as_str().unwrap_or(""))).collect();
        mails.sort_by(|a, b| a.date.cmp(&b.date));
        for d in mails.iter().take(10) {
            lines.push(format!("- email unanswered since {}: {} from {} [{}]", &d.date[..d.date.len().min(10)], d.title, d.meta["from"].as_str().unwrap_or(""), d.id));
        }
        let mut evs: Vec<&Doc> = all.iter().filter(|d| d.source == "calendar" && d.meta["start"].as_str().is_some_and(|s| s >= now) && d.meta["status"] != "cancelled").collect();
        evs.sort_by(|a, b| a.meta["start"].as_str().cmp(&b.meta["start"].as_str()));
        for d in evs.iter().take(10) {
            lines.push(format!("- upcoming: {} at {} [{}]", d.title, d.meta["start"].as_str().unwrap_or(""), d.id));
        }
        lines.join("\n")
    }
}

const ANSWER: &str = "You answer questions about the user's work from their documents (emails, notes, contracts, calendar, tasks, files, sheets, chats). \
Use ONLY the documents. Cite sources inline as [id] after each claim. Newer documents supersede older ones: state the CURRENT state and mention what changed (old → new) when relevant. \
If sources disagree on a current fact, say so explicitly (\"conflict: …\") with both sources — including a record (sheet, master data, contract) that still shows an older value as if current. \
If the documents don't contain the answer, say it isn't in the documents. Calendar: an event happens at its start (its date is when it was created). Tasks: due / done. \
Reply ONLY with JSON {\"answer\": \"...\"}.";

/// The answer + the documents it could cite (for links in the overview).
pub async fn ask(app: &AppState, ix: &Index, question: &str, now: chrono::DateTime<chrono::Local>, me: &[String]) -> anyhow::Result<(String, Vec<Doc>, u64)> {
    let mut docs = ix.retrieve(question, 8, true).await?;
    docs.sort_by(|a, b| a.date.cmp(&b.date));
    let nows = now.format("%Y-%m-%dT%H:%M").to_string();
    let ctx = format!("{}\n\n{}", ix.status(&nows, me), docs.iter().map(|d| doc_text(d, 1200)).collect::<Vec<_>>().join("\n\n"));
    let user = format!("Today is {} ({}).\nQuestion: {question}\n\nDocuments:\n{ctx}", now.format("%Y-%m-%d"), now.format("%A"));
    let (txt, u) = llm::complete(app, app.cfg.provider, ANSWER, &user, true, 6000).await?;
    let v: Value = serde_json::from_str(&txt).context("answer json")?;
    Ok((v["answer"].as_str().unwrap_or("").to_string(), docs, u.prompt + u.completion))
}

// ---------------------------------------------------------------------------------------------
// Ingestion: the lab corpus (tests) and the user's sources (incremental, background).

/// Loads documents from a directory of `<id>.json` files (knowledge lab format).
pub fn ingest_dir(ix: &Index, dir: &Path) -> usize {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Doc>(&s).ok()).filter(|d| ix.upsert(d)).count()
}

const NOTES_RECENT: &str = r#"function run(argv) {
  const N = Application('Notes'); const out = []; const since = new Date(Date.now() - 1000 * 3600 * 24 * Number(argv[0]));
  for (const n of N.notes()) { let m; try { m = n.modificationDate(); } catch (e) { continue; } if (m < since) continue;
    out.push({id: n.id(), title: n.name(), date: m.toISOString(), text: n.plaintext().slice(0, 4000)}); if (out.length >= 150) break; }
  return JSON.stringify(out); }"#;

/// The user's sources → documents (read-only). Skipped entirely in test mode.
pub async fn ingest_live(app: &AppState, ix: &Index) -> anyhow::Result<usize> {
    // Tests (SAM_NO_REAL_APPS=1) never read the user's apps; Sam's own history and granted
    // folders are still indexed, so "the research you did" works there too.
    let real_apps = !std::env::var("SAM_NO_REAL_APPS").is_ok_and(|v| v == "1");
    let mut n = 0;
    let now = chrono::Local::now();
    let fmt = |s: &str| chrono::DateTime::parse_from_rfc3339(s).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_else(|_| s.chars().take(16).collect());
    // Calendar: 30 days back, 30 ahead.
    if let Ok(v) = if real_apps { crate::apps::calendar_events_raw(now - chrono::Duration::days(30), now + chrono::Duration::days(30)).await } else { Err(anyhow::anyhow!("test mode")) } {
        for e in v.as_array().into_iter().flatten() {
            let start = fmt(e["start"].as_str().unwrap_or(""));
            let d = Doc { id: format!("calendar:{}|{}", e["title"].as_str().unwrap_or(""), start), source: "calendar".into(), title: e["title"].as_str().unwrap_or("").into(), date: start.clone(),
                          people: vec![], text: format!("{} {}", e["location"].as_str().unwrap_or(""), e["notes"].as_str().unwrap_or("")), meta: json!({"start": start, "end": fmt(e["end"].as_str().unwrap_or("")), "location": e["location"]}) };
            n += ix.upsert(&d) as usize;
        }
    }
    // Reminders (open).
    if let Ok(v) = if real_apps { crate::apps::reminders("").await } else { Err(anyhow::anyhow!("test mode")) } {
        for r in v.as_array().into_iter().flatten() {
            let due = r["due"].as_str().and_then(|d| chrono::NaiveDateTime::parse_from_str(d, "%a %b %d %Y %H:%M").ok()).map(|d| d.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default();
            let d = Doc { id: format!("task:{}|{}", r["list"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or("")), source: "task".into(), title: r["title"].as_str().unwrap_or("").into(),
                          date: due.clone(), people: vec![], text: String::new(), meta: json!({"due": due, "done": false, "list": r["list"]}) };
            n += ix.upsert(&d) as usize;
        }
    }
    // Mail (IMAP): the last 30 days, bodies capped.
    if real_apps && crate::email::Account::load().is_some() {
        let q: crate::email::Query = serde_json::from_value(json!({"since_days": 30, "limit": 150})).unwrap_or_default();
        if let Ok(ms) = crate::email::search(&q).await {
            for m in ms {
                let id = format!("email:{}", m.uid);
                if ix.get(&id).is_some() {
                    continue;
                }
                let body = crate::email::read(m.uid, None, 4000).await.map(|x| x.text).unwrap_or(m.snippet.clone());
                let d = Doc { id, source: "email".into(), title: m.subject.clone(), date: m.date.clone(), people: vec![m.from.clone()], text: body, meta: json!({"from": m.from, "uid": m.uid}) };
                n += ix.upsert(&d) as usize;
            }
        }
    }
    // Notes: Apple Notes changed in the last 90 days, or the Obsidian vault.
    match crate::notes::backend() {
        crate::notes::Backend::Apple if !real_apps => {}
        crate::notes::Backend::Apple => {
            if let Ok(v) = crate::apps::jxa_pub(NOTES_RECENT, &["90".into()]).await {
                for x in v.as_array().into_iter().flatten() {
                    let d = Doc { id: format!("note:{}", x["id"].as_str().unwrap_or("")), source: "note".into(), title: x["title"].as_str().unwrap_or("").into(), date: fmt(x["date"].as_str().unwrap_or("")),
                                  people: vec![], text: x["text"].as_str().unwrap_or("").into(), meta: json!({}) };
                    n += ix.upsert(&d) as usize;
                }
            }
        }
        crate::notes::Backend::Obsidian(vault) => n += ingest_folder(ix, &vault, "note"),
    }
    // Files in folders the user granted.
    if let Ok(f) = crate::files::Files::open(&crate::thinker::data_dir()) {
        for g in f.grants() {
            n += ingest_folder(ix, &g.path, "file");
        }
    }
    // Sam's own finished tasks (what it researched, decided, sent).
    if let Some(h) = app.tasks.get().and_then(|t| t.log.history.clone()) {
        let (hits, _) = h.search(&crate::history::Query { limit: Some(200), ..Default::default() }).await.map_err(|e| tracing::info!("knowledge: history search failed: {e:#}")).unwrap_or_default();
        tracing::debug!(hits = hits.len(), "knowledge: history records to index");
        for x in hits {
            if let Some(r) = h.get(&x.id) {
                let d = Doc { id: format!("history:{}", r.id), source: "history".into(), title: r.request.chars().take(150).collect(),
                              date: chrono::Local.timestamp_millis_opt(r.created).single().map(|t| t.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default(),
                              people: vec![], text: task_text(&r), meta: json!({"status": r.status, "task": r.id}) };
                n += ix.upsert(&d) as usize;
            }
        }
    }
    Ok(n)
}

use chrono::TimeZone;

/// What a finished task knows: its result, each step's summary and the text of the files the
/// steps wrote (a research brief, a report), so "the research you did on X" finds the names,
/// prices and places in it, not just the one-line summary.
fn task_text(r: &crate::tasklog::TaskRec) -> String {
    let mut t = format!("{}\n{}", r.result, r.steps.iter().map(|s| s.summary.chars().take(400).collect::<String>()).collect::<Vec<_>>().join("\n"));
    for f in r.steps.iter().filter_map(|s| s.file.as_deref()) {
        if f.ends_with(".md") || f.ends_with(".txt") {
            if let Ok(x) = std::fs::read_to_string(f) {
                t.push('\n');
                t.extend(x.chars().take(6000));
            }
        }
    }
    let rep = crate::thinker::data_dir().join("artifacts/reports").join(&r.id).join("report.json");
    if let Ok(x) = std::fs::read_to_string(rep) {
        if let Ok(v) = serde_json::from_str::<Value>(&x) {
            t.push('\n');
            t.push_str(v["summary"].as_str().unwrap_or(""));
            for h in v["highlights"].as_array().into_iter().flatten() {
                t.push_str(&format!("\n- {}: {}", h["title"].as_str().unwrap_or(""), h["text"].as_str().unwrap_or("")));
            }
        }
    }
    t.chars().take(12000).collect()
}

/// Text files of a folder tree (md / txt / csv / json / pdf / docx), recent first, capped.
fn ingest_folder(ix: &Index, root: &Path, source: &str) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with('.') || crate::files::is_denied(&p) {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            seen += 1;
            if seen > 2000 {
                return n;
            }
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            let text = match ext.as_str() {
                "md" | "txt" | "csv" | "json" => std::fs::read_to_string(&p).ok(),
                "pdf" => std::process::Command::new("pdftotext").args(["-l", "8"]).arg(&p).arg("-").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string()),
                "docx" => std::process::Command::new("textutil").args(["-convert", "txt", "-stdout"]).arg(&p).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string()),
                _ => None,
            };
            let Some(text) = text else { continue };
            let modified = e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default();
            let src = if ext == "csv" { "sheet" } else if name.to_lowercase().contains("vertrag") || name.to_lowercase().contains("contract") { "contract" } else { source };
            let d = Doc { id: format!("{source}:{}", p.display()), source: src.into(), title: name, date: modified, people: vec![], text: text.chars().take(6000).collect(), meta: json!({"path": p.display().to_string()}) };
            n += ix.upsert(&d) as usize;
        }
    }
    n
}

static APP: std::sync::OnceLock<std::sync::Arc<AppState>> = std::sync::OnceLock::new();

/// A finished background request (what Sam just researched, decided, drafted) is indexed
/// within seconds, not at the next half-hour pass: "the research you did" must work right away.
/// Several finishes in a row collapse into one pass.
pub fn ingest_soon() {
    static SCHEDULED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(app) = APP.get().cloned() else { tracing::info!("knowledge: indexer not started, finished request not indexed"); return };
    if SCHEDULED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        SCHEDULED.store(false, std::sync::atomic::Ordering::SeqCst);
        let Some(ix) = index() else { return };
        match ingest_live(&app, &ix).await {
            Ok(n) => tracing::info!(n, total = ix.count(), "knowledge: documents indexed after a finished request"),
            Err(e) => tracing::info!("knowledge ingest: {e:#}"),
        }
        while let Ok(k) = ix.embed_pending().await {
            if k == 0 {
                break;
            }
        }
    });
}

/// Background indexing: sources every 30 min (incremental), embeddings and entities as needed.
pub fn spawn(app: std::sync::Arc<AppState>) {
    let _ = APP.set(app.clone());
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        loop {
            if let Some(ix) = index() {
                match ingest_live(&app, &ix).await {
                    Ok(n) if n > 0 => tracing::info!(n, "knowledge: documents indexed"),
                    Ok(_) => {}
                    Err(e) => tracing::info!("knowledge ingest: {e:#}"),
                }
                while let Ok(k) = ix.embed_pending().await {
                    if k == 0 {
                        break;
                    }
                }
                if let Err(e) = ix.update_entities(&app).await {
                    tracing::info!("knowledge entities: {e:#}");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_status_and_records() {
        let d = std::env::temp_dir().join(format!("kn-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&d);
        let ix = Index::open_at(&d, "http://127.0.0.1:9").unwrap();
        let doc = |id: &str, src: &str, title: &str, date: &str, meta: Value| Doc { id: id.into(), source: src.into(), title: title.into(), date: date.into(), people: vec![], text: format!("{title} Brenner Logistik"), meta };
        assert!(ix.upsert(&doc("t1", "task", "Send contract v2 to legal", "2026-10-01T09:00", json!({"due": "2026-10-08", "done": false}))));
        assert!(!ix.upsert(&doc("t1", "task", "Send contract v2 to legal", "2026-10-01T09:00", json!({"due": "2026-10-08", "done": false}))), "unchanged = skipped");
        ix.upsert(&doc("e1", "email", "Signature status?", "2026-10-06T10:00", json!({"from": "treiter@brenner-logistik.at", "replied": false})));
        ix.upsert(&doc("c1", "calendar", "Supplier meeting", "2026-09-30T08:00", json!({"start": "2026-10-14T10:00"})));
        ix.upsert(&doc("s1", "sheet", "Purchasing prices", "2026-09-30T08:00", json!({})));
        let st = ix.status("2026-10-10T09:00", &["elena".into()]);
        assert!(st.contains("OVERDUE: Send contract v2") && st.contains("unanswered since 2026-10-06") && st.contains("upcoming: Supplier meeting"));
        assert_eq!(ix.count(), 4);
    }
}
