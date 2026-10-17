//! Automations: Sam acts on its own when a time comes or something happens (lab:
//! triggers lab). Two kinds:
//! - schedule — "every weekday at 08:00", "every 2 hours": runs the request at that time;
//! - watch — a rule over the workspace (tasks, calendar events, emails, files in granted
//!   folders, monitor listings) checked every few minutes. Events ("a new email from …",
//!   "a file appears in …", "a meeting ended") and conditions ("a deadline is 2 days away and
//!   the task is open", "an important email is unanswered for 3 days") are both rules; a rule
//!   fires ONCE per newly matching item (item keys remembered: duplicate-event protection).
//! Rules are compiled ONCE by the LLM into a small DSL and checked by code at 0 tokens (lab:
//! checking every condition with an LLM was ~900 tokens per check → ~1M tokens/day at real
//! scale; compiled with a data sample + validation + one verification turn: F1 0.84). Rules
//! with a fuzzy part ("important") get a short LLM confirmation only when they fire.
//! Firing submits the request as a background task (approvals apply as always); limits: a
//! daily maximum per automation, a minimum check interval. Runs in the server process, not in
//! a chat window: nothing needs to be open.

use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{AppState, llm};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS automations (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, request TEXT NOT NULL, kind TEXT NOT NULL,
  schedule TEXT, rule TEXT, condition TEXT NOT NULL DEFAULT '',
  active INTEGER NOT NULL DEFAULT 1, max_per_day INTEGER NOT NULL DEFAULT 5, every_min INTEGER NOT NULL DEFAULT 15,
  next_check INTEGER NOT NULL, last_check INTEGER, last_fired INTEGER, fired_day TEXT NOT NULL DEFAULT '', fired_today INTEGER NOT NULL DEFAULT 0,
  runs INTEGER NOT NULL DEFAULT 0, last_error TEXT, created INTEGER NOT NULL, session TEXT NOT NULL DEFAULT 'automation'
);
CREATE TABLE IF NOT EXISTS seen (automation TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (automation, key));
CREATE TABLE IF NOT EXISTS fired (automation TEXT NOT NULL, at INTEGER NOT NULL, detail TEXT NOT NULL);
";

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

#[derive(Debug, Clone, Serialize)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub request: String,
    /// schedule | watch
    pub kind: String,
    pub schedule: Option<Value>,
    pub rule: Option<Value>,
    pub condition: String,
    pub active: bool,
    pub max_per_day: i64,
    pub every_min: i64,
    pub next_check: i64,
    pub last_check: Option<i64>,
    pub last_fired: Option<i64>,
    pub fired_today: i64,
    pub runs: i64,
    pub last_error: Option<String>,
    pub created: i64,
}

pub struct Store {
    db: Mutex<Connection>,
}

static STORE: std::sync::OnceLock<Option<std::sync::Arc<Store>>> = std::sync::OnceLock::new();

