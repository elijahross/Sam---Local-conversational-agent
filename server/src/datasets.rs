//! Datasets: long-running collections that grow and get enriched every day, kept as a table the
//! user can open — `data/datasets/<name>/<name>.xlsx` (sheets: data, changes), `.csv` and `.md`.
//!
//! A dataset has a source and a key column. Every run collects again (new rows are added, known
//! rows updated: last seen, changed values recorded with old → new), then enriches a slice of
//! the existing rows (detail pages, refreshed profiles) under the sites' budgets, and rewrites the
//! files. Sources:
//! - scrape: a listing / search page (Sam's scrape tool, the recipe is cached → ~0 tokens later),
//!   optional `detail_fields` read from each row's own page, a slice per run;
//! - creators: TikTok / X / Instagram creator search (new creators each day) + refreshed stats;
//! - social: what people say on X / TikTok, the posts accumulate, one stance row per day;
//! - market: sold-price statistics per item, one row per item and day (a price history);
//! - monitor: the listings of a monitor or price tracker as a table.
//! Storage `data/datasets.db`; the scheduler runs due datasets in the server process.

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::AppState;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS datasets (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, request TEXT NOT NULL, source TEXT NOT NULL, spec TEXT NOT NULL, key TEXT NOT NULL,
  every_min INTEGER NOT NULL, active INTEGER NOT NULL DEFAULT 1, created INTEGER NOT NULL, next_run INTEGER NOT NULL,
  last_run INTEGER, runs INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_note TEXT
);
CREATE TABLE IF NOT EXISTS rows (dataset TEXT NOT NULL, key TEXT NOT NULL, data TEXT NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, updated INTEGER NOT NULL, seen_runs INTEGER NOT NULL DEFAULT 1, enriched INTEGER, PRIMARY KEY (dataset, key));
CREATE TABLE IF NOT EXISTS changes (dataset TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL, field TEXT NOT NULL, old TEXT, new TEXT);
";

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

fn slug(s: &str) -> String {
    let s: String = s.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    s.split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-").chars().take(60).collect()
}

#[derive(Debug, Clone)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    pub request: String,
    pub source: String,
    pub spec: Value,
    pub key: String,
    pub every_min: i64,
    pub active: bool,
    pub runs: i64,
    pub last_run: Option<i64>,
    pub last_error: Option<String>,
    pub last_note: Option<String>,
}

impl Dataset {
    pub fn dir(&self) -> PathBuf {
        crate::thinker::data_dir().join("datasets").join(slug(&self.name))
    }
    pub fn files(&self) -> Vec<PathBuf> {
        let s = slug(&self.name);
        ["xlsx", "csv", "md"].iter().map(|e| self.dir().join(format!("{s}.{e}"))).collect()
    }
}

pub struct Store {
    db: Mutex<Connection>,
}

pub struct Merge {
    pub new: usize,
    pub updated: usize,
    pub total: usize,
}

impl Store {
    pub fn open(data: &std::path::Path) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("datasets.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn create(&self, name: &str, request: &str, source: &str, spec: &Value, key: &str, every_min: i64) -> anyhow::Result<String> {
        let id = format!("d{}", now());
        let t = now();
        self.db.lock().unwrap().execute(
            "INSERT INTO datasets (id, name, request, source, spec, key, every_min, created, next_run) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, name, request, source, spec.to_string(), key, every_min.max(60), t, t + every_min.max(60) * 60_000],
        )?;
        Ok(id)
    }

    fn row(x: &rusqlite::Row) -> rusqlite::Result<Dataset> {
        Ok(Dataset {
            id: x.get("id")?, name: x.get("name")?, request: x.get("request")?, source: x.get("source")?,
            spec: serde_json::from_str(&x.get::<_, String>("spec")?).unwrap_or(Value::Null), key: x.get("key")?, every_min: x.get("every_min")?,
            active: x.get::<_, i64>("active")? == 1, runs: x.get("runs")?, last_run: x.get("last_run")?, last_error: x.get("last_error")?, last_note: x.get("last_note")?,
        })
    }

    pub fn get(&self, id: &str) -> Option<Dataset> {
        self.db.lock().unwrap().query_row("SELECT * FROM datasets WHERE id = ?1", [id], Self::row).optional().ok().flatten()
    }

