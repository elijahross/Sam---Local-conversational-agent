//! Artifact store: readable names, only what is worth keeping, and a searchable catalog.
//!
//! A deep-research run may check ~700 URLs and use ~12. Only the useful sources are written as
//! files (`sources/NN_<title>__<domain>.md`); every checked URL lands in `checked.jsonl` (no
//! content: status, reason, scores, whether the brief used it). Everything kept is indexed in
//! `catalog.sqlite` (FTS5 over title, domain, query, path) so the agent and the user can find
//! artifacts by name or words later.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{Connection, params};
use serde::Serialize;

pub struct Catalog {
    db: Mutex<Connection>,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub kind: String,
    pub title: String,
    pub url: Option<String>,
    pub query: Option<String>,
    pub path: String,
    pub created: i64,
}

impl Catalog {
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let root = data_dir.join("artifacts");
        std::fs::create_dir_all(&root)?;
        let db = Connection::open(root.join("catalog.sqlite"))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS entries (
               id INTEGER PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL, url TEXT, domain TEXT,
               query TEXT, path TEXT NOT NULL UNIQUE, created INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(title, domain, query, path, content='entries', content_rowid='id');
             CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
               INSERT INTO entries_fts(rowid, title, domain, query, path) VALUES (new.id, new.title, new.domain, new.query, new.path); END;
             CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
               INSERT INTO entries_fts(entries_fts, rowid, title, domain, query, path) VALUES ('delete', old.id, old.title, old.domain, old.query, old.path); END;",
        )?;
        Ok(Self { db: Mutex::new(db), root })
    }

    /// Register a kept file (path relative to the artifacts root).
    pub fn add(&self, kind: &str, title: &str, url: Option<&str>, query: Option<&str>, path: &Path) -> anyhow::Result<()> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        let domain = url.and_then(domain_of);
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO entries (kind, title, url, domain, query, path, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, title, url, domain, query, rel, chrono::Local::now().timestamp()],
        )?;
        Ok(())
    }

    /// Full-text search over names (title, domain, query, path); newest first among equals.
    pub fn search(&self, q: &str, limit: usize) -> anyhow::Result<Vec<Entry>> {
        let terms: Vec<String> = q.split(|c: char| !c.is_alphanumeric()).filter(|t| t.len() > 1).map(|t| format!("\"{t}\"*")).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT e.kind, e.title, e.url, e.query, e.path, e.created FROM entries_fts f JOIN entries e ON e.id = f.rowid
             WHERE entries_fts MATCH ?1 ORDER BY bm25(entries_fts), e.created DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![terms.join(" OR "), limit as i64], |r| {
            Ok(Entry { kind: r.get(0)?, title: r.get(1)?, url: r.get(2)?, query: r.get(3)?, path: r.get(4)?, created: r.get(5)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

pub fn domain_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(|h| h.trim_start_matches("www.").to_string())
}

/// Filesystem-safe, readable slug: lowercase words joined by '-', at most `max` chars.
pub fn slug(s: &str, max: usize) -> String {
    let mut out = String::new();
    for w in s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let w = w.to_lowercase();
        if out.len() + w.len() + 1 > max {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&w);
    }
    if out.is_empty() { "untitled".into() } else { out }
}

/// `research/2026-10-08_why-do-octopuses-have-three-hearts` (suffix -2, -3 if taken).
pub fn run_dir(root: &Path, kind: &str, label: &str) -> PathBuf {
    let base = format!("{}_{}", chrono::Local::now().format("%Y-%m-%d"), slug(label, 60));
    let mut dir = root.join(kind).join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = root.join(kind).join(format!("{base}-{n}"));
        n += 1;
    }
    dir
}

/// `03_why-is-octopus-blood-blue__howstuffworks.com.md`
pub fn source_name(rank: usize, title: &str, url: &str) -> String {
    format!("{rank:02}_{}__{}.md", slug(title, 60), domain_of(url).unwrap_or_else(|| "local".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_search() {
        assert_eq!(slug("Why Is Octopus Blood Blue? | HowStuffWorks", 60), "why-is-octopus-blood-blue-howstuffworks");
        assert_eq!(source_name(3, "Night train Vienna–Venice", "https://www.oebb.at/x"), "03_night-train-vienna-venice__oebb.at.md");
        let dir = std::env::temp_dir().join(format!("cat-test-{}", std::process::id()));
        let c = Catalog::open(&dir).unwrap();
        c.add("source", "Why is octopus blood blue", Some("https://www.nhm.ac.uk/a"), Some("octopus hearts"), &c.root.join("research/x/sources/01_a.md")).unwrap();
        c.add("brief", "Night trains to Venice", None, Some("vienna venice night train"), &c.root.join("research/y/brief.md")).unwrap();
        assert_eq!(c.search("octopus", 5).unwrap().len(), 1);
        assert_eq!(c.search("venice trains", 5).unwrap()[0].kind, "brief");
        assert_eq!(c.search("nhm", 5).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
