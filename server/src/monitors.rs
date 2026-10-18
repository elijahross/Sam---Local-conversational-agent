//! Monitors: saved searches Sam re-checks on a schedule, reacting when a new listing appears
//! (willhaben, ImmoScout24, eBay, Kleinanzeigen, Amazon, any listing page). Lab: scrape lab.
//!
//! Storage `data/monitors.db`: monitors (the search + its cached extraction plan), listings (one
//! row per monitor × site item id — the dedup key; first/last seen, current price), price
//! history, and one row per run. A run diffs the page against the stored listings:
//! - unknown id → new (alerted, except on the first "baseline" run);
//! - known id → last_seen bumped; a changed price is recorded (drops are mentioned);
//! - bumped / re-promoted old ads keep their id, so they never alert twice.
//! Alerts become a finished task record (kind "monitor") → drop-down toast + searchable history.

use std::{path::Path, sync::Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS monitors (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  request TEXT NOT NULL,
  url TEXT NOT NULL,
  site TEXT NOT NULL,
  plan TEXT,
  interval_min INTEGER NOT NULL,
  active INTEGER NOT NULL DEFAULT 1,
  created INTEGER NOT NULL,
  last_run INTEGER,
  next_run INTEGER NOT NULL,
  last_status TEXT NOT NULL DEFAULT 'new',
  last_error TEXT,
  fails INTEGER NOT NULL DEFAULT 0,
  runs INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS listings (
  monitor TEXT NOT NULL,
  item TEXT NOT NULL,
  url TEXT NOT NULL,
  title TEXT NOT NULL,
  price TEXT NOT NULL,
  price_num REAL,
  location TEXT NOT NULL,
  posted TEXT NOT NULL,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  seen_runs INTEGER NOT NULL DEFAULT 1,
  baseline INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (monitor, item)
);
CREATE INDEX IF NOT EXISTS listings_new ON listings(monitor, first_seen);
CREATE TABLE IF NOT EXISTS prices (monitor TEXT NOT NULL, item TEXT NOT NULL, at INTEGER NOT NULL, price TEXT NOT NULL, price_num REAL);
CREATE TABLE IF NOT EXISTS runs (monitor TEXT NOT NULL, at INTEGER NOT NULL, found INTEGER NOT NULL, new INTEGER NOT NULL, changed INTEGER NOT NULL, tokens INTEGER NOT NULL, ms INTEGER NOT NULL, error TEXT);
";

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// One listing as extracted from a results page.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Listing {
    pub id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub posted: String,
    /// Description or card text (not stored): what the judge reads besides the title.
    #[serde(default, skip_serializing)]
    pub desc: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub request: String,
    pub url: String,
    pub site: String,
    pub interval_min: i64,
    pub active: bool,
    pub created: i64,
    pub last_run: Option<i64>,
    pub next_run: i64,
    pub last_status: String,
    pub last_error: Option<String>,
    pub fails: i64,
    pub runs: i64,
    /// Listings stored for this monitor / first seen in the last 24 h (filled by `list`).
    pub listings: i64,
    pub new_24h: i64,
    #[serde(skip)]
    pub plan: Option<Value>,
    /// Price bounds applied after reading the page: {"min": n|null, "max": n|null}.
    pub filters: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct Diff {
    pub found: usize,
    pub new: Vec<Listing>,
    /// Unseen ids that aren't fresh (posted before the last check, or far down an undated
    /// page): older ads sliding onto page 1, re-promotions. Stored, never alerted.
    pub quiet: Vec<Listing>,
    /// (listing, old price) for listings whose price changed.
    pub changed: Vec<(Listing, String)>,
    pub baseline: bool,
    /// Judged monitors: how many of the judged listings match (None: not judged).
    pub matched: Option<usize>,
}

/// "€ 1.200" / "1.238 €" / "EUR 886,19" / "$20.00" → number (German and English separators).
pub fn price_num(s: &str) -> Option<f64> {
    let s = s.replace('\u{a0}', " ");
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let raw: String = s[start..].chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '\'')).collect();
    let raw = raw.trim_end_matches(['.', ',']);
    let cleaned = if raw.rsplit_once(',').is_some_and(|(_, d)| (1..=2).contains(&d.len())) {
        raw.replace(['.', '\''], "").replace(',', ".")
    } else if raw.matches('.').count() == 1 && raw.rsplit_once('.').is_some_and(|(_, d)| (1..=2).contains(&d.len())) && !raw.contains(',') {
        raw.to_string()
    } else {
        raw.replace(['.', ',', '\''], "")
    };
    cleaned.parse().ok()
}

/// When a listing was posted, from what sites show: ISO times (willhaben), "Heute, 12:34" /
/// "Gestern, …" (Kleinanzeigen), "10. Okt. 12:03" (eBay), "12.10.2026", "vor 3 Stunden".
pub fn posted_ms(s: &str, now: chrono::DateTime<chrono::Local>) -> Option<i64> {
    use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone};
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.timestamp_millis());
    }
    let low = s.to_lowercase();
    let time = regex::Regex::new(r"(\d{1,2}):(\d{2})").ok()?.captures(&low).and_then(|c| NaiveTime::from_hms_opt(c[1].parse().ok()?, c[2].parse().ok()?, 0));
    let at = |d: NaiveDate| chrono::Local.from_local_datetime(&d.and_time(time.unwrap_or(NaiveTime::MIN))).single().map(|x| x.timestamp_millis());
    let today = now.date_naive();
    if low.starts_with("heute") || low.starts_with("today") {
        return at(today);
    }
    if low.starts_with("gestern") || low.starts_with("yesterday") {
        return at(today.pred_opt()?);
    }
    if let Some(c) = regex::Regex::new(r"vor (\d+)\s*(min|std|stunde|tag)").ok()?.captures(&low) {
        let n: i64 = c[1].parse().ok()?;
        let unit = match &c[2] { "min" => 60_000, "tag" => 86_400_000, _ => 3_600_000 };
        return Some(now.timestamp_millis() - n * unit);
    }
    if let Some(c) = regex::Regex::new(r"^(\d{1,2})\.(\d{1,2})\.(\d{2,4})").ok()?.captures(&low) {
        let y: i32 = c[3].parse().ok()?;
        return at(NaiveDate::from_ymd_opt(if y < 100 { 2000 + y } else { y }, c[2].parse().ok()?, c[1].parse().ok()?)?);
    }
    // "10. Oct. 12:03" / "10. Okt. 12:03" (current year; a date "in the future" is last year's).
    if let Some(c) = regex::Regex::new(r"^(\d{1,2})\.\s*([a-zä]{3})").ok()?.captures(&low) {
        let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
        let de = ["jan", "feb", "mär", "apr", "mai", "jun", "jul", "aug", "sep", "okt", "nov", "dez"];
        let m = months.iter().position(|x| *x == &c[2]).or_else(|| de.iter().position(|x| *x == &c[2]))? as u32 + 1;
        let mut d = NaiveDate::from_ymd_opt(today.year(), m, c[1].parse().ok()?)?;
        if d > today {
            d = NaiveDate::from_ymd_opt(today.year() - 1, m, d.day())?;
        }
        return at(d);
    }
    None
}

pub fn site_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())).unwrap_or_default()
}

pub struct Store {
    db: Mutex<Connection>,
}