pub fn store() -> Option<std::sync::Arc<Store>> {
    STORE.get_or_init(|| Store::open(&crate::thinker::data_dir()).map_err(|e| tracing::warn!("automations unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

impl Store {
    pub fn open(data: &std::path::Path) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("automations.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn row(x: &rusqlite::Row) -> rusqlite::Result<Automation> {
        let j = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
        Ok(Automation {
            id: x.get("id")?, name: x.get("name")?, request: x.get("request")?, kind: x.get("kind")?, schedule: j(x.get("schedule")?), rule: j(x.get("rule")?),
            condition: x.get("condition")?, active: x.get::<_, i64>("active")? == 1, max_per_day: x.get("max_per_day")?, every_min: x.get("every_min")?,
            next_check: x.get("next_check")?, last_check: x.get("last_check")?, last_fired: x.get("last_fired")?, fired_today: x.get("fired_today")?,
            runs: x.get("runs")?, last_error: x.get("last_error")?, created: x.get("created")?,
        })
    }

    pub fn create(&self, name: &str, request: &str, kind: &str, schedule: Option<&Value>, rule: Option<&Value>, condition: &str, every_min: i64, max_per_day: i64) -> anyhow::Result<String> {
        let id = format!("a{}", now());
        let next = match schedule {
            Some(s) => next_run(s, chrono::Local::now()).map(|t| t.timestamp_millis()).unwrap_or(now() + 3_600_000),
            None => now(),
        };
        self.db.lock().unwrap().execute(
            "INSERT INTO automations (id, name, request, kind, schedule, rule, condition, every_min, max_per_day, next_check, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![id, name, request, kind, schedule.map(|s| s.to_string()), rule.map(|r| r.to_string()), condition, every_min.max(5), max_per_day.max(1), next, now()],
        )?;
        Ok(id)
    }

    pub fn list(&self) -> Vec<Automation> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM automations ORDER BY created DESC") else { return vec![] };
        st.query_map([], Self::row).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<Automation> {
        self.db.lock().unwrap().query_row("SELECT * FROM automations WHERE id = ?1", [id], Self::row).optional().ok().flatten()
    }

    pub fn due(&self) -> Vec<Automation> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM automations WHERE active = 1 AND next_check <= ?1 ORDER BY next_check") else { return vec![] };
        st.query_map([now()], Self::row).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// The automation the user means: its id, else the best word overlap with name / request.
    pub fn find(&self, words: &str) -> Option<Automation> {
        let all = self.list();
        if let Some(a) = all.iter().find(|a| a.id == words.trim()) {
            return Some(a.clone());
        }
        let toks = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_string).collect::<std::collections::HashSet<_>>();
        let want = toks(words);
        all.iter().map(|a| (toks(&format!("{} {} {}", a.name, a.request, a.condition)).intersection(&want).count(), a)).filter(|(n, _)| *n > 0).max_by_key(|(n, a)| (*n, a.created)).map(|(_, a)| a.clone())
    }

    pub fn set_active(&self, id: &str, on: bool) -> bool {
        self.db.lock().unwrap().execute("UPDATE automations SET active = ?2, next_check = CASE WHEN ?2 = 1 AND next_check < ?3 THEN ?3 ELSE next_check END WHERE id = ?1", params![id, on as i64, now()]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn delete(&self, id: &str) -> bool {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM seen WHERE automation = ?1", [id]);
        let _ = db.execute("DELETE FROM fired WHERE automation = ?1", [id]);
        db.execute("DELETE FROM automations WHERE id = ?1", [id]).map(|n| n > 0).unwrap_or(false)
    }

    /// Item keys not seen before (and marks them seen).
    pub fn unseen(&self, id: &str, keys: &[String]) -> Vec<String> {
        let db = self.db.lock().unwrap();
        keys.iter().filter(|k| db.execute("INSERT OR IGNORE INTO seen (automation, key, at) VALUES (?1, ?2, ?3)", params![id, k, now()]).map(|n| n > 0).unwrap_or(false)).cloned().collect()
    }

    /// Whether the daily limit allows one more firing (and counts it).
    pub fn take_fire(&self, id: &str, detail: &str) -> bool {
        let db = self.db.lock().unwrap();
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let (fd, ft, max): (String, i64, i64) = db.query_row("SELECT fired_day, fired_today, max_per_day FROM automations WHERE id = ?1", [id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).unwrap_or_default();
        let today = if fd == day { ft } else { 0 };
        if today >= max {
            return false;
        }
        let _ = db.execute("UPDATE automations SET fired_day = ?2, fired_today = ?3, last_fired = ?4, runs = runs + 1 WHERE id = ?1", params![id, day, today + 1, now()]);
        let _ = db.execute("INSERT INTO fired (automation, at, detail) VALUES (?1, ?2, ?3)", params![id, now(), detail]);
        true
    }

    pub fn checked(&self, id: &str, next: i64, error: Option<&str>) {
        let _ = self.db.lock().unwrap().execute("UPDATE automations SET last_check = ?2, next_check = ?3, last_error = ?4 WHERE id = ?1", params![id, now(), next, error]);
    }

    pub fn history(&self, id: &str, n: usize) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT at, detail FROM fired WHERE automation = ?1 ORDER BY at DESC LIMIT ?2") else { return vec![] };
        st.query_map(params![id, n as i64], |x| Ok(json!({"at": x.get::<_, i64>(0)?, "detail": x.get::<_, String>(1)?}))).map(|r| r.flatten().collect()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Schedules: {"at": "08:00", "days": "daily" | "weekdays" | ["mon", "fri"]} or {"every_min": 120}.

/// "8:00", "08:00", "8", "8am", "8:30 PM", "20 Uhr" → time of day.
fn parse_at(t: &str) -> Option<chrono::NaiveTime> {
    let l = t.trim().to_lowercase().replace("uhr", "").replace('.', ":");
    let pm = l.contains("pm");
    let digits: String = l.chars().filter(|c| c.is_ascii_digit() || *c == ':').collect();
    let mut parts = digits.split(':');
    let mut h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next().filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0)).unwrap_or(0);
    if pm && h < 12 {
        h += 12;
    }
    if l.contains("am") && h == 12 {
        h = 0;
    }
    chrono::NaiveTime::from_hms_opt(h, m, 0)
}

/// "daily" | "weekdays" | "weekends" | "mon-fri" | "mon,wed" | ["Monday", "Fri"] → weekday numbers (Mon = 0).
fn parse_days(v: &Value, names: &[&str; 7]) -> Vec<u32> {
    let idx = |x: &str| {
        let x = x.trim().to_lowercase();
        let de = ["mo", "di", "mi", "do", "fr", "sa", "so"];
        names.iter().position(|n| x.starts_with(n)).or_else(|| de.iter().position(|n| x.starts_with(n))).map(|p| p as u32)
    };
    let text = match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let t = text.to_lowercase();
    if t.is_empty() || t.contains("daily") || t.contains("every day") || t.contains("täglich") {
        return (0..7).collect();
    }
    if t.contains("weekday") || t.contains("werktag") || t.contains("wochentag") {
        return (0..5).collect();
    }
    if t.contains("weekend") || t.contains("wochenende") {
        return vec![5, 6];
    }
    let mut out = Vec::new();
    for part in t.split([',', ' ', ';']).filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once(['-', '–']) {
            if let (Some(a), Some(b)) = (idx(a), idx(b)) {
                out.extend(if a <= b { (a..=b).collect::<Vec<_>>() } else { (a..7).chain(0..=b).collect() });
            }
        } else if let Some(d) = idx(part) {
            out.push(d);
        }
    }
    if out.is_empty() { (0..7).collect() } else { out }
}

pub fn next_run(s: &Value, after: chrono::DateTime<chrono::Local>) -> Option<chrono::DateTime<chrono::Local>> {
    // "08:53 daily" / "at 8:05" → "08:53" / "8:05": the model sometimes puts words into the time.
    let s = &match s["at"].as_str().and_then(|a| regex::Regex::new(r"\b(\d{1,2}:\d{2})\b").ok()?.find(a).map(|m| m.as_str().to_string())) {
        Some(hm) => { let mut v = s.clone(); v["at"] = json!(hm); v }
        None => s.clone(),
    };
    use chrono::{Datelike, TimeZone};
    if let Some(m) = s["every_min"].as_i64().filter(|m| *m > 0) {
        return Some(after + chrono::Duration::minutes(m.max(5)));
    }
    let at = parse_at(s["at"].as_str().map(str::to_string).or_else(|| s["at"].as_i64().map(|h| h.to_string()))?.as_str())?;
    let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let allowed = parse_days(&s["days"], &names);
    for i in 0..8 {
        let d = after.date_naive() + chrono::Duration::days(i);
        if !allowed.contains(&d.weekday().num_days_from_monday()) {
            continue;
        }
        let t = chrono::Local.from_local_datetime(&d.and_time(at)).single()?;
        if t > after {
            return Some(t);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Workspace state for watch rules (what the rule DSL can see). Test mode: empty sources.

pub async fn workspace() -> Value {
    let test = std::env::var("SAM_NO_REAL_APPS").is_ok_and(|v| v == "1");
    let today = chrono::Local::now();
    let mut tasks = Vec::new();
    let mut events = Vec::new();
    let mut emails = Vec::new();
    if !test {
        if let Ok(v) = crate::apps::reminders("").await {
            for r in v.as_array().into_iter().flatten() {
                let due = r["due"].as_str().and_then(|d| chrono::NaiveDateTime::parse_from_str(d, "%a %b %d %Y %H:%M").ok()).map(|d| d.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default();
                tasks.push(json!({"title": r["title"], "list": r["list"], "due": due, "done": false}));
            }
        }
        if let Ok(v) = crate::apps::calendar_events_raw(today - chrono::Duration::days(1), today + chrono::Duration::days(14)).await {
            for e in v.as_array().into_iter().flatten() {
                events.push(json!({"title": e["title"], "start": e["start"], "end": e["end"], "location": e["location"], "calendar": e["calendar"]}));
            }
        }
        if crate::email::Account::load().is_some() {
            let q: crate::email::Query = serde_json::from_value(json!({"since_days": 7, "limit": 40})).unwrap_or_default();
            if let Ok(ms) = crate::email::search(&q).await {
                for m in ms {
                    emails.push(json!({"uid": m.uid, "from": m.from, "subject": m.subject, "received": m.date, "snippet": m.snippet}));
                }
            }
        }
    }
    // Files in the folders the user granted (newest first, top level).
    let mut files = Vec::new();
    if let Ok(f) = crate::files::Files::open(&crate::thinker::data_dir()) {
        for g in f.grants() {
            for e in std::fs::read_dir(&g.path).into_iter().flatten().flatten().take(200) {
                let Ok(md) = e.metadata() else { continue };
                if md.is_file() {
                    let modified = md.modified().ok().map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default();
                    files.push(json!({"path": e.path().display().to_string(), "name": e.file_name().to_string_lossy(), "folder": g.path.display().to_string(), "modified": modified}));
                }
            }
        }
    }
    // Monitor listings first seen in the last 3 days.
    let mut listings = Vec::new();
    if let Some(m) = crate::monitors::store() {
        for mon in m.list() {
            for l in m.listings(&mon.id, Some(now() - 3 * 86_400_000), 50) {
                listings.push(json!({"monitor": mon.name, "id": l["id"], "title": l["title"], "price": l["price"], "url": l["url"], "first_seen": l["first_seen"], "new": l["baseline"] == false}));
            }
        }
    }
    json!({"today": today.format("%Y-%m-%dT%H:%M").to_string(), "tasks": tasks, "events": events, "emails": emails, "files": files, "listings": listings})
}

// ---------------------------------------------------------------------------------------------
// Rule DSL (triggers lab): {"source", "where": [[field, op, value]...], "fire_if": "any" |
// "count>N", "key": [fields that identify an item], "llm_check": ""}.

pub const DSL: &str = "A rule is JSON: {\"source\": \"tasks|events|emails|files|listings\", \"where\": [[field, op, value], ...], \"fire_if\": \"any\" | \"count>N\", \
\"key\": [fields that identify one item, e.g. [\"title\", \"due\"]], \"llm_check\": \"a fuzzy part only a reader can judge (e.g. 'the email is important'), else empty\", \
\"new_items_only\": true for triggers about NEW things (a new email / file / listing arrives: what exists now doesn't fire), false for states (anything overdue fires now)}. \
Ops: == != < <= > >= contains not_contains days_until_lte days_until_gte days_since_gte hours_since_lte hours_until_lte (date fields vs now), is_true is_false. \
Fields: tasks{title, list, due, done} events{title, start, end, location, calendar} emails{from, subject, received, snippet} files{name, folder, modified, path} \
listings{monitor, title, price, url, first_seen, new}. Dates are 'YYYY-MM-DDTHH:MM'. Values must be literal values seen in the data sample (no placeholders). \
'fire_if' is only 'any' or 'count>N'; put every date test into 'where'.";

const OPS: &[&str] = &["==", "!=", "<", "<=", ">", ">=", "contains", "not_contains", "days_until_lte", "days_until_gte", "days_since_gte", "hours_since_lte", "hours_until_lte", "is_true", "is_false"];

fn get<'a>(o: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(o, |o, p| &o[p])
}

fn parse_time(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    chrono::DateTime::parse_from_rfc3339(s).map(|t| t.with_timezone(&chrono::Local).naive_local()).ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(&s.chars().take(16).collect::<String>(), "%Y-%m-%dT%H:%M").ok())
        .or_else(|| chrono::NaiveDate::parse_from_str(&s.chars().take(10).collect::<String>(), "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)))
        .or_else(|| chrono::DateTime::parse_from_rfc2822(s).map(|t| t.with_timezone(&chrono::Local).naive_local()).ok())
}

fn test(row: &Value, f: &str, op: &str, v: &Value, now: chrono::NaiveDateTime) -> bool {
    let x = get(row, f);
    let num = |a: &Value| a.as_f64().or_else(|| a.as_str().and_then(crate::monitors::price_num));
    let txt = |a: &Value| a.as_str().map(str::to_lowercase).unwrap_or_else(|| a.to_string().to_lowercase());
    let days = |a: &Value| a.as_str().and_then(parse_time).map(|t| (t.date() - now.date()).num_days());
    let hours = |a: &Value| a.as_str().and_then(parse_time).map(|t| (t - now).num_minutes() as f64 / 60.0);
    let vn = v.as_f64().unwrap_or(0.0);
    match op {
        "==" => x == v || (num(x).is_some() && num(x) == num(v)) || txt(x) == txt(v),
        "!=" => !(x == v || (num(x).is_some() && num(x) == num(v)) || txt(x) == txt(v)),
        "<" | "<=" | ">" | ">=" => match (num(x), num(v)) {
            (Some(a), Some(b)) => match op { "<" => a < b, "<=" => a <= b, ">" => a > b, _ => a >= b },
            _ => false,
        },
        "contains" => txt(x).contains(&txt(v).trim_matches('"').to_string()),
        "not_contains" => !txt(x).contains(&txt(v).trim_matches('"').to_string()),
        "days_until_lte" => days(x).is_some_and(|d| d >= 0 && d as f64 <= vn),
        "days_until_gte" => days(x).is_some_and(|d| d as f64 >= vn),
        "days_since_gte" => days(x).is_some_and(|d| -(d as f64) >= vn),
        "hours_since_lte" => hours(x).is_some_and(|h| h <= 0.0 && -h <= vn),
        "hours_until_lte" => hours(x).is_some_and(|h| h >= 0.0 && h <= vn),
        "is_true" => x.as_bool() == Some(true),
        "is_false" => x.as_bool() != Some(true),
        _ => false,
    }
}

/// Strict check of a compiled rule (lab: the model put date tests into fire_if, wrote
/// placeholder values): Err explains what to fix.
pub fn validate(rule: &Value) -> anyhow::Result<()> {
    let src = rule["source"].as_str().unwrap_or("");
    anyhow::ensure!(["tasks", "events", "emails", "files", "listings"].contains(&src), "unknown source {src:?}");
    for c in rule["where"].as_array().into_iter().flatten() {
        let (op, v) = (c[1].as_str().unwrap_or(""), &c[2]);
        anyhow::ensure!(c.as_array().is_some_and(|a| a.len() == 2 || a.len() == 3) && c[0].is_string(), "each condition is [field, op, value]");
        anyhow::ensure!(OPS.contains(&op), "unknown op {op:?}");
        if let Some(s) = v.as_str() {
            anyhow::ensure!(!regex::Regex::new(r"^[A-Z_]{4,}$").unwrap().is_match(s), "placeholder value {s:?}: use a real value from the data sample");
        }
    }
    let fi = rule["fire_if"].as_str().unwrap_or("any").replace(' ', "");
    anyhow::ensure!(fi == "any" || regex::Regex::new(r"^count>\d+$").unwrap().is_match(&fi), "fire_if must be 'any' or 'count>N' (date tests go into where)");
    Ok(())
}

/// Items of the workspace matching the rule (with their dedup keys), and whether it fires.
pub fn matches(rule: &Value, ws: &Value) -> (bool, Vec<(String, Value)>) {
    let now = ws["today"].as_str().and_then(parse_time).unwrap_or_else(|| chrono::Local::now().naive_local());
    let rows = ws[rule["source"].as_str().unwrap_or("")].as_array().cloned().unwrap_or_default();
    let conds: Vec<(String, String, Value)> = rule["where"].as_array().into_iter().flatten().map(|c| (c[0].as_str().unwrap_or("").to_string(), c[1].as_str().unwrap_or("").to_string(), c.get(2).cloned().unwrap_or(Value::Null))).collect();
    let keys: Vec<String> = rule["key"].as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect()).filter(|k: &Vec<String>| !k.is_empty())
        .unwrap_or_else(|| vec!["title".into(), "subject".into(), "path".into(), "url".into(), "uid".into(), "due".into(), "start".into()]);
    let hits: Vec<(String, Value)> = rows.into_iter().filter(|r| conds.iter().all(|(f, op, v)| test(r, f, op, v, now)))
        .map(|r| (keys.iter().map(|k| get(&r, k).to_string()).collect::<Vec<_>>().join("|"), r)).collect();
    let fi = rule["fire_if"].as_str().unwrap_or("any").replace(' ', "");
    let n: usize = fi.strip_prefix("count>").and_then(|x| x.parse().ok()).unwrap_or(0);
    (hits.len() > n, hits)
}

const COMPILE: &str = "You compile a user's automation trigger into a rule a program checks every few minutes. ";

/// Condition text → validated rule (data sample, strict checks, one verification turn).
pub async fn compile(app: &AppState, condition: &str) -> anyhow::Result<(Value, u64)> {
    let ws = workspace().await;
    let sample: String = json!({"today": ws["today"], "tasks": ws["tasks"].as_array().map(|a| a.iter().take(3).cloned().collect::<Vec<_>>()), "events": ws["events"].as_array().map(|a| a.iter().take(3).cloned().collect::<Vec<_>>()),
        "emails": ws["emails"].as_array().map(|a| a.iter().take(3).cloned().collect::<Vec<_>>()), "files": ws["files"].as_array().map(|a| a.iter().take(3).cloned().collect::<Vec<_>>()), "listings": ws["listings"].as_array().map(|a| a.iter().take(3).cloned().collect::<Vec<_>>())})
        .to_string().chars().take(3000).collect();
    let sys = format!("{COMPILE}{DSL}\nReply ONLY with the JSON rule.");
    let ctx = format!("Trigger: {condition}\nData sample (today's real values): {sample}");
    let mut tokens = 0;
    let (txt, u) = llm::complete(app, app.cfg.provider, &sys, &ctx, true, 3000).await?;
    tokens += u.prompt + u.completion;
    let mut rule: Value = serde_json::from_str(&txt)?;
    if let Err(e) = validate(&rule) {
        let (txt, u) = llm::complete(app, app.cfg.provider, &sys, &format!("{ctx}\nYour rule {rule} is invalid: {e:#}. Return a corrected rule."), true, 3000).await?;
        tokens += u.prompt + u.completion;
        rule = serde_json::from_str(&txt)?;
        validate(&rule)?;
    }
    // Verification: what it matches today; the model confirms or corrects once.
    let (fires, hits) = matches(&rule, &ws);
    let rows: String = ws[rule["source"].as_str().unwrap_or("")].to_string().chars().take(1500).collect();
    let verify = format!("{ctx}\nYour rule: {rule}\nToday it {} ({} matching items: {}). All rows of that source: {rows}\nIs that what the trigger means? Return the same rule if yes, else a corrected one.",
        if fires { "FIRES" } else { "does NOT fire" }, hits.len(), hits.iter().take(3).map(|h| h.0.clone()).collect::<Vec<_>>().join("; "));
    if let Ok((txt, u)) = llm::complete(app, app.cfg.provider, &sys, &verify, true, 3000).await {
        tokens += u.prompt + u.completion;
        if let Ok(r2) = serde_json::from_str::<Value>(&txt) {
            if validate(&r2).is_ok() {
                rule = r2;
            }
        }
    }
    Ok((rule, tokens))
}

/// Seeds a watch rule's current matches as seen when it watches for NEW things (a new email,
/// a new file): those already there don't fire. State conditions ("anything overdue") fire.
pub async fn baseline(id: &str, rule: &Value) {
    if rule["new_items_only"] == true {
        if let Some(st) = store() {
            let (_, hits) = matches(rule, &workspace().await);
            st.unseen(id, &hits.into_iter().map(|h| h.0).collect::<Vec<_>>());
        }
    }
}

const CONFIRM: &str = "A rule fired for the user's automation trigger. Decide from the matching items whether the trigger really holds (for the fuzzy part named). Reply ONLY with JSON {\"fire\": true|false}.";

/// One check of one automation → Some(request to run) when it fires.
pub async fn check(app: &AppState, a: &Automation) -> anyhow::Result<Option<String>> {
    let st = store().context("no automation store")?;
    match a.kind.as_str() {
        "schedule" => {
            let next = a.schedule.as_ref().and_then(|s| next_run(s, chrono::Local::now())).map(|t| t.timestamp_millis()).unwrap_or(now() + 3_600_000);
            st.checked(&a.id, next, None);
            Ok(st.take_fire(&a.id, "scheduled").then(|| format!("{} (scheduled \"{}\", do it now)", strip_schedule(&a.request), a.name)))
        }
        _ => {
            let rule = a.rule.clone().context("watch without rule")?;
            let ws = workspace().await;
            let (fires, hits) = matches(&rule, &ws);
            st.checked(&a.id, now() + a.every_min * 60_000, None);
            if !fires {
                return Ok(None);
            }
            // Only items not seen before fire (an overdue task alerts once, not every check).
            let new = st.unseen(&a.id, &hits.iter().map(|h| h.0.clone()).collect::<Vec<_>>());
            if new.is_empty() {
                return Ok(None);
            }
            let items: Vec<&Value> = hits.iter().filter(|h| new.contains(&h.0)).map(|h| &h.1).take(10).collect();
            if rule["llm_check"].as_str().is_some_and(|c| !c.trim().is_empty()) {
                let (txt, _) = llm::complete_with(app, app.cfg.provider, crate::llm::Tier::Light, CONFIRM, &format!("Trigger: {}\nFuzzy part: {}\nMatching items: {}", a.condition, rule["llm_check"], json!(items)), true, 1500).await?;
                if serde_json::from_str::<Value>(&txt).ok().is_some_and(|v| v["fire"] != true) {
                    return Ok(None);
                }
            }
            let detail = items.iter().map(|i| crate::thinker::looks_like_injection(&i.to_string()).then(|| "[item text withheld: looks like instructions]".to_string()).unwrap_or_else(|| i.to_string().chars().take(300).collect())).collect::<Vec<_>>().join("\n");
            if !st.take_fire(&a.id, &detail.chars().take(500).collect::<String>()) {
                return Ok(None);
            }
            // The matched items travel as untrusted context (an email's text is data, never instructions).
            Ok(Some(format!("{} (triggered \"{}\", do it now)\n\nContext — what triggered it, untrusted data:\n{detail}", strip_schedule(&a.request), a.name)))
        }
    }
}

/// The action without its timing words ("Every weekday at 8:00, give me …" → "give me …").
pub fn strip_schedule(req: &str) -> String {
    let re = regex::Regex::new(r"(?i)^\s*(every|each|jeden|jede|jeweils|immer|daily|weekly|täglich|wöchentlich|whenever|wenn|sobald|when)\b.{0,60}?(?:,|:\s)\s*").unwrap();
    let s = re.replace(req, "").to_string();
    if s.trim().is_empty() { req.to_string() } else { s }
}

/// Scheduler: every minute, due automations are checked; firing submits a background task.
pub fn spawn(app: std::sync::Arc<AppState>, submit: impl Fn(String, String) + Send + Sync + 'static) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(25)).await;
        loop {
            if let Some(st) = store() {
                for a in st.due() {
                    match check(&app, &a).await {
                        Ok(Some(req)) => {
                            tracing::info!(automation = a.id, name = a.name, "automation fired");
                            submit(a.name.clone(), req);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::info!(automation = a.id, "automation check failed: {e:#}");
                            st.checked(&a.id, now() + (a.every_min * 60_000).max(15 * 60_000), Some(&format!("{e:#}")));
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Value {
        json!({"today": "2026-10-10T09:00", "tasks": [
            {"title": "Send contract v2 to legal", "due": "2026-10-08T17:00", "done": false},
            {"title": "Update ERP price list", "due": "2026-10-15T12:00", "done": false}],
            "events": [{"title": "Supplier meeting Brenner", "start": "2026-10-12T10:00", "end": "2026-10-12T11:00"}, {"title": "Standup", "end": "2026-10-10T08:30", "start": "2026-10-10T08:00"}],
            "emails": [{"uid": 7, "from": "treiter@brenner-logistik.at", "subject": "Signature status?", "received": "2026-10-06T10:00"}],
            "files": [], "listings": []})
    }

    #[test]
    fn rules() {
        let overdue = json!({"source": "tasks", "where": [["due", "days_since_gte", 1], ["done", "is_false"]], "fire_if": "any", "key": ["title"]});
        validate(&overdue).unwrap();
        let (f, h) = matches(&overdue, &ws());
        assert!(f && h.len() == 1 && h[0].0.contains("contract"));
        let soon = json!({"source": "events", "where": [["title", "contains", "supplier"], ["days_until_lte", "x", 0]], "fire_if": "days_until_lte"});
        assert!(validate(&soon).is_err(), "date tests in fire_if are rejected");
        let soon = json!({"source": "events", "where": [["title", "contains", "supplier"], ["start", "days_until_lte", 3]], "fire_if": "any"});
        assert!(matches(&soon, &ws()).0);
        let ended = json!({"source": "events", "where": [["end", "hours_since_lte", 2]], "fire_if": "any"});
        assert_eq!(matches(&ended, &ws()).1.len(), 1, "a meeting that ended within 2 hours");
        let stale = json!({"source": "emails", "where": [["from", "contains", "brenner"], ["received", "days_since_gte", 3]], "fire_if": "any"});
        assert!(matches(&stale, &ws()).0);
        assert!(validate(&json!({"source": "emails", "where": [["from", "contains", "CEO_EMAIL"]]})).is_err(), "placeholders are rejected");
    }

    #[test]
    fn strips_timing() {
        assert_eq!(strip_schedule("Every weekday at 8:00, give me a morning briefing"), "give me a morning briefing");
        assert_eq!(strip_schedule("Jeden Freitag um 16 Uhr: Wochenbericht erstellen"), "Wochenbericht erstellen");
        assert_eq!(strip_schedule("Give me a briefing"), "Give me a briefing");
    }

    #[test]
    fn schedules() {
        use chrono::TimeZone;
        let fri = chrono::Local.with_ymd_and_hms(2026, 10, 9, 9, 0, 0).unwrap();
        let n = next_run(&json!({"at": "08:00", "days": "weekdays"}), fri).unwrap();
        assert_eq!(n.format("%a %H:%M").to_string(), "Mon 08:00");
        let n = next_run(&json!({"at": "16:00", "days": ["fri"]}), fri).unwrap();
        assert_eq!(n.format("%a %d %H:%M").to_string(), "Fri 09 16:00");
        assert!(next_run(&json!({"every_min": 120}), fri).unwrap() > fri);
        for (at, days, want) in [("8:00", json!("Mon-Fri"), "Mon 08:00"), ("8am", json!(["Monday", "Tuesday"]), "Mon 08:00"), ("8", json!("weekdays"), "Mon 08:00"),
                                 ("4 pm", json!("fri"), "Fri 16:00"), ("20 Uhr", json!("täglich"), "Fri 20:00"), ("16.30", json!("mo,fr"), "Fri 16:30")] {
            let n = next_run(&json!({"at": at, "days": days}), fri).unwrap_or_else(|| panic!("{at} {days}"));
            assert_eq!(n.format("%a %H:%M").to_string(), want, "{at} {days}");
        }
    }

    #[test]
    fn dedup_and_limits() {
        let d = std::env::temp_dir().join(format!("auto-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("automations.db"));
        let s = Store::open(&d).unwrap();
        let id = s.create("Overdue", "review it", "watch", None, Some(&json!({"source": "tasks"})), "overdue", 15, 2).unwrap();
        assert_eq!(s.unseen(&id, &["a".into(), "b".into()]).len(), 2);
        assert_eq!(s.unseen(&id, &["a".into(), "c".into()]), vec!["c".to_string()], "an item fires once");
        assert!(s.take_fire(&id, "1") && s.take_fire(&id, "2"));
        assert!(!s.take_fire(&id, "3"), "daily limit");
        assert_eq!(s.find("overdue tasks").unwrap().id, id);
    }
}