    pub fn list(&self) -> Vec<Dataset> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM datasets ORDER BY created DESC") else { return vec![] };
        st.query_map([], Self::row).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn due(&self) -> Vec<Dataset> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM datasets WHERE active = 1 AND next_run <= ?1 ORDER BY next_run") else { return vec![] };
        st.query_map([now()], Self::row).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// The dataset the user means: its id, else the best word overlap with its name / request.
    pub fn find(&self, words: &str) -> Option<Dataset> {
        let all = self.list();
        if let Some(d) = all.iter().find(|d| d.id == words.trim()) {
            return Some(d.clone());
        }
        let toks = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_string).collect::<std::collections::HashSet<_>>();
        let want = toks(words);
        all.iter().map(|d| (toks(&format!("{} {}", d.name, d.request)).intersection(&want).count(), d)).filter(|(n, _)| *n > 0).max_by_key(|(n, _)| *n).map(|(_, d)| d.clone())
            .or_else(|| (all.len() == 1).then(|| all[0].clone()))
    }

    pub fn set_active(&self, id: &str, on: bool) -> bool {
        self.db.lock().unwrap().execute("UPDATE datasets SET active = ?2, next_run = CASE WHEN ?2 = 1 THEN ?3 ELSE next_run END WHERE id = ?1", params![id, on as i64, now()]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn run_now(&self, id: &str) -> bool {
        self.db.lock().unwrap().execute("UPDATE datasets SET next_run = ?2 WHERE id = ?1", params![id, now()]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn delete(&self, id: &str) -> bool {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM rows WHERE dataset = ?1", [id]);
        let _ = db.execute("DELETE FROM changes WHERE dataset = ?1", [id]);
        db.execute("DELETE FROM datasets WHERE id = ?1", [id]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn finish(&self, id: &str, note: &str, error: Option<&str>) {
        let db = self.db.lock().unwrap();
        let every: i64 = db.query_row("SELECT every_min FROM datasets WHERE id = ?1", [id], |x| x.get(0)).unwrap_or(1440);
        let t = now();
        // A failed run retries in 3 h, not a day later.
        let next = t + if error.is_some() { 180 * 60_000 } else { every * 60_000 };
        let _ = db.execute("UPDATE datasets SET last_run = ?2, next_run = ?3, runs = runs + 1, last_error = ?4, last_note = ?5 WHERE id = ?1", params![id, t, next, error, note]);
    }

    /// Adds / updates rows by key; changed values are recorded (old → new).
    pub fn merge(&self, id: &str, key: &str, items: &[Value], enriched: bool) -> Merge {
        let mut db = self.db.lock().unwrap();
        let t = now();
        let mut m = Merge { new: 0, updated: 0, total: 0 };
        let Ok(tx) = db.transaction() else { return m };
        for it in items {
            let k = match &it[key] {
                Value::String(s) => s.trim().to_string(),
                Value::Null => continue,
                v => v.to_string(),
            };
            if k.is_empty() {
                continue;
            }
            let old: Option<String> = tx.query_row("SELECT data FROM rows WHERE dataset = ?1 AND key = ?2", params![id, k], |x| x.get(0)).optional().ok().flatten();
            match old {
                None => {
                    let _ = tx.execute("INSERT INTO rows (dataset, key, data, first_seen, last_seen, updated, enriched) VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?5)", params![id, k, it.to_string(), t, enriched.then_some(t)]);
                    m.new += 1;
                }
                Some(o) => {
                    let mut cur: Map<String, Value> = serde_json::from_str(&o).unwrap_or_default();
                    let mut changed = false;
                    for (f, v) in it.as_object().into_iter().flatten() {
                        if v.is_null() || v.as_str().is_some_and(|s| s.is_empty()) {
                            continue; // a weaker run never blanks a value
                        }
                        let before = cur.get(f).cloned().unwrap_or(Value::Null);
                        if &before != v {
                            if !before.is_null() {
                                let s = |x: &Value| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string());
                                let _ = tx.execute("INSERT INTO changes (dataset, key, at, field, old, new) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, k, t, f, s(&before), s(v)]);
                            }
                            cur.insert(f.clone(), v.clone());
                            changed = true;
                        }
                    }
                    let _ = tx.execute("UPDATE rows SET data = ?3, last_seen = ?4, seen_runs = seen_runs + 1, updated = CASE WHEN ?5 THEN ?4 ELSE updated END, enriched = CASE WHEN ?6 THEN ?4 ELSE enriched END WHERE dataset = ?1 AND key = ?2",
                        params![id, k, Value::Object(cur).to_string(), t, changed, enriched]);
                    if changed {
                        m.updated += 1;
                    }
                }
            }
        }
        let _ = tx.commit();
        m.total = db.query_row("SELECT COUNT(*) FROM rows WHERE dataset = ?1", [id], |x| x.get::<_, i64>(0)).unwrap_or(0) as usize;
        m
    }

    /// Rows, oldest enrichment first when `for_enrich` (never-enriched first), else newest first.
    pub fn rows(&self, id: &str, for_enrich: bool, limit: usize) -> Vec<(String, Value, i64, i64)> {
        let db = self.db.lock().unwrap();
        let q = if for_enrich { "SELECT key, data, first_seen, last_seen FROM rows WHERE dataset = ?1 ORDER BY COALESCE(enriched, 0), first_seen LIMIT ?2" } else { "SELECT key, data, first_seen, last_seen FROM rows WHERE dataset = ?1 ORDER BY first_seen DESC, rowid LIMIT ?2" };
        let Ok(mut st) = db.prepare(q) else { return vec![] };
        st.query_map(params![id, limit as i64], |x| Ok((x.get::<_, String>(0)?, serde_json::from_str(&x.get::<_, String>(1)?).unwrap_or(Value::Null), x.get(2)?, x.get(3)?)))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn changes(&self, id: &str, limit: usize) -> Vec<(String, i64, String, String, String)> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT key, at, field, COALESCE(old, ''), COALESCE(new, '') FROM changes WHERE dataset = ?1 ORDER BY at DESC LIMIT ?2") else { return vec![] };
        st.query_map(params![id, limit as i64], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?))).map(|r| r.flatten().collect()).unwrap_or_default()
    }
}

static STORE: std::sync::OnceLock<Option<std::sync::Arc<Store>>> = std::sync::OnceLock::new();

pub fn store() -> Option<std::sync::Arc<Store>> {
    STORE.get_or_init(|| Store::open(&crate::thinker::data_dir()).map_err(|e| tracing::warn!("datasets unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

// ---------------------------------------------------------------------------------------------
// Files

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(cell).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// Writes <name>.xlsx (sheets data + changes), .csv and .md from the stored rows.
pub fn export(st: &Store, d: &Dataset) -> anyhow::Result<Vec<PathBuf>> {
    let rows = st.rows(&d.id, false, 100_000);
    // Columns: the key, then the fields in order of first appearance, then first / last seen.
    let mut cols: Vec<String> = vec![d.key.clone()];
    for (_, v, _, _) in &rows {
        for k in v.as_object().into_iter().flatten().map(|(k, _)| k) {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let table: Vec<Vec<String>> = rows.iter().map(|(key, v, fs, ls)| {
        let mut r: Vec<String> = cols.iter().map(|c| if c == &d.key && v[c].is_null() { key.clone() } else { cell(&v[c]) }).collect();
        r.push(date(*fs));
        r.push(date(*ls));
        r
    }).collect();
    let mut head = cols.clone();
    head.push("first_seen".into());
    head.push("last_seen".into());
    let dir = d.dir();
    std::fs::create_dir_all(&dir)?;
    let files = d.files();
    // CSV
    let esc = |s: &str| if s.contains([',', '"', '\n', ';']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut csv = head.iter().map(|h| esc(h)).collect::<Vec<_>>().join(",") + "\n";
    for r in &table {
        csv.push_str(&(r.iter().map(|c| esc(c)).collect::<Vec<_>>().join(",") + "\n"));
    }
    std::fs::write(&files[1], csv)?;
    // Markdown: the newest 300 rows.
    let md_esc = |s: &str| s.replace('|', "/").replace('\n', " ").chars().take(160).collect::<String>();
    let mut md = format!("# {}\n\n{} rows · {} · updated {}\n\n| {} |\n|{}|\n", d.name, table.len(), d.request, date(now()), head.join(" | "), head.iter().map(|_| "---").collect::<Vec<_>>().join("|"));
    for r in table.iter().take(300) {
        md.push_str(&format!("| {} |\n", r.iter().map(|c| md_esc(c)).collect::<Vec<_>>().join(" | ")));
    }
    std::fs::write(&files[2], md)?;
    // XLSX: data + changes.
    let mut wb = rust_xlsxwriter::Workbook::new();
    let bold = rust_xlsxwriter::Format::new().set_bold();
    let ws = wb.add_worksheet().set_name("data")?;
    for (c, h) in head.iter().enumerate() {
        ws.write_string_with_format(0, c as u16, h, &bold)?;
    }
    for (r, row) in table.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            // Numbers as numbers (sortable in Excel), everything else as text.
            // (Leading zeros stay text: postcodes, ids.)
            match v.parse::<f64>() {
                Ok(n) if n.is_finite() && (v == "0" || v.starts_with("0.") || !v.starts_with('0')) => {
                    ws.write_number(r as u32 + 1, c as u16, n)?;
                }
                _ => {
                    ws.write_string(r as u32 + 1, c as u16, v)?;
                }
            }
        }
    }
    ws.autofit();
    let ch = wb.add_worksheet().set_name("changes")?;
    for (c, h) in ["key", "at", "field", "old", "new"].iter().enumerate() {
        ch.write_string_with_format(0, c as u16, *h, &bold)?;
    }
    for (r, (k, at, f, o, n)) in st.changes(&d.id, 50_000).iter().enumerate() {
        for (c, v) in [k.clone(), date(*at), f.clone(), o.clone(), n.clone()].iter().enumerate() {
            ch.write_string(r as u32 + 1, c as u16, v)?;
        }
    }
    wb.save(&files[0])?;
    Ok(files)
}

// ---------------------------------------------------------------------------------------------
// Runs

/// One run of a dataset: collect, merge, enrich a slice, export. → a one-line note.
pub async fn run(app: &AppState, d: &Dataset) -> anyhow::Result<String> {
    let st = store().context("datasets unavailable")?;
    let s = |k: &str| d.spec[k].as_str().unwrap_or("").to_string();
    let note = match d.source.as_str() {
        "scrape" => {
            use crate::browser::cdp::{Chrome, Visibility};
            let data = crate::thinker::data_dir();
            let screen = crate::config::env_num("BG_SCREEN", 1u16);
            let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
            let tab = chrome.new_tab().await?;
            let catalog = crate::artifacts::Catalog::open(&data)?;
            let fields: Vec<String> = d.spec["fields"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
            let opt = crate::browser::scrape::Options { max_items: d.spec["max_items"].as_u64().unwrap_or(200) as usize, max_pages: 10, model: app.cfg.provider, page_delay_ms: 2500,
                recipes_dir: crate::browser::scrape::default_recipes_dir(&data), auto_login: false, on_user: None };
            let r = async {
                let url = s("url");
                let market = ["willhaben.at/iad/", "kleinanzeigen.de/s-", "ebay.de/sch/", "ebay.at/sch/", "amazon.de/s?", "facebook.com/marketplace/"].iter().any(|m| url.contains(m));
                let items: Vec<Value> = if market {
                    // Known marketplaces: the listings reader (all results of the page with title,
                    // price, location, date, description; willhaben 100 per page) — not a CSS recipe.
                    let url = if url.contains("willhaben.at") && !url.contains("rows=") { format!("{url}&rows=100") } else { url.clone() };
                    crate::browser::listings::load(&tab, &url).await?;
                    let (plan, _) = crate::browser::listings::learn(app, &tab, app.cfg.provider).await?;
                    crate::browser::listings::apply(&tab, &plan).await?.into_iter().map(|l| json!({"url": l.url, "title": l.title, "price": l.price, "location": l.location, "posted": l.posted, "description": l.desc, "id": l.id})).collect()
                } else {
                    let rep = crate::browser::scrape::run(app, &tab, &s("task"), &url, &fields, &catalog, &opt).await?;
                    std::fs::read_to_string(PathBuf::from(&rep.dir).join("items.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
                };
                let m = st.merge(&d.id, &d.key, &items, false);
                let mut note = format!("{} new, {} updated, {} rows", m.new, m.updated, m.total);
                // Enrichment: detail fields from each row's own page, a slice per run.
                let detail: Vec<String> = d.spec["detail_fields"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
                if !detail.is_empty() {
                    let per = d.spec["detail_per_run"].as_u64().unwrap_or(25) as usize;
                    let todo: Vec<String> = st.rows(&d.id, true, per).into_iter().filter_map(|(_, v, _, _)| v["url"].as_str().map(str::to_string)).collect();
                    if !todo.is_empty() {
                        let rep = crate::browser::scrape::run_details(app, &tab, &format!("{}: details", s("task")), &todo, &detail, &catalog, &opt).await?;
                        let det: Vec<Value> = std::fs::read_to_string(PathBuf::from(&rep.dir).join("items.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
                        // Detail items carry the url: merge them onto the rows by their key.
                        let keyed: Vec<Value> = det.into_iter().filter_map(|mut it| {
                            let url = it["url"].as_str()?.to_string();
                            let key = st.rows(&d.id, false, 100_000).into_iter().find(|(_, v, _, _)| v["url"].as_str() == Some(url.as_str())).map(|(k, _, _, _)| k)?;
                            it[d.key.as_str()] = json!(key);
                            Some(it)
                        }).collect();
                        st.merge(&d.id, &d.key, &keyed, true);
                        note.push_str(&format!("; details for {}", keyed.len()));
                    }
                }
                Ok::<_, anyhow::Error>(note)
            }
            .await;
            tab.close().await;
            let _ = chrome.save_cookies(screen).await;
            r?
        }
        "creators" => {
            let site = s("site").to_lowercase();
            let range = crate::creators::parse_range(&d.spec["followers"]);
            let rows: Vec<Value> = match site.as_str() {
                "" | "tiktok" => {
                    let r = crate::creators::find(app, &s("brief"), range, d.spec["min_avg_views"].as_i64(), 30, None).await?;
                    // Everyone who fits, also below the views filter: the table is for the user to sort.
                    r.data["qualified"].as_array().into_iter().chain(r.data["below_views"].as_array()).flatten().cloned().collect()
                }
                "x" | "twitter" => crate::social_sites::x_creators(app, &s("brief"), range, None).await?.2["qualified"].as_array().cloned().unwrap_or_default(),
                "instagram" | "ig" => crate::social_sites::ig_creators(app, &s("brief"), range, None).await?.2["qualified"].as_array().cloned().unwrap_or_default(),
                other => bail!("creators on {other} aren't available"),
            };
            let rows: Vec<Value> = rows.into_iter().map(|mut r| { if r["handle"].is_null() { r["handle"] = r["uid"].clone(); } r }).collect();
            let started = now();
            let m = st.merge(&d.id, "handle", &rows, true);
            // Refresh the stats of a few of the oldest rows (followers, median views) per run —
            // rows from earlier runs only (today's were just read).
            let mut refreshed = 0;
            if site.is_empty() || site == "tiktok" {
                let todo: Vec<String> = st.rows(&d.id, true, 8).into_iter().filter(|(_, _, fs, _)| *fs < started - 60_000).map(|(k, _, _, _)| k).collect();
                if !todo.is_empty() {
                    use crate::browser::cdp::{Chrome, Visibility};
                    // The discovery run's browser is still closing: wait, then one session for all.
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    let data = crate::thinker::data_dir();
                    let screen = crate::thinker::accounts_screen();
                    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
                    let tab = chrome.new_tab().await?;
                    for key in todo {
                        tokio::time::sleep(std::time::Duration::from_millis(6000 + (now() % 4000) as u64)).await;
                        match crate::creators::profile(&tab, &key).await {
                            Ok(c) => {
                                st.merge(&d.id, "handle", &[json!({"handle": key, "followers": c.followers, "median_views": c.median_plays(), "days_since_post": c.days_since_post()})], true);
                                refreshed += 1;
                            }
                            Err(e) if e.to_string().contains("budget") || e.to_string().contains("rate-limiting") => {
                                tracing::info!(dataset = d.id, "refresh stopped: {e:#}");
                                break;
                            }
                            Err(e) => tracing::info!(dataset = d.id, handle = key, "refresh failed: {e:#}"),
                        }
                    }
                    tab.close().await;
                    let _ = chrome.save_cookies(screen).await;
                }
            }
            format!("{} new creators, {} updated, {} refreshed, {} in the table", m.new, m.updated, refreshed, m.total)
        }
        "social" => {
            let r = match s("site").to_lowercase().as_str() {
                "" | "x" | "twitter" => crate::social::research_x(app, &s("question"), None).await?,
                "tiktok" => crate::social_sites::tiktok_research(app, &s("question"), None).await?,
                other => bail!("social research on {other} isn't available"),
            };
            let day = chrono::Local::now().format("%Y-%m-%d").to_string();
            let posts: Vec<Value> = r.data["items"].as_array().into_iter().flatten().map(|p| { let mut p = p.clone(); p["day"] = json!(day); p }).collect();
            let m = st.merge(&d.id, "id", &posts, false);
            // The day's split as its own row (key "day:<date>"): a stance timeline in the same table.
            st.merge(&d.id, "id", &[json!({"id": format!("day:{day}"), "day": day, "text": "daily stance split", "stance": format!("pro {} % / contra {} % / neutral {} %", r.data["split"]["pro"], r.data["split"]["contra"], r.data["split"]["neutral"]), "posts": r.data["related"]})], false);
            format!("{} new posts, {} rows; today: pro {} %, contra {} %", m.new, m.total, r.data["split"]["pro"], r.data["split"]["contra"])
        }
        "market" => {
            let day = chrono::Local::now().format("%Y-%m-%d").to_string();
            let mut n = 0;
            for item in d.spec["items"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                let r = crate::market::analyze(app, item, if s("country").is_empty() { "at" } else { &d.spec["country"].as_str().unwrap_or("at") }, None).await?;
                st.merge(&d.id, "id", &[json!({"id": format!("{item} · {day}"), "item": item, "day": day, "median": r.data["median"], "p25": r.data["p25"], "p75": r.data["p75"],
                    "sales": r.data["kept"], "asking_median": r.data["asking"]["median"], "asking_n": r.data["asking"]["n"]})], false);
                n += 1;
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            }
            format!("prices for {n} items recorded")
        }
        "monitor" => {
            let ms = crate::monitors::store().context("monitors unavailable")?;
            let m = ms.find(&s("monitor")).context("no such monitor")?;
            let items: Vec<Value> = ms.listings(&m.id, None, 100_000);
            let r = st.merge(&d.id, "id", &items, false);
            format!("{} new, {} updated, {} rows (from monitor {})", r.new, r.updated, r.total, m.name)
        }
        other => bail!("unknown dataset source {other}"),
    };
    export(&st, d)?;
    Ok(note)
}

/// Scheduler (started with the monitors): due datasets one at a time.
pub fn spawn(app: std::sync::Arc<AppState>, log: crate::tasklog::TaskLog) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        loop {
            if let Some(st) = store() {
                for d in st.due() {
                    if crate::governor::available_mb().is_some_and(|mb| mb < crate::governor::reserve_mb()) {
                        break;
                    }
                    match run(&app, &d).await {
                        Ok(note) => {
                            tracing::info!(dataset = d.id, note, "dataset run");
                            st.finish(&d.id, &note, None);
                            let tid = log.start("dataset", "dataset", &format!("Updated: {}", d.name));
                            log.finish(&tid, "done", &format!("{}: {note}. Files: {}", d.name, d.files().iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")));
                        }
                        Err(e) => {
                            tracing::info!(dataset = d.id, "dataset run failed: {e:#}");
                            st.finish(&d.id, "", Some(&format!("{e:#}")));
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_tracks_new_updates_and_changes() {
        let dir = std::env::temp_dir().join(format!("ds-test-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = Store::open(&dir).unwrap();
        let id = st.create("Test", "t", "scrape", &json!({}), "url", 1440).unwrap();
        let m = st.merge(&id, "url", &[json!({"url": "a", "price": "10"}), json!({"url": "b", "price": "20"})], false);
        assert_eq!((m.new, m.updated, m.total), (2, 0, 2));
        let m = st.merge(&id, "url", &[json!({"url": "a", "price": "9"}), json!({"url": "c", "price": "5"}), json!({"url": "b", "price": ""})], false);
        assert_eq!((m.new, m.updated, m.total), (1, 1, 3));
        let ch = st.changes(&id, 10);
        assert_eq!(ch.len(), 1);
        assert_eq!((ch[0].3.as_str(), ch[0].4.as_str()), ("10", "9"));
        // An empty value never blanks a stored one.
        assert_eq!(st.rows(&id, false, 10).iter().find(|r| r.0 == "b").unwrap().1["price"], "20");
    }
}