impl Store {
    pub fn open(data: &Path) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("monitors.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA)?;
        // Added after the first release: price bounds etc. ({"min": n, "max": n}).
        let _ = db.execute("ALTER TABLE monitors ADD COLUMN filters TEXT", []);
        // Judged monitors: 1 = matches the request, 0 = the judge rejected it, NULL = not judged.
        let _ = db.execute("ALTER TABLE listings ADD COLUMN matched INTEGER", []);
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn create(&self, name: &str, request: &str, url: &str, interval_min: i64) -> anyhow::Result<String> {
        self.create_with(name, request, url, interval_min, None, None, &[])
    }

    /// `words`: every one must appear in a listing's title (things for sale; empty for flats).
    pub fn create_with(&self, name: &str, request: &str, url: &str, interval_min: i64, min: Option<f64>, max: Option<f64>, words: &[String]) -> anyhow::Result<String> {
        let id = format!("m{}", now());
        // LinkedIn restricts accounts that look automated: a few checks a day at most.
        let interval_min = if crate::browser::linkedin::is_linkedin(url) { interval_min.max(crate::browser::linkedin::MIN_INTERVAL) } else { interval_min };
        let t = now();
        self.db.lock().unwrap().execute(
            // The creator runs the baseline itself; the schedule starts one interval later.
            "INSERT INTO monitors (id, name, request, url, site, interval_min, created, next_run, filters) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, name, request, url, site_of(url), interval_min.max(10), t, t + interval_min.max(10) * 60_000, json!({"min": min, "max": max, "words": words}).to_string()],
        )?;
        Ok(id)
    }

    fn row(x: &rusqlite::Row) -> rusqlite::Result<Monitor> {
        let plan: Option<String> = x.get("plan")?;
        Ok(Monitor {
            id: x.get("id")?, name: x.get("name")?, request: x.get("request")?, url: x.get("url")?, site: x.get("site")?,
            interval_min: x.get("interval_min")?, active: x.get::<_, i64>("active")? == 1, created: x.get("created")?, last_run: x.get("last_run")?,
            next_run: x.get("next_run")?, last_status: x.get("last_status")?, last_error: x.get("last_error")?, fails: x.get("fails")?, runs: x.get("runs")?,
            listings: 0, new_24h: 0, plan: plan.and_then(|p| serde_json::from_str(&p).ok()),
            filters: x.get::<_, Option<String>>("filters").ok().flatten().and_then(|f| serde_json::from_str(&f).ok()).unwrap_or(Value::Null),
        })
    }

    pub fn get(&self, id: &str) -> Option<Monitor> {
        self.db.lock().unwrap().query_row("SELECT * FROM monitors WHERE id = ?1", [id], Self::row).optional().ok().flatten()
    }

    pub fn list(&self) -> Vec<Monitor> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM monitors ORDER BY created DESC") else { return vec![] };
        let mut ms: Vec<Monitor> = st.query_map([], Self::row).map(|r| r.flatten().collect()).unwrap_or_default();
        let day = now() - 24 * 3600 * 1000;
        for m in &mut ms {
            m.listings = db.query_row("SELECT COUNT(*) FROM listings WHERE monitor = ?1", [&m.id], |x| x.get(0)).unwrap_or(0);
            // The baseline run's listings aren't "new".
            m.new_24h = db.query_row("SELECT COUNT(*) FROM listings WHERE monitor = ?1 AND first_seen >= ?2 AND baseline = 0 AND COALESCE(matched, 1) = 1", params![m.id, day], |x| x.get(0)).unwrap_or(0);
        }
        ms
    }

    /// Monitors whose next run is due (active only), oldest first.
    pub fn due(&self) -> Vec<Monitor> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT * FROM monitors WHERE active = 1 AND next_run <= ?1 ORDER BY next_run") else { return vec![] };
        st.query_map([now()], Self::row).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// The monitor the user means: its id, else the best word overlap with name / request / site.
    pub fn find(&self, words: &str) -> Option<Monitor> {
        let all = self.list();
        if let Some(m) = all.iter().find(|m| m.id == words.trim()) {
            return Some(m.clone());
        }
        let toks = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_string).collect::<std::collections::HashSet<_>>();
        let want = toks(words);
        let best = all.iter().map(|m| (toks(&format!("{} {} {}", m.name, m.request, m.site)).intersection(&want).count(), m)).max_by_key(|(n, m)| (*n, m.created));
        match best {
            Some((n, m)) if n > 0 || all.len() == 1 => Some(m.clone()),
            _ => None,
        }
    }

    pub fn set_active(&self, id: &str, on: bool) -> bool {
        let db = self.db.lock().unwrap();
        // Resuming runs soon; failures start over.
        db.execute("UPDATE monitors SET active = ?2, fails = CASE WHEN ?2 = 1 THEN 0 ELSE fails END, next_run = CASE WHEN ?2 = 1 THEN ?3 ELSE next_run END, last_status = CASE WHEN ?2 = 1 THEN last_status ELSE 'paused' END WHERE id = ?1", params![id, on as i64, now()]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn set_interval(&self, id: &str, minutes: i64) -> bool {
        let minutes = if self.get(id).is_some_and(|m| crate::browser::linkedin::is_linkedin(&m.url)) { minutes.max(crate::browser::linkedin::MIN_INTERVAL) } else { minutes };
        self.db.lock().unwrap().execute("UPDATE monitors SET interval_min = ?2 WHERE id = ?1", params![id, minutes.max(10)]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn run_now(&self, id: &str) -> bool {
        self.db.lock().unwrap().execute("UPDATE monitors SET next_run = ?2 WHERE id = ?1", params![id, now()]).map(|n| n > 0).unwrap_or(false)
    }

    pub fn delete(&self, id: &str) -> bool {
        let db = self.db.lock().unwrap();
        for t in ["listings", "prices", "runs"] {
            let _ = db.execute(&format!("DELETE FROM {t} WHERE monitor = ?1"), [id]);
        }
        db.execute("DELETE FROM monitors WHERE id = ?1", [id]).map(|n| n > 0).unwrap_or(false)
    }

    /// Stores the judge's verdicts for a monitor's listings.
    pub fn set_matched(&self, id: &str, verdicts: &[(String, bool)]) {
        let mut db = self.db.lock().unwrap();
        let Ok(tx) = db.transaction() else { return };
        for (item, ok) in verdicts {
            let _ = tx.execute("UPDATE listings SET matched = ?3 WHERE monitor = ?1 AND item = ?2", params![id, item, *ok as i64]);
        }
        let _ = tx.commit();
    }

    /// A monitor over several search URLs with its filters ({"min", "max", "words", "urls",
    /// "city", "judge"}); `url` is the main search.
    pub fn create_full(&self, name: &str, request: &str, url: &str, interval_min: i64, filters: &Value) -> anyhow::Result<String> {
        let id = self.create_with(name, request, url, interval_min, None, None, &[])?;
        let _ = self.db.lock().unwrap().execute("UPDATE monitors SET filters = ?2 WHERE id = ?1", params![id, filters.to_string()]);
        Ok(id)
    }

    pub fn save_plan(&self, id: &str, plan: &Value) {
        let _ = self.db.lock().unwrap().execute("UPDATE monitors SET plan = ?2 WHERE id = ?1", params![id, plan.to_string()]);
    }

    /// Stores a run's listings and returns what is new / changed. Listings without an id are
    /// skipped (no dedup key, no alert).
    pub fn apply(&self, id: &str, items: &[Listing]) -> Diff {
        let mut db = self.db.lock().unwrap();
        let t = now();
        let baseline: bool = db.query_row("SELECT COUNT(*) FROM listings WHERE monitor = ?1", [id], |x| x.get::<_, i64>(0)).unwrap_or(0) == 0;
        let last_run: Option<i64> = db.query_row("SELECT last_run FROM monitors WHERE id = ?1", [id], |x| x.get(0)).ok().flatten();
        // Newest-first pages put fresh listings on top; without a date, only the top counts.
        let top = (items.len() / 3).max(5);
        let wall = chrono::Local::now();
        let mut d = Diff { found: items.len(), baseline, ..Default::default() };
        let Ok(tx) = db.transaction() else { return d };
        let mut seen = std::collections::HashSet::new();
        for (rank, it) in items.iter().enumerate().filter(|(_, i)| !i.id.trim().is_empty()) {
            if !seen.insert(it.id.clone()) {
                continue;
            }
            let pn = price_num(&it.price);
            let old: Option<String> = tx.query_row("SELECT price FROM listings WHERE monitor = ?1 AND item = ?2", params![id, it.id], |x| x.get(0)).optional().ok().flatten();
            match old {
                None => {
                    // Fresh = posted after the last check (2 h slack for clock / rounding), or,
                    // without a date, near the top of the page.
                    let fresh = !baseline && match (posted_ms(&it.posted, wall), last_run) {
                        (Some(p), Some(lr)) => p >= lr - 2 * 3_600_000,
                        (Some(_), None) => true,
                        (None, _) => rank < top,
                    };
                    // baseline: 1 = there at the start, 2 = appeared but not fresh, 0 = new.
                    let flag = if baseline { 1 } else if fresh { 0 } else { 2 };
                    let _ = tx.execute(
                        "INSERT INTO listings (monitor, item, url, title, price, price_num, location, posted, first_seen, last_seen, baseline) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                        params![id, it.id, it.url, it.title, it.price, pn, it.location, it.posted, t, flag],
                    );
                    let _ = tx.execute("INSERT INTO prices (monitor, item, at, price, price_num) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, it.id, t, it.price, pn]);
                    if fresh {
                        d.new.push(it.clone());
                    } else if !baseline {
                        d.quiet.push(it.clone());
                    }
                }
                Some(old) => {
                    let changed = !it.price.is_empty() && price_num(&old) != pn;
                    // Fill in fields a weaker run left empty; keep the rest stable.
                    let _ = tx.execute(
                        "UPDATE listings SET last_seen = ?3, seen_runs = seen_runs + 1, price = CASE WHEN ?4 = '' THEN price ELSE ?4 END, price_num = COALESCE(?5, price_num),
                         title = CASE WHEN title = '' THEN ?6 ELSE title END, location = CASE WHEN location = '' THEN ?7 ELSE location END, url = CASE WHEN url = '' THEN ?8 ELSE url END
                         WHERE monitor = ?1 AND item = ?2",
                        params![id, it.id, t, it.price, pn, it.title, it.location, it.url],
                    );
                    if changed {
                        let _ = tx.execute("INSERT INTO prices (monitor, item, at, price, price_num) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, it.id, t, it.price, pn]);
                        d.changed.push((it.clone(), old));
                    }
                }
            }
        }
        let _ = tx.commit();
        d
    }

    /// Records a run and schedules the next one (failures back off: ×2 per failure, ≤ 12 h).
    pub fn finish_run(&self, id: &str, d: &Diff, tokens: u64, ms: u64, error: Option<&str>) {
        let db = self.db.lock().unwrap();
        let t = now();
        let _ = db.execute("INSERT INTO runs (monitor, at, found, new, changed, tokens, ms, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, t, d.found as i64, d.new.len() as i64, d.changed.len() as i64, tokens as i64, ms as i64, error]);
        let (interval, fails): (i64, i64) = db.query_row("SELECT interval_min, fails FROM monitors WHERE id = ?1", [id], |x| Ok((x.get(0)?, x.get(1)?))).unwrap_or((60, 0));
        let fails = if error.is_some() { fails + 1 } else { 0 };
        let base = interval * 60_000 * (1i64 << fails.min(6));
        // ±10 % jitter so many monitors don't hit one site in lockstep.
        let jitter = (t % 2001 - 1000) as f64 / 10_000.0;
        let next = t + ((base as f64) * (1.0 + jitter)).min(12.0 * 3_600_000.0) as i64;
        let status = match error { Some(_) => "failed", None if d.baseline => "baseline", None => "ok" };
        let _ = db.execute("UPDATE monitors SET last_run = ?2, next_run = ?3, last_status = ?4, last_error = ?5, fails = ?6, runs = runs + 1 WHERE id = ?1",
            params![id, t, next, status, error, fails]);
    }

    /// Stored listings, newest first; `since` (ms) limits to first seen after it.
    pub fn listings(&self, id: &str, since: Option<i64>, limit: usize) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT item, url, title, price, location, posted, first_seen, last_seen, seen_runs, baseline FROM listings WHERE monitor = ?1 AND first_seen >= ?2 AND COALESCE(matched, 1) = 1 ORDER BY first_seen DESC, rowid LIMIT ?3") else { return vec![] };
        st.query_map(params![id, since.unwrap_or(0), limit as i64], |x| {
            let item: String = x.get(0)?;
            let fs: i64 = x.get(6)?;
            Ok(json!({"id": item, "url": x.get::<_, String>(1)?, "title": x.get::<_, String>(2)?, "price": x.get::<_, String>(3)?, "location": x.get::<_, String>(4)?,
                      "posted": x.get::<_, String>(5)?, "first_seen": fs, "last_seen": x.get::<_, i64>(7)?, "seen_runs": x.get::<_, i64>(8)?, "baseline": x.get::<_, i64>(9)? != 0, "quiet": x.get::<_, i64>(9)? == 2}))
        }).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn recent_runs(&self, id: &str, n: usize) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT at, found, new, changed, tokens, ms, error FROM runs WHERE monitor = ?1 ORDER BY at DESC LIMIT ?2") else { return vec![] };
        st.query_map(params![id, n as i64], |x| Ok(json!({"at": x.get::<_, i64>(0)?, "found": x.get::<_, i64>(1)?, "new": x.get::<_, i64>(2)?, "changed": x.get::<_, i64>(3)?, "tokens": x.get::<_, i64>(4)?, "ms": x.get::<_, i64>(5)?, "error": x.get::<_, Option<String>>(6)?})))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(id: &str, price: &str) -> Listing {
        Listing { id: id.into(), title: format!("Flat {id}"), price: price.into(), ..Default::default() }
    }

    #[test]
    fn posted_dates() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 10, 10, 14, 0, 0).unwrap();
        let ms = |s: &str| super::posted_ms(s, now).map(|x| chrono::Local.timestamp_millis_opt(x).unwrap().format("%Y-%m-%d %H:%M").to_string());
        assert_eq!(ms("Heute, 12:34").as_deref(), Some("2026-10-10 12:34"));
        assert_eq!(ms("Gestern, 13:27").as_deref(), Some("2026-10-09 13:27"));
        assert_eq!(ms("10. Oct. 12:03").as_deref(), Some("2026-10-10 12:03"));
        assert_eq!(ms("28. Dez. 09:00").as_deref(), Some("2025-12-28 09:00"));
        assert_eq!(ms("05.10.2026").as_deref(), Some("2026-10-05 00:00"));
        assert_eq!(ms("vor 3 Stunden").as_deref(), Some("2026-10-10 11:00"));
        assert!(ms("2026-10-10T13:18:53Z").is_some());
        assert_eq!(ms(""), None);
    }

    #[test]
    fn freshness() {
        let d = std::env::temp_dir().join(format!("mon-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("monitors.db"));
        let s = Store::open(&d).unwrap();
        let id = s.create("Laptops", "laptops", "https://www.kleinanzeigen.de/s-x/k0", 30).unwrap();
        let base: Vec<Listing> = (0..12).map(|i| l(&format!("b{i}"), "€ 100")).collect();
        let r = s.apply(&id, &base);
        s.finish_run(&id, &r, 0, 1, None);
        let mut page: Vec<Listing> = base.clone();
        // Posted now (fresh), posted yesterday (old ad sliding in), undated on top, undated at the bottom.
        page.insert(0, Listing { posted: "Heute, 23:59".into(), ..l("fresh", "€ 90") });
        page.insert(1, l("top", "€ 80"));
        page.push(Listing { posted: "Gestern, 08:00".into(), ..l("old", "€ 70") });
        page.push(l("bottom", "€ 60"));
        let r = s.apply(&id, &page);
        let ids = |v: &[Listing]| v.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&r.new), vec!["fresh", "top"]);
        assert_eq!(ids(&r.quiet), vec!["old", "bottom"]);
        assert_eq!(s.list()[0].new_24h, 2, "quiet ones aren't counted as new");
    }

    #[test]
    fn prices() {
        for (s, n) in [("€ 1.200", 1200.0), ("1.238 €", 1238.0), ("EUR 886,19", 886.19), ("2.632,94 €", 2632.94), ("$20.00", 20.0), ("319 € VB", 319.0), ("1192", 1192.0)] {
            assert_eq!(price_num(s), Some(n), "{s}");
        }
        assert_eq!(price_num("VB"), None);
    }

    #[test]
    fn baseline_then_new_changed_and_bumped() {
        let d = std::env::temp_dir().join(format!("mon-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("monitors.db"));
        let s = Store::open(&d).unwrap();
        let id = s.create("Flats", "flats in Vienna", "https://www.willhaben.at/iad/immobilien/x", 30).unwrap();
        let r1 = s.apply(&id, &[l("1", "€ 900"), l("2", "€ 1.100")]);
        assert!(r1.baseline && r1.new.is_empty(), "first run is the baseline: no alerts");
        s.finish_run(&id, &r1, 0, 10, None);
        // "2" bumped to the top with a lower price, "3" is new, a duplicate row and an id-less row.
        let r2 = s.apply(&id, &[l("2", "€ 990"), l("3", "€ 700"), l("3", "€ 700"), l("", "€ 1"), l("1", "€ 900")]);
        assert_eq!(r2.new.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec!["3"]);
        assert_eq!(r2.changed.len(), 1);
        assert_eq!(r2.changed[0].1, "€ 1.100");
        s.finish_run(&id, &r2, 0, 10, None);
        let m = &s.list()[0];
        assert_eq!((m.listings, m.new_24h, m.runs), (3, 1, 2));
        assert!(m.next_run > now() + 20 * 60_000, "next run about one interval later");
        // Failures back off.
        s.finish_run(&id, &Diff::default(), 0, 1, Some("blocked"));
        assert!(s.get(&id).unwrap().next_run > now() + 50 * 60_000);
        assert!(s.set_active(&id, false) && s.due().is_empty());
        assert!(s.delete(&id) && s.list().is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// Runner: the scheduler checks due monitors once a minute and runs them one at a time in
// Sam's background browser (polite: one page per run, sites in random order of due time).

static STORE: std::sync::OnceLock<Option<std::sync::Arc<Store>>> = std::sync::OnceLock::new();

/// The shared store (data/monitors.db); None if it can't be opened.
pub fn store() -> Option<std::sync::Arc<Store>> {
    STORE.get_or_init(|| Store::open(&crate::thinker::data_dir()).map_err(|e| tracing::warn!("monitors unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

/// The search URLs of a monitor: its main URL plus the keyword-expansion URLs (filters.urls).
pub fn urls_of(m: &Monitor) -> Vec<String> {
    let mut u = vec![m.url.clone()];
    for x in m.filters["urls"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
        if !u.iter().any(|y| y == x) {
            u.push(x.to_string());
        }
    }
    u
}

/// The stored plan for one of the monitor's URLs: plans are kept per URL ({"by_url": {..}}); an
/// older monitor's plain plan belongs to its main URL. willhaben always uses the built-in plan.
fn plan_for(m: &Monitor, url: &str) -> Option<Value> {
    if let Some(p) = crate::browser::listings::builtin_plan(url) {
        return Some(p);
    }
    let p = m.plan.as_ref()?;
    match p.get("by_url") {
        Some(by) => by.get(url).cloned(),
        None => (url == m.url).then(|| p.clone()),
    }
}

/// One run: load every search URL of the monitor, apply its plan (learn it first / again if
/// needed), merge, apply the code rules, store and diff, then let the judge decide which new
/// listings match (monitors with `filters.judge`). Returns the diff and tokens spent.
pub async fn run_one(app: &crate::AppState, m: &Monitor) -> anyhow::Result<(Diff, u64)> {
    use crate::browser::{cdp::{Chrome, Visibility}, listings};
    static RUNNING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    // One run per monitor at a time (setup's baseline vs the scheduler, "check now" twice).
    if RUNNING.lock().unwrap().contains(&m.id) {
        anyhow::bail!("already checking");
    }
    RUNNING.lock().unwrap().push(m.id.clone());
    struct Done(String);
    impl Drop for Done {
        fn drop(&mut self) {
            RUNNING.lock().unwrap().retain(|x| x != &self.0);
        }
    }
    let _done = Done(m.id.clone());
    let st = store().ok_or_else(|| anyhow::anyhow!("no monitor store"))?;
    // The stored plan may be newer than `m` (learned by a run that just finished).
    let m = &st.get(&m.id).unwrap_or_else(|| m.clone());
    // Signed-in sites (LinkedIn) read in the accounts profile; everything else in the background one.
    let signed = crate::browser::linkedin::is_linkedin(&m.url) || m.url.contains("facebook.com/marketplace");
    let screen = if signed { crate::thinker::accounts_screen() } else { crate::config::env_num("BG_SCREEN", 1u16) };
    let chrome = Chrome::launch(&crate::thinker::data_dir(), screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let res = async {
        if signed {
            tab.goto(crate::browser::linkedin::HOME, std::time::Duration::from_secs(30)).await?;
            if !crate::browser::linkedin::signed_in(&tab).await {
                anyhow::bail!("LinkedIn isn't signed in: Connectors › LinkedIn › Link");
            }
            // People searches: new matching people (slug = id), paced like all LinkedIn views.
            if m.url.contains("/search/results/people") {
                let people = crate::browser::linkedin::search_page(&tab, &m.url).await?;
                let items: Vec<Listing> = people.into_iter().map(|p| Listing { id: p.slug, url: p.url, title: format!("{} — {}", p.name, p.headline), location: p.location, ..Default::default() }).collect();
                return Ok::<_, anyhow::Error>((st.apply(&m.id, &items), 0));
            }
        }
        // Price trackers (price_track): each product page is one "listing" whose price is
        // re-read every run; Store::apply records the history and reports changes.
        if let Some(products) = m.filters["products"].as_array() {
            let mut items = vec![];
            for (i, p) in products.iter().filter_map(|p| p.as_str()).enumerate() {
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(5000 + (now() % 5000) as u64)).await;
                }
                match product(&tab, p).await {
                    Ok(l) => items.push(l),
                    // A bot check on one product: skip it today, never ask the user from a schedule.
                    Err(e) => tracing::info!(monitor = m.id, url = p, "price check skipped: {e:#}"),
                }
            }
            if items.is_empty() {
                anyhow::bail!("no product page could be read this time");
            }
            return Ok((st.apply(&m.id, &items), 0));
        }
        if m.url.contains("facebook.com/marketplace") {
            crate::browser::sessions::check("facebook")?;
            crate::browser::sessions::take("facebook", urls_of(m).len() as u32)?;
            if !tab.has_cookie("https://www.facebook.com/", "c_user").await {
                anyhow::bail!("Facebook isn't signed in: Connectors › Facebook › Link");
            }
        }
        let mut tokens = 0u64;
        let mut per_url: Vec<Vec<Listing>> = vec![];
        let mut plans = serde_json::Map::new();
        let mut errors = vec![];
        for (i, url) in urls_of(m).iter().enumerate() {
            if i > 0 {
                // Same site, several searches: a short pause between page loads.
                tokio::time::sleep(std::time::Duration::from_millis(2500 + (now() % 2500) as u64)).await;
            }
            let got = async {
                listings::load(&tab, url).await?;
                let mut t = 0u64;
                let items = match plan_for(m, url) {
                    Some(p) => match listings::apply(&tab, &p).await {
                        Ok(items) => (items, p),
                        Err(e) => {
                            tracing::info!(monitor = m.id, url, "plan stale ({e:#}); re-learning");
                            let (p, tk) = listings::learn(app, &tab, app.cfg.provider).await?;
                            t += tk;
                            (listings::apply(&tab, &p).await?, p)
                        }
                    },
                    None => {
                        let (p, tk) = listings::learn(app, &tab, app.cfg.provider).await?;
                        t += tk;
                        (listings::apply(&tab, &p).await?, p)
                    }
                };
                Ok::<_, anyhow::Error>((items, t))
            }
            .await;
            match got {
                Ok(((items, p), t)) => {
                    tokens += t;
                    plans.insert(url.clone(), p);
                    per_url.push(items);
                }
                // One failing search doesn't sink the others; all failing does.
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if per_url.is_empty() {
            anyhow::bail!("{}", errors.first().cloned().unwrap_or_else(|| "no results page".into()));
        }
        st.save_plan(&m.id, &json!({"by_url": plans}));
        // Interleave the searches so "near the top of the page" keeps its meaning for undated pages.
        let mut items = vec![];
        let longest = per_url.iter().map(Vec::len).max().unwrap_or(0);
        for r in 0..longest {
            for list in &per_url {
                if let Some(x) = list.get(r) {
                    items.push(x.clone());
                }
            }
        }
        let items = within(items, m.filters["min"].as_f64(), m.filters["max"].as_f64());
        let words: Vec<String> = m.filters["words"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(str::to_string)).collect();
        let items = matching(items, &words);
        let items = rules(items, m.filters["city"].as_str().unwrap_or(""));
        let mut d = st.apply(&m.id, &items);
        // The judge decides which new listings match the user's request (monitor lab L1:
        // keyword search finds ~99 % of what the user wants but with a third look-alikes).
        if m.filters["judge"] == true {
            let todo: Vec<Listing> = if d.baseline { items.clone() } else { d.new.clone() };
            if !todo.is_empty() {
                let (verdicts, t) = judge(app, &m.request, &todo).await;
                tokens += t;
                st.set_matched(&m.id, &verdicts);
                let ok: std::collections::HashSet<&str> = verdicts.iter().filter(|(_, v)| *v).map(|(id, _)| id.as_str()).collect();
                d.new.retain(|l| ok.contains(l.id.as_str()));
                d.matched = Some(ok.len());
            }
        }
        Ok::<_, anyhow::Error>((d, tokens))
    }
    .await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    res
}

/// Postcode ranges of the cities whose Kleinanzeigen searches pad thin results with listings
/// from other towns (monitor lab L1: Gotha and Marburg in a Hamburg free search).
fn city_postcodes(city: &str) -> Option<(u32, u32)> {
    Some(match city {
        "berlin" => (10000, 14199),
        "hamburg" => (20000, 22799),
        "muenchen" => (80000, 81929),
        _ => return None,
    })
}

/// Code rules before the judge: sold listings, "looking for" / swap posts, and (Kleinanzeigen)
/// listings outside the asked city.
pub fn rules(items: Vec<Listing>, city: &str) -> Vec<Listing> {
    let sold = regex::Regex::new(r"(?i)\b(verkauft|sold)\b").unwrap();
    let suche = regex::Regex::new(r"(?i)^\s*(ich\s+)?(suche|tausche|gesucht)\b").unwrap();
    let range = city_postcodes(city);
    let pc = regex::Regex::new(r"\b(\d{5})\b").unwrap();
    items.into_iter().filter(|l| {
        if sold.is_match(&l.title) || suche.is_match(&l.title) {
            return false;
        }
        match (range, l.url.contains("kleinanzeigen.de"), pc.captures(&l.location)) {
            (Some((lo, hi)), true, Some(c)) => c[1].parse::<u32>().is_ok_and(|p| (lo..=hi).contains(&p)),
            _ => true,
        }
    }).collect()
}

const JUDGE_SYS: &str = "You check second-hand marketplace listings for a saved search. For EVERY numbered listing first name what is offered (t: 1-4 words), then decide whether it is the kind of item the buyer asked for. \
'New' in the request means newly posted listings, never the item's condition: used, worn or damaged items still match. Place, price and 'free' are already filtered: ignore them. \
Do NOT match: accessories or parts alone, look-alikes of another type, other models/sizes than the buyer named, and 'suche' (looking-for) posts. \
Reply ONLY with JSON {\"verdicts\": [{\"n\": 1, \"t\": \"…\", \"m\": true|false}, ...]} covering every listing.";

/// Judge v6 from the monitor lab (L1, exp-064: recall 0.99, precision 0.97 with the user's request
/// verbatim, title + 160 chars of description, the light model). → (listing id, matches) and tokens.
/// If the model is unreachable every listing counts as a match (fail open: never miss an item).
pub async fn judge(app: &crate::AppState, request: &str, items: &[Listing]) -> (Vec<(String, bool)>, u64) {
    let mut out = vec![];
    let mut tokens = 0u64;
    for chunk in items.chunks(40) {
        let cards: Vec<String> = chunk.iter().enumerate().map(|(k, l)| {
            let desc: String = l.desc.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
            if desc.is_empty() { format!("{}. {}", k + 1, l.title) } else { format!("{}. {} — {desc}", k + 1, l.title) }
        }).collect();
        let user = format!("Buyer's request: {request}\n\nListings:\n{}", cards.join("\n"));
        let verdicts: Option<std::collections::HashMap<usize, bool>> = match crate::llm::complete_tier(app, crate::llm::Tier::Light, JUDGE_SYS, &user, true, 4000).await {
            Ok((txt, u)) => {
                tokens += u.prompt + u.completion;
                let j: Option<Value> = txt.find('{').and_then(|i| serde_json::from_str(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok());
                j.and_then(|j| j["verdicts"].as_array().map(|vs| vs.iter().filter_map(|v| Some((v["n"].as_u64()? as usize, v["m"].as_bool().unwrap_or(false)))).collect()))
            }
            Err(e) => {
                tracing::warn!("monitor judge unavailable: {e:#}");
                None
            }
        };
        for (k, l) in chunk.iter().enumerate() {
            out.push((l.id.clone(), verdicts.as_ref().map(|v| v.get(&(k + 1)).copied().unwrap_or(false)).unwrap_or(true)));
        }
    }
    (out, tokens)
}

const EXPAND_SYS: &str = "You set up a saved search on a second-hand marketplace (willhaben.at / kleinanzeigen.de / ebay, German-language listings). \
Place, price and 'free' are applied as filters separately: NEVER put places, prices, 'gratis', 'zu verschenken', 'gebraucht' or similar into keywords. \
The site matches every word of a keyword against title + description, so keep each keyword 1-2 words. \
Reply ONLY with JSON {\"keywords\": [3-5 terms: the exact item word, its synonyms and common spellings sellers use, and one broader word that still contains most matches]}.";

/// Keyword expansion at monitor creation (monitor lab L1: one exact keyword reaches 86 % of
/// what users want, 3-5 filter-free keywords 99.8 %). → keywords (the query first) and tokens.
pub async fn expand(app: &crate::AppState, request: &str, query: &str) -> (Vec<String>, u64) {
    let mut kws = vec![query.trim().to_string()];
    let Ok((txt, u)) = crate::llm::complete_tier(app, crate::llm::Tier::Light, EXPAND_SYS, request, true, 1500).await else { return (kws, 0) };
    let j: Option<Value> = txt.find('{').and_then(|i| serde_json::from_str(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok());
    for k in j.as_ref().and_then(|j| j["keywords"].as_array()).into_iter().flatten().filter_map(|k| k.as_str()) {
        let k = k.trim();
        if !k.is_empty() && k.split_whitespace().count() <= 3 && !kws.iter().any(|x| x.eq_ignore_ascii_case(k)) {
            kws.push(k.to_string());
        }
    }
    kws.truncate(5);
    (kws, u.prompt + u.completion)
}

/// One product page as a tracked "listing": id = the URL, title, price and stock from the page's
/// own offer data (JSON-LD) or the shop's price element (Amazon, eBay). 0 tokens.
async fn product(tab: &crate::browser::cdp::Tab, url: &str) -> anyhow::Result<Listing> {
    tab.goto(url, std::time::Duration::from_secs(30)).await?;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let head = tab.eval("document.documentElement.outerHTML.slice(0, 20000)").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if crate::browser::extract::is_challenge(&head) {
        anyhow::bail!("bot check");
    }
    let offer = tab.eval(crate::browser::scrape::OFFER_JS).await.unwrap_or(Value::Null);
    let page = tab.eval(r#"(() => { const t = s => { const e = document.querySelector(s); return e ? (e.innerText || e.textContent || '').replace(/\s+/g, ' ').trim() : ''; };
        return {title: t('#productTitle') || t('h1') || document.title, price: t('#corePrice_feature_div .a-offscreen') || t('#apex_desktop .a-price .a-offscreen') || t('.x-price-primary'),
                stock: t('#availability') || t('.x-quantity__availability')}; })()"#).await.unwrap_or(Value::Null);
    let price = match (&offer["price"], offer["currency"].as_str()) {
        (Value::String(p), c) => format!("{p} {}", c.unwrap_or("")).trim().to_string(),
        (Value::Number(p), c) => format!("{p} {}", c.unwrap_or("")).trim().to_string(),
        _ => page["price"].as_str().unwrap_or("").to_string(),
    };
    if price_num(&price).is_none() {
        anyhow::bail!("no price on the page");
    }
    let stock = offer["availability"].as_str().map(str::to_string).unwrap_or_else(|| page["stock"].as_str().unwrap_or("").split('{').next().unwrap_or("").trim().to_string());
    Ok(Listing { id: url.to_string(), url: url.to_string(), title: page["title"].as_str().unwrap_or("").chars().take(140).collect(), price,
                 location: offer["seller"].as_str().map(|s| format!("Seller: {s}")).unwrap_or_default(), posted: stock, desc: String::new() })
}

/// Price trackers: which changes are worth an alert — any drop, or crossing the user's target
/// price (filters.below), or back in stock.
fn price_alerts(m: &Monitor, d: &Diff) -> Vec<(Listing, String)> {
    let below = m.filters["below"].as_f64();
    d.changed.iter().filter(|(l, old)| {
        let (n, o) = (price_num(&l.price), price_num(old));
        match (n, o, below) {
            (Some(n), Some(_), Some(b)) => n <= b,
            (Some(n), Some(o), None) => n < o,
            _ => false,
        }
    }).cloned().collect()
}

fn alert_text(m: &Monitor, d: &Diff) -> String {
    let mut s = String::new();
    for l in d.new.iter().take(30) {
        s.push_str(&format!("- **{}** — {}{}  \n  {}\n", l.title, l.price, if l.location.is_empty() { String::new() } else { format!(" · {}", l.location) }, l.url));
    }
    let drops: Vec<&(Listing, String)> = d.changed.iter().filter(|(l, old)| price_num(&l.price).zip(price_num(old)).is_some_and(|(n, o)| n < o)).collect();
    if !drops.is_empty() {
        s.push_str("\nPrice drops:\n");
        for (l, old) in drops.iter().take(10) {
            s.push_str(&format!("- **{}**: {} → {}  \n  {}\n", l.title, old, l.price, l.url));
        }
    }
    format!("{} new on {} for \"{}\".\n\n{s}", d.new.len(), m.site, m.name)
}

/// A presented result for the overview (no LLM: the listings are already structured).
fn alert_report(m: &Monitor, d: &Diff) -> Value {
    let sources: Vec<Value> = d.new.iter().take(30).enumerate().map(|(i, l)| json!({"n": i + 1, "title": l.title, "url": l.url, "domain": m.site})).collect();
    let highlights: Vec<Value> = d.new.iter().take(12).enumerate().map(|(i, l)| {
        let text = [l.price.as_str(), l.location.as_str(), l.posted.as_str()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
        json!({"title": l.title, "text": text, "tag": "new", "source": i + 1})
    }).collect();
    let prices: Vec<f64> = d.new.iter().filter_map(|l| price_num(&l.price)).collect();
    let mut facts = vec![json!({"icon": "sparkles", "label": "New", "value": d.new.len().to_string()}), json!({"icon": "clock", "label": "Checks", "value": format!("every {} min", m.interval_min)})];
    if let (Some(lo), Some(hi)) = (prices.iter().cloned().reduce(f64::min), prices.iter().cloned().reduce(f64::max)) {
        facts.push(json!({"icon": "eurosign", "label": "Price range", "value": if lo == hi { format!("{lo:.0}") } else { format!("{lo:.0} – {hi:.0}") }}));
    }
    json!({"title": format!("{} new · {}", d.new.len(), m.name), "subtitle": format!("{} · saved search", m.site), "summary": format!("New listings since the last check of {}.", m.site),
           "facts": facts, "highlights": highlights, "sections": [], "budget": [], "place": {"name": ""}, "sources": sources, "request": m.request, "monitor": m.id,
           "created": chrono::Local::now().to_rfc3339()})
}

/// Scheduler loop (started once by the task registry).
pub fn spawn(app: std::sync::Arc<crate::AppState>, log: crate::tasklog::TaskLog) {
    tokio::spawn(async move {
        // Let the server settle first.
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        loop {
            if let Some(st) = store() {
                for m in st.due() {
                    // A busy Mac skips this round; the monitor stays due.
                    if crate::governor::available_mb().is_some_and(|mb| mb < crate::governor::reserve_mb()) {
                        break;
                    }
                    let t0 = std::time::Instant::now();
                    let r = run_one(&app, &m).await;
                    let ms = t0.elapsed().as_millis() as u64;
                    match r {
                        Ok((d, tokens)) => {
                            tracing::info!(monitor = m.id, found = d.found, new = d.new.len(), changed = d.changed.len(), tokens, "monitor run");
                            st.finish_run(&m.id, &d, tokens, ms, None);
                            let drops = if m.filters["products"].is_array() { price_alerts(&m, &d) } else { vec![] };
                            if !drops.is_empty() {
                                let tid = log.start("monitor", "monitor", &format!("Price change: {}", m.name));
                                let lines: Vec<String> = drops.iter().map(|(l, old)| format!("- **{}**: {old} → {}  \n  {}", l.title, l.price, l.url)).collect();
                                log.finish(&tid, "done", &format!("{} price {} on \"{}\":\n\n{}", drops.len(), if drops.len() == 1 { "change" } else { "changes" }, m.name, lines.join("\n")));
                            }
                            if !d.new.is_empty() && !m.filters["products"].is_array() {
                                let tid = log.start("monitor", "monitor", &format!("New on {}: {}", m.site, m.name));
                                let rep = alert_report(&m, &d);
                                log.update(&tid, |r| {
                                    r.plan = vec!["monitor".into()];
                                    r.report = Some(rep);
                                    r.notes = vec![format!("Saved search: {}", m.url)];
                                });
                                log.finish(&tid, "done", &alert_text(&m, &d));
                            }
                        }
                        Err(e) if e.to_string() == "already checking" => {}
                        Err(e) => {
                            tracing::info!(monitor = m.id, "monitor run failed: {e:#}");
                            st.finish_run(&m.id, &Diff::default(), 0, ms, Some(&format!("{e:#}")));
                            // Third failure in a row: tell the user once (bot check, site change, offline).
                            if st.get(&m.id).is_some_and(|x| x.fails == 3) {
                                let tid = log.start("monitor", "monitor", &format!("Monitor needs a look: {}", m.name));
                                log.finish(&tid, "failed", &format!("Checking {} failed three times in a row: {e:#}. It keeps retrying less often; open the page once in Sam's browser if it asks for a check.", m.site));
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Search URLs for the common sites, built in code from the user's filters (lab: models invent
// category paths and drop sort / price parameters when they write these URLs themselves).

fn slug(s: &str) -> String {
    let s = s.to_lowercase().replace('ä', "ae").replace('ö', "oe").replace('ü', "ue").replace('ß', "ss");
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-")
}

/// Austrian city → willhaben / ImmoScout AT region path.
fn at_region(place: &str) -> Option<&'static str> {
    Some(match slug(place).as_str() {
        "wien" | "vienna" => "wien/wien",
        "graz" => "steiermark/graz",
        "linz" => "oberoesterreich/linz",
        "salzburg" => "salzburg/salzburg",
        "innsbruck" => "tirol/innsbruck",
        "klagenfurt" | "klagenfurt-am-woerthersee" => "kaernten/klagenfurt-am-woerthersee",
        "st-poelten" | "sankt-poelten" => "niederoesterreich/st-poelten",
        "bregenz" => "vorarlberg/bregenz",
        "eisenstadt" => "burgenland/eisenstadt",
        "villach" => "kaernten/villach",
        "wels" => "oberoesterreich/wels",
        _ => return None,
    })
}

/// German city → ImmoScout DE region path.
fn de_region(place: &str) -> Option<&'static str> {
    Some(match slug(place).as_str() {
        "berlin" => "berlin/berlin",
        "hamburg" => "hamburg/hamburg",
        "muenchen" | "munich" => "bayern/muenchen",
        "koeln" | "cologne" => "nordrhein-westfalen/koeln",
        "frankfurt" | "frankfurt-am-main" => "hessen/frankfurt-am-main",
        "stuttgart" => "baden-wuerttemberg/stuttgart",
        "duesseldorf" => "nordrhein-westfalen/duesseldorf",
        "leipzig" => "sachsen/leipzig",
        "dresden" => "sachsen/dresden",
        "nuernberg" | "nuremberg" => "bayern/nuernberg",
        "hannover" => "niedersachsen/hannover",
        "bremen" => "bremen/bremen",
        _ => return None,
    })
}

/// Extra search filters (things for sale).
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOpts {
    /// Only free items: willhaben "Nur zu verschenken", Kleinanzeigen "Zu verschenken & Tauschen".
    pub free: bool,
    /// Private sellers only (willhaben).
    pub private: bool,
}

/// willhaben `areaId` for a place (checked on the live site, 2026-10-18): the Bundesländer, Wien
/// (900) and the Vienna districts (117222 + district number), from a district number, a postcode
/// (1070) or "Wien 7" / "7. Bezirk"; other Austrian cities → their Bundesland.
pub fn willhaben_area(place: &str) -> Option<String> {
    let p = slug(place);
    let digits: String = p.chars().filter(|c| c.is_ascii_digit()).collect();
    let viennese = p.contains("wien") || p.contains("vienna") || p.contains("bezirk");
    let district = match digits.len() {
        4 if digits.starts_with('1') => digits[1..3].parse::<u32>().ok(),
        1 | 2 if viennese => digits.parse::<u32>().ok(),
        _ => None,
    };
    if let Some(n) = district.filter(|n| (1..=23).contains(n)) {
        return Some((117222 + n).to_string());
    }
    let state = |s: &str| Some(match s {
        "wien" | "vienna" => "900", "burgenland" => "1", "kaernten" => "2", "niederoesterreich" => "3", "oberoesterreich" => "4",
        "salzburg" => "5", "steiermark" => "6", "tirol" => "7", "vorarlberg" => "8", _ => return None,
    }.to_string());
    state(p.as_str()).or_else(|| at_region(place).and_then(|r| state(r.split('/').next().unwrap_or(""))))
}

/// Removes "zu verschenken", "gratis", "kostenlos", "umsonst", "for free", "gives away" from a
/// query → (cleaned query, whether the user asked for free items).
pub fn strip_free_words(q: &str) -> (String, bool) {
    let re = regex::Regex::new(r"(?i)\b(zu verschenken|verschenken|verschenkt|geschenkt|gratis|kostenlos|umsonst|for free|free|gives? away|giving away)\b").unwrap();
    let free = re.is_match(q);
    (re.replace_all(q, " ").split_whitespace().collect::<Vec<_>>().join(" "), free)
}

/// Kleinanzeigen location (slug, code) for the cities checked on the live site.
pub fn kleinanzeigen_city(place: &str) -> Option<(&'static str, &'static str)> {
    Some(match slug(place).as_str() {
        "berlin" => ("berlin", "l3331"),
        "hamburg" => ("hamburg", "l9409"),
        "muenchen" | "munich" => ("muenchen", "l6411"),
        _ => return None,
    })
}

/// `site`: willhaben | immoscout_at | immoscout_de | ebay_de | ebay_at | kleinanzeigen | amazon_de;
/// `kind`: market (things) | rent | buy (flats). Newest first wherever the site allows it.
pub fn search_url(site: &str, kind: &str, query: &str, place: &str, min: Option<f64>, max: Option<f64>) -> Option<String> {
    search_url_opts(site, kind, query, place, min, max, SearchOpts::default())
}

pub fn search_url_opts(site: &str, kind: &str, query: &str, place: &str, min: Option<f64>, max: Option<f64>, o: SearchOpts) -> Option<String> {
    let (min, max) = if o.free && kind == "market" { (Some(0.0), Some(0.0)) } else { (min, max) };
    let q: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    let n = |x: Option<f64>| x.map(|v| format!("{v:.0}")).unwrap_or_default();
    let flat = kind == "rent" || kind == "buy";
    Some(match site {
        "willhaben" if flat => {
            let base = if kind == "rent" { "mietwohnungen" } else { "eigentumswohnung" };
            let mut u = match (kind, slug(place).as_str()) {
                ("rent", "wien" | "vienna") => "https://www.willhaben.at/iad/immobilien/mietwohnungen/mietwohnung-angebote?areaId=900".to_string(),
                (_, "wien" | "vienna") => format!("https://www.willhaben.at/iad/immobilien/{base}/wien?"),
                _ => match at_region(place) {
                    Some(r) => format!("https://www.willhaben.at/iad/immobilien/{base}/{r}?"),
                    None => format!("https://www.willhaben.at/iad/immobilien/{base}/{base}-angebote?keyword={}", url::form_urlencoded::byte_serialize(place.as_bytes()).collect::<String>()),
                },
            };
            if !q.is_empty() { u.push_str(&format!("&keyword={q}")); }
            if min.is_some() { u.push_str(&format!("&PRICE_FROM={}", n(min))); }
            if max.is_some() { u.push_str(&format!("&PRICE_TO={}", n(max))); }
            u.push_str("&sort=1");
            u.replace("?&", "?")
        }
        "willhaben" => {
            let mut u = format!("https://www.willhaben.at/iad/kaufen-und-verkaufen/marktplatz?keyword={q}");
            if let Some(a) = willhaben_area(place) { u.push_str(&format!("&areaId={a}")); }
            if min.is_some() { u.push_str(&format!("&PRICE_FROM={}", n(min))); }
            if max.is_some() { u.push_str(&format!("&PRICE_TO={}", n(max))); }
            if o.private { u.push_str("&ISPRIVATE=1"); }
            u + "&sort=1"
        }
        "immoscout_de" => {
            let r = de_region(place)?;
            let what = if kind == "buy" { "wohnung-kaufen" } else { "wohnung-mieten" };
            let price = if min.is_some() || max.is_some() { format!("&price={}-{}", min.map(|v| format!("{v:.1}")).unwrap_or_default(), max.map(|v| format!("{v:.1}")).unwrap_or_default()) } else { String::new() };
            format!("https://www.immobilienscout24.de/Suche/de/{r}/{what}?sorting=2{price}")
        }
        // No reliable price / sort parameters: prices are filtered after reading the page.
        "immoscout_at" => format!("https://www.immobilienscout24.at/regional/{}/{}", at_region(place)?, if kind == "buy" { "wohnung-kaufen" } else { "wohnung-mieten" }),
        "ebay_de" | "ebay_at" => {
            let host = if site == "ebay_at" { "www.ebay.at" } else { "www.ebay.de" };
            let mut u = format!("https://{host}/sch/i.html?_nkw={q}&_sop=10");
            if min.is_some() { u.push_str(&format!("&_udlo={}", n(min))); }
            if max.is_some() { u.push_str(&format!("&_udhi={}", n(max))); }
            u
        }
        "kleinanzeigen" => {
            let city = kleinanzeigen_city(place);
            if o.free {
                // The "Zu verschenken & Tauschen" category (c272): every listing in it is free.
                return Some(match city {
                    Some((c, l)) => format!("https://www.kleinanzeigen.de/s-zu-verschenken-tauschen/{c}/{}/k0c272{l}", slug(query)),
                    None => format!("https://www.kleinanzeigen.de/s-zu-verschenken-tauschen/{}/k0c272", slug(query)),
                });
            }
            let price = if min.is_some() || max.is_some() { format!("preis:{}:{}/", n(min), n(max)) } else { String::new() };
            match city {
                Some((c, l)) => format!("https://www.kleinanzeigen.de/s-{c}/sortierung:neuste/{price}{}/k0{l}", slug(query)),
                None => format!("https://www.kleinanzeigen.de/s-sortierung:neuste/{price}{}/k0", slug(query)),
            }
        }
        // Facebook Marketplace (Sam's signed-in session): city page, newest first; free = max price 0.
        "fb_marketplace" | "facebook" => {
            let p = slug(place);
            let city = match p.as_str() {
                "wien" | "vienna" => "vienna", "berlin" => "berlin", "muenchen" | "munich" => "munich", "hamburg" => "hamburg", "graz" => "graz",
                "linz" => "linz", "salzburg" => "salzburg", "koeln" | "cologne" => "cologne", "frankfurt" => "frankfurt", s if !s.is_empty() => s, _ => "vienna",
            };
            let mut u = format!("https://www.facebook.com/marketplace/{city}/search/?query={q}&sortBy=creation_time_descend&exact=false");
            if min.is_some() { u.push_str(&format!("&minPrice={}", n(min))); }
            if max.is_some() { u.push_str(&format!("&maxPrice={}", n(max))); }
            u
        }
        "amazon_de" => {
            let mut u = format!("https://www.amazon.de/s?k={q}&s=date-desc-rank");
            if min.is_some() || max.is_some() {
                let c = |x: Option<f64>| x.map(|v| format!("{:.0}", v * 100.0)).unwrap_or_default();
                u.push_str(&format!("&rh=p_36%3A{}-{}", c(min), c(max)));
            }
            u
        }
        _ => return None,
    })
}

/// Listings within the monitor's price bounds (listings without a readable price stay).
pub fn within(items: Vec<Listing>, min: Option<f64>, max: Option<f64>) -> Vec<Listing> {
    items.into_iter().filter(|l| match price_num(&l.price) {
        Some(p) => min.is_none_or(|m| p >= m) && max.is_none_or(|m| p <= m),
        None => true,
    }).collect()
}

/// Search words a thing's title must contain (sites' keyword search also returns accessories
/// and look-alikes: "ThinkPad X1" → a docking station, a ThinkPad E15).
pub fn words_of(query: &str) -> Vec<String> {
    query.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(str::to_string).collect()
}

pub fn matching(items: Vec<Listing>, words: &[String]) -> Vec<Listing> {
    if words.is_empty() {
        return items;
    }
    items.into_iter().filter(|l| {
        let t = l.title.to_lowercase();
        words.iter().all(|w| t.contains(w.as_str()))
    }).collect()
}

#[cfg(test)]
mod url_tests {
    #[test]
    fn urls() {
        let u = |s, k, q, p, mx| super::search_url(s, k, q, p, None, mx).unwrap();
        assert_eq!(u("willhaben", "rent", "", "Wien", Some(1200.0)), "https://www.willhaben.at/iad/immobilien/mietwohnungen/mietwohnung-angebote?areaId=900&PRICE_TO=1200&sort=1");
        assert_eq!(u("willhaben", "rent", "", "Graz", Some(900.0)), "https://www.willhaben.at/iad/immobilien/mietwohnungen/steiermark/graz?PRICE_TO=900&sort=1");
        assert_eq!(u("willhaben", "buy", "", "wien", Some(400000.0)), "https://www.willhaben.at/iad/immobilien/eigentumswohnung/wien?PRICE_TO=400000&sort=1");
        assert_eq!(u("willhaben", "market", "thinkpad x1", "", Some(600.0)), "https://www.willhaben.at/iad/kaufen-und-verkaufen/marktplatz?keyword=thinkpad+x1&PRICE_TO=600&sort=1");
        let free = super::SearchOpts { free: true, ..Default::default() };
        assert_eq!(super::search_url_opts("willhaben", "market", "trenchcoat", "Wien", None, None, free).unwrap(), "https://www.willhaben.at/iad/kaufen-und-verkaufen/marktplatz?keyword=trenchcoat&areaId=900&PRICE_FROM=0&PRICE_TO=0&sort=1");
        assert_eq!(super::search_url_opts("willhaben", "market", "sofa", "1070 Wien", None, Some(50.0), super::SearchOpts { private: true, ..Default::default() }).unwrap(), "https://www.willhaben.at/iad/kaufen-und-verkaufen/marktplatz?keyword=sofa&areaId=117229&PRICE_TO=50&ISPRIVATE=1&sort=1");
        assert_eq!(super::willhaben_area("Graz").as_deref(), Some("6"));
        assert_eq!(super::search_url_opts("fb_marketplace", "market", "sofa", "Wien", None, None, free).unwrap(), "https://www.facebook.com/marketplace/vienna/search/?query=sofa&sortBy=creation_time_descend&exact=false&minPrice=0&maxPrice=0");
        assert_eq!(super::strip_free_words("Trenchcoat zu verschenken"), ("Trenchcoat".to_string(), true));
        assert_eq!(super::strip_free_words("sofa"), ("sofa".to_string(), false));
        assert_eq!(super::willhaben_area("Wien 22").as_deref(), Some("117244"));
        assert_eq!(super::search_url_opts("kleinanzeigen", "market", "Fahrrad", "Berlin", None, None, free).unwrap(), "https://www.kleinanzeigen.de/s-zu-verschenken-tauschen/berlin/fahrrad/k0c272l3331");
        assert_eq!(u("kleinanzeigen", "market", "schreibtisch", "München", Some(200.0)), "https://www.kleinanzeigen.de/s-muenchen/sortierung:neuste/preis::200/schreibtisch/k0l6411");
        assert_eq!(u("immoscout_de", "rent", "", "München", Some(1500.0)), "https://www.immobilienscout24.de/Suche/de/bayern/muenchen/wohnung-mieten?sorting=2&price=-1500.0");
        assert_eq!(u("ebay_at", "market", "thinkpad x1", "", Some(600.0)), "https://www.ebay.at/sch/i.html?_nkw=thinkpad+x1&_sop=10&_udhi=600");
        assert_eq!(u("kleinanzeigen", "market", "thinkpad x1", "", Some(600.0)), "https://www.kleinanzeigen.de/s-sortierung:neuste/preis::600/thinkpad-x1/k0");
        assert_eq!(u("amazon_de", "market", "thinkpad x1", "", Some(600.0)), "https://www.amazon.de/s?k=thinkpad+x1&s=date-desc-rank&rh=p_36%3A-60000");
        assert!(super::search_url("immoscout_de", "rent", "", "Kleinkleckersdorf", None, None).is_none());
        let l = |p: &str| super::Listing { id: p.into(), price: p.into(), ..Default::default() };
        let kept: Vec<String> = super::within(vec![l("€ 500"), l("€ 1.300"), l("VB")], None, Some(1200.0)).into_iter().map(|x| x.id).collect();
        assert_eq!(kept, vec!["€ 500", "VB"]);
        let t = |x: &str| super::Listing { id: x.into(), title: x.into(), ..Default::default() };
        let w = super::words_of("ThinkPad X1");
        let kept: Vec<String> = super::matching(vec![t("Lenovo ThinkPad X1 Carbon G9"), t("Lenovo Docking Station"), t("ThinkPad E15")], &w).into_iter().map(|x| x.id).collect();
        assert_eq!(kept, vec!["Lenovo ThinkPad X1 Carbon G9"]);
    }
}
