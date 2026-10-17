//! Lead finding for a product on LinkedIn (leads lab). From a product link or idea:
//! 1. product text (the page, or the idea as given) → ONE LLM call: buyer profile (roles,
//!    industries, places, exclusions) + a few people-search keyword queries;
//! 2. the searches (paced, Sam's own LinkedIn session) → candidates, deduped by profile slug;
//! 3. ONE batch LLM call scores every candidate from its search card (headline, location,
//!    current / past role);
//! 4. the best K get their profile opened (About, Experience, recent activity) and are re-scored
//!    with a reason, an outreach angle and, if one fits, a recent post to engage with.
//! Leads are stored per campaign (data/leads.db) — new runs only add people not seen before.

use std::{path::Path, sync::Mutex};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{AppState, browser::{cdp::Tab, linkedin::{self, Lead}}, llm::{self, Provider}};

const ICP_SYS: &str = "You plan B2B lead research on LinkedIn for a product. From the product description, reply ONLY with JSON: \
{\"product\": \"one sentence: what it is and for whom\", \
\"buyers\": [{\"role\": \"job title of a buyer / decision maker / champion\", \"why\": \"short\"}], \
\"industries\": [\"…\"], \"places\": [\"countries or cities if the product implies them, else []\"], \
\"exclude\": [\"who is NOT a fit: e.g. students, recruiters, vendors of the same product\"], \
\"searches\": [\"LinkedIn people-search keywords, 2-4 words: a BUYER job title + an industry word (+ country / city), in the language people write their titles in\"]}. \
3-6 buyers, 3-5 searches, most promising first. Searches name decision makers (Geschäftsführer, Head of …, Vertriebsleiter, Leiter …), never tasks or clerk roles \
(lab: task phrases like 'Speditionskaufmann Export Angebot' find clerks, not buyers). No invented facts about the product.";

const SCORE_SYS: &str = "You score LinkedIn search results as leads for a product. For each person (one line: i | name | headline | location | current/past role), \
judge from the line only: are they a likely buyer, decision maker or strong champion for THIS product at a company that would need it? \
Reply ONLY with JSON {\"scores\": [{\"i\": n, \"score\": 0-100, \"reason\": \"max 12 words\"}]} for every line. \
90+: exact buyer role at a matching company; 60-89: plausible; 30-59: adjacent; <30: not a fit (wrong role, industry, student, recruiter, competitor).";

const ENRICH_SYS: &str = "You judge leads for a product from their LinkedIn profiles (top card, About, Experience, recent activity). \
For each lead reply with a fit score, a reason, and an outreach angle that refers to something concrete in THEIR profile or posts (no flattery, no invention). \
Reply ONLY with JSON {\"leads\": [{\"i\": n, \"score\": 0-100, \"reason\": \"max 20 words\", \"angle\": \"one sentence hook for a first message\", \"post\": \"url of one of their own recent posts worth a thoughtful comment, or empty\"}]}.";

pub struct Store {
    db: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS campaigns (id TEXT PRIMARY KEY, product TEXT NOT NULL, input TEXT NOT NULL, icp TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS leads (
  campaign TEXT NOT NULL, slug TEXT NOT NULL, name TEXT NOT NULL, headline TEXT NOT NULL, location TEXT NOT NULL, summary TEXT NOT NULL,
  degree TEXT NOT NULL, action TEXT NOT NULL, url TEXT NOT NULL, query TEXT NOT NULL,
  card_score INTEGER, score INTEGER, reason TEXT NOT NULL DEFAULT '', angle TEXT NOT NULL DEFAULT '', post TEXT NOT NULL DEFAULT '',
  profile TEXT, posts TEXT,
  -- new | drafted | contacted | replied | skipped
  status TEXT NOT NULL DEFAULT 'new',
  first_seen INTEGER NOT NULL, updated INTEGER NOT NULL,
  PRIMARY KEY (campaign, slug)
);
CREATE TABLE IF NOT EXISTS outreach (id INTEGER PRIMARY KEY, campaign TEXT, slug TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, target TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, at INTEGER NOT NULL);
";

/// Daily caps for sent outreach — well below LinkedIn's own limits.
pub fn cap(kind: &str) -> i64 {
    match kind {
        "connect" => crate::config::env_num("LINKEDIN_CONNECTS_PER_DAY", 15),
        "message" => crate::config::env_num("LINKEDIN_MESSAGES_PER_DAY", 25),
        _ => crate::config::env_num("LINKEDIN_COMMENTS_PER_DAY", 15),
    }
}

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

static STORE: std::sync::OnceLock<Option<std::sync::Arc<Store>>> = std::sync::OnceLock::new();

pub fn store() -> Option<std::sync::Arc<Store>> {
    STORE.get_or_init(|| Store::open(&crate::thinker::data_dir()).map_err(|e| tracing::warn!("leads unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

impl Store {
    pub fn open(data: &Path) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("leads.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL;")?;
        db.execute_batch(SCHEMA)?;
        // Long-running campaigns (added later): target size, query grid + cursor, pacing.
        for col in ["target INTEGER", "state TEXT", "grid TEXT", "cursor TEXT", "daily_views INTEGER", "next_run INTEGER", "note TEXT", "milestone INTEGER", "session TEXT", "batch TEXT"] {
            let _ = db.execute(&format!("ALTER TABLE campaigns ADD COLUMN {col}"), []);
        }
        Ok(Self { db: Mutex::new(db) })
    }

    /// Turns a campaign into a long-running one (state "active").
    pub fn start_campaign(&self, id: &str, target: i64, grid: &[String], daily_views: i64, session: &str) {
        let _ = self.db.lock().unwrap().execute("UPDATE campaigns SET target = ?2, state = 'active', grid = ?3, cursor = ?4, daily_views = ?5, next_run = ?6, milestone = 0, session = ?7 WHERE id = ?1",
            params![id, target, json!(grid).to_string(), json!({"q": 0, "page": 1}).to_string(), daily_views, now(), session]);
    }

    pub fn campaign_row(&self, id: &str) -> Option<Value> {
        self.db.lock().unwrap().query_row("SELECT id, product, input, target, state, grid, cursor, daily_views, next_run, note, milestone, session, icp, (SELECT COUNT(*) FROM leads l WHERE l.campaign = c.id), (SELECT COUNT(*) FROM leads l WHERE l.campaign = c.id AND l.score >= 70), (SELECT COUNT(*) FROM leads l WHERE l.campaign = c.id AND l.card_score IS NULL) FROM campaigns c WHERE id = ?1", [id], |x| {
            let j = |i: usize| x.get::<_, Option<String>>(i).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
            Ok(json!({"id": x.get::<_, String>(0)?, "product": x.get::<_, String>(1)?, "input": x.get::<_, String>(2)?, "target": x.get::<_, Option<i64>>(3)?, "state": x.get::<_, Option<String>>(4)?,
                      "grid": j(5), "cursor": j(6), "daily_views": x.get::<_, Option<i64>>(7)?, "next_run": x.get::<_, Option<i64>>(8)?, "note": x.get::<_, Option<String>>(9)?, "milestone": x.get::<_, Option<i64>>(10)?,
                      "session": x.get::<_, Option<String>>(11)?, "icp": j(12), "leads": x.get::<_, i64>(13)?, "good": x.get::<_, i64>(14)?, "unscored": x.get::<_, i64>(15)?}))
        }).optional().ok().flatten()
    }

    pub fn active_campaigns(&self) -> Vec<String> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT id FROM campaigns WHERE state = 'active' AND COALESCE(next_run, 0) <= ?1") else { return vec![] };
        st.query_map([now()], |x| x.get::<_, String>(0)).map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn set_campaign(&self, id: &str, state: Option<&str>, cursor: Option<&Value>, next_run: Option<i64>, note: Option<&str>, grid: Option<&[String]>) {
        let db = self.db.lock().unwrap();
        if let Some(s) = state { let _ = db.execute("UPDATE campaigns SET state = ?2 WHERE id = ?1", params![id, s]); }
        if let Some(c) = cursor { let _ = db.execute("UPDATE campaigns SET cursor = ?2 WHERE id = ?1", params![id, c.to_string()]); }
        if let Some(n) = next_run { let _ = db.execute("UPDATE campaigns SET next_run = ?2 WHERE id = ?1", params![id, n]); }
        if let Some(n) = note { let _ = db.execute("UPDATE campaigns SET note = ?2 WHERE id = ?1", params![id, n]); }
        if let Some(g) = grid { let _ = db.execute("UPDATE campaigns SET grid = ?2 WHERE id = ?1", params![id, json!(g).to_string()]); }
    }

    /// The pending scoring batch of a campaign: {"id": batch id, "slugs": [[chunk slugs]…]}.
    pub fn batch(&self, id: &str) -> Option<Value> {
        let s: Option<String> = self.db.lock().unwrap().query_row("SELECT batch FROM campaigns WHERE id = ?1", [id], |x| x.get(0)).optional().ok().flatten().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok()).filter(|v: &Value| !v.is_null())
    }

    pub fn set_batch(&self, id: &str, b: Option<&Value>) {
        let _ = self.db.lock().unwrap().execute("UPDATE campaigns SET batch = ?2 WHERE id = ?1", params![id, b.map(|v| v.to_string())]);
    }

    pub fn set_milestone(&self, id: &str, m: i64) {
        let _ = self.db.lock().unwrap().execute("UPDATE campaigns SET milestone = ?2 WHERE id = ?1", params![id, m]);
    }

    /// Leads without a card score (scored in batches after each slice).
    pub fn unscored(&self, campaign: &str, n: usize) -> Vec<Lead> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT slug, name, headline, location, summary, degree, action, url FROM leads WHERE campaign = ?1 AND card_score IS NULL LIMIT ?2") else { return vec![] };
        st.query_map(params![campaign, n as i64], |x| Ok(Lead { slug: x.get(0)?, name: x.get(1)?, headline: x.get(2)?, location: x.get(3)?, summary: x.get(4)?, degree: x.get(5)?, action: x.get(6)?, url: x.get(7)? }))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Best card-scored leads whose profile wasn't checked yet.
    pub fn to_enrich(&self, campaign: &str, n: usize) -> Vec<Lead> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT slug, name, headline, location, summary, degree, action, url FROM leads WHERE campaign = ?1 AND profile IS NULL AND card_score >= 70 ORDER BY card_score DESC LIMIT ?2") else { return vec![] };
        st.query_map(params![campaign, n as i64], |x| Ok(Lead { slug: x.get(0)?, name: x.get(1)?, headline: x.get(2)?, location: x.get(3)?, summary: x.get(4)?, degree: x.get(5)?, action: x.get(6)?, url: x.get(7)? }))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// CSV of the campaign (best first) → path.
    pub fn export_csv(&self, campaign: &str, dir: &Path) -> anyhow::Result<std::path::PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{campaign}.csv"));
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let mut out = String::from("name,headline,location,current_or_past,score,reason,angle,profile_url,status\n");
        for l in self.leads(campaign, 1_000_000) {
            let st = |k: &str| l[k].as_str().unwrap_or("").to_string();
            out.push_str(&format!("{},{},{},{},{},{},{},{},{}\n", q(&st("name")), q(&st("headline")), q(&st("location")), q(&st("summary")), l["score"].as_i64().map(|s| s.to_string()).unwrap_or_default(), q(&st("reason")), q(&st("angle")), q(&st("url")), st("status")));
        }
        std::fs::write(&path, out)?;
        Ok(path)
    }

    pub fn campaign(&self, input: &str, icp: &Value) -> anyhow::Result<String> {
        let id = format!("c{}", now());
        self.db.lock().unwrap().execute("INSERT INTO campaigns (id, product, input, icp, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, icp["product"].as_str().unwrap_or(input), input, icp.to_string(), now()])?;
        Ok(id)
    }

    /// Adds leads not seen in this campaign yet; returns how many were new.
    pub fn add(&self, campaign: &str, leads: &[(Lead, String)]) -> usize {
        let db = self.db.lock().unwrap();
        let t = now();
        leads.iter().filter(|(l, q)| db.execute(
            "INSERT OR IGNORE INTO leads (campaign, slug, name, headline, location, summary, degree, action, url, query, first_seen, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![campaign, l.slug, l.name, l.headline, l.location, l.summary, l.degree, l.action, l.url, q, t]).map(|n| n > 0).unwrap_or(false)).count()
    }

    pub fn set_card_score(&self, campaign: &str, slug: &str, score: i64, reason: &str) {
        let _ = self.db.lock().unwrap().execute("UPDATE leads SET card_score = ?3, score = COALESCE(score, ?3), reason = CASE WHEN reason = '' THEN ?4 ELSE reason END, updated = ?5 WHERE campaign = ?1 AND slug = ?2",
            params![campaign, slug, score, reason, now()]);
    }

    pub fn set_enriched(&self, campaign: &str, slug: &str, profile: &Value, posts: &Value, score: i64, reason: &str, angle: &str, post: &str) {
        let _ = self.db.lock().unwrap().execute("UPDATE leads SET profile = ?3, posts = ?4, score = ?5, reason = ?6, angle = ?7, post = ?8, updated = ?9 WHERE campaign = ?1 AND slug = ?2",
            params![campaign, slug, profile.to_string(), posts.to_string(), score, reason, angle, post, now()]);
    }

    /// The campaign's leads, best first.
    pub fn leads(&self, campaign: &str, limit: usize) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT slug, name, headline, location, summary, degree, action, url, card_score, score, reason, angle, post, status, profile IS NOT NULL FROM leads WHERE campaign = ?1 ORDER BY COALESCE(score, -1) DESC, first_seen LIMIT ?2") else { return vec![] };
        st.query_map(params![campaign, limit as i64], |x| Ok(json!({
            "slug": x.get::<_, String>(0)?, "name": x.get::<_, String>(1)?, "headline": x.get::<_, String>(2)?, "location": x.get::<_, String>(3)?, "summary": x.get::<_, String>(4)?,
            "degree": x.get::<_, String>(5)?, "action": x.get::<_, String>(6)?, "url": x.get::<_, String>(7)?, "card_score": x.get::<_, Option<i64>>(8)?, "score": x.get::<_, Option<i64>>(9)?,
            "reason": x.get::<_, String>(10)?, "angle": x.get::<_, String>(11)?, "post": x.get::<_, String>(12)?, "status": x.get::<_, String>(13)?, "enriched": x.get::<_, bool>(14)?})))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn campaigns(&self) -> Vec<Value> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT c.id, c.product, c.input, c.created, (SELECT COUNT(*) FROM leads l WHERE l.campaign = c.id), (SELECT COUNT(*) FROM leads l WHERE l.campaign = c.id AND l.score >= 70), c.state, c.target, c.note, c.next_run FROM campaigns c ORDER BY c.created DESC") else { return vec![] };
        st.query_map([], |x| Ok(json!({"id": x.get::<_, String>(0)?, "product": x.get::<_, String>(1)?, "input": x.get::<_, String>(2)?, "created": x.get::<_, i64>(3)?, "leads": x.get::<_, i64>(4)?, "good": x.get::<_, i64>(5)?,
                                       "state": x.get::<_, Option<String>>(6)?, "target": x.get::<_, Option<i64>>(7)?, "note": x.get::<_, Option<String>>(8)?, "next_run": x.get::<_, Option<i64>>(9)?})))
            .map(|r| r.flatten().collect()).unwrap_or_default()
    }

    pub fn icp(&self, campaign: &str) -> Option<Value> {
        let s: Option<String> = self.db.lock().unwrap().query_row("SELECT icp FROM campaigns WHERE id = ?1", [campaign], |x| x.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Today's sent outreach of a kind (connect | message | comment).
    pub fn sent_today(&self, kind: &str) -> i64 {
        let start = chrono::Local::now().date_naive().and_hms_opt(0, 0, 0).and_then(|d| d.and_local_timezone(chrono::Local).single()).map(|d| d.timestamp_millis()).unwrap_or(0);
        self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM outreach WHERE kind = ?1 AND status = 'sent' AND at >= ?2", params![kind, start], |x| x.get(0)).unwrap_or(0)
    }

    /// Err when today's cap for `kind` is reached.
    pub fn check_cap(&self, kind: &str) -> anyhow::Result<()> {
        let cap = cap(kind);
        if self.sent_today(kind) >= cap {
            anyhow::bail!("today's limit of {cap} LinkedIn {kind}s is reached (keeps the account safe); it resets tomorrow");
        }
        Ok(())
    }

    /// Records an outreach attempt; a sent one moves the lead to "contacted".
    pub fn log_outreach(&self, slug: &str, kind: &str, text: &str, target: &str, status: &str) {
        let db = self.db.lock().unwrap();
        let campaign: Option<String> = db.query_row("SELECT campaign FROM leads WHERE slug = ?1 ORDER BY updated DESC LIMIT 1", [slug], |x| x.get(0)).optional().ok().flatten();
        let _ = db.execute("INSERT INTO outreach (campaign, slug, kind, text, target, status, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![campaign, slug, kind, text, target, status, now()]);
        if status == "sent" {
            let _ = db.execute("UPDATE leads SET status = 'contacted', updated = ?2 WHERE slug = ?1 AND status IN ('new', 'drafted')", params![slug, now()]);
        }
    }

    /// Already sent this kind to this person (any campaign)?
    pub fn contacted(&self, slug: &str, kind: &str) -> bool {
        self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM outreach WHERE slug = ?1 AND kind = ?2 AND status = 'sent'", params![slug, kind], |x| x.get::<_, i64>(0)).unwrap_or(0) > 0
    }

    /// A lead by slug, name or profile URL (newest campaign first).
    pub fn lead(&self, who: &str) -> Option<Value> {
        let w = who.trim().trim_end_matches('/');
        let slug = w.rsplit("/in/").next().unwrap_or(w).split('/').next().unwrap_or(w).to_string();
        let db = self.db.lock().unwrap();
        db.query_row("SELECT slug, name, headline FROM leads WHERE slug = ?1 OR lower(name) = lower(?2) ORDER BY updated DESC LIMIT 1", params![slug, w],
            |x| Ok(json!({"slug": x.get::<_, String>(0)?, "name": x.get::<_, String>(1)?, "headline": x.get::<_, String>(2)?}))).optional().ok().flatten()
    }

    /// What Sam stored about a person (headline, reason, angle, best post) for outreach texts.
    pub fn leads_by_slug(&self, slug: &str) -> String {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT headline, location, summary, reason, angle, post FROM leads WHERE slug = ?1 ORDER BY updated DESC LIMIT 1", [slug],
            |x| Ok(format!("headline: {} | location: {} | {} | fit: {} | angle: {} | post: {}", x.get::<_, String>(0)?, x.get::<_, String>(1)?, x.get::<_, String>(2)?, x.get::<_, String>(3)?, x.get::<_, String>(4)?, x.get::<_, String>(5)?)))
            .unwrap_or_default()
    }

    /// GDPR: forget a person everywhere Sam stored them.
    pub fn forget(&self, slug: &str) -> usize {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM leads WHERE slug = ?1", [slug]).unwrap_or(0) + db.execute("DELETE FROM outreach WHERE slug = ?1", [slug]).unwrap_or(0)
    }

    /// The campaign whose product best matches `words` (or the newest).
    pub fn find_campaign(&self, words: &str) -> Option<String> {
        let all = self.campaigns();
        let want: std::collections::HashSet<String> = words.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_string).collect();
        all.iter().max_by_key(|c| {
            let have: std::collections::HashSet<String> = format!("{} {}", c["product"], c["input"]).to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 3).map(str::to_string).collect();
            (have.intersection(&want).count(), c["created"].as_i64().unwrap_or(0))
        }).and_then(|c| c["id"].as_str().map(str::to_string))
    }
}

/// The product as text: a link's page (readable text), else the idea as given.
pub async fn product_text(app: &AppState, input: &str) -> String {
    let t = input.trim();
    if crate::browser::cdp::allowed_url(t) {
        if let Ok(p) = crate::browser::extract::fetch_static(&app.http, t).await {
            return format!("Product page {t}:\n{}", p.text.chars().take(5000).collect::<String>());
        }
    }
    t.chars().take(5000).collect()
}

pub async fn plan(app: &AppState, model: Provider, product: &str, place: &str) -> anyhow::Result<(Value, u64)> {
    let user = format!("{product}{}", if place.is_empty() { String::new() } else { format!("\nThe user wants leads in: {place}") });
    let (txt, u) = llm::complete(app, model, ICP_SYS, &user, true, 4000).await?;
    Ok((serde_json::from_str(&txt).context("icp json")?, u.prompt + u.completion))
}

fn card_line(i: usize, l: &Lead) -> String {
    format!("{i} | {} | {} | {} | {}", l.name, l.headline.chars().take(160).collect::<String>(), l.location, l.summary.chars().take(120).collect::<String>())
}

/// One batch call over the search cards → (index, score, reason).
pub async fn score_cards(app: &AppState, model: Provider, icp: &Value, leads: &[Lead]) -> anyhow::Result<(Vec<(usize, i64, String)>, u64)> {
    let mut out = Vec::new();
    let mut tokens = 0;
    for chunk in leads.chunks(40).enumerate() {
        let lines: Vec<String> = chunk.1.iter().enumerate().map(|(j, l)| card_line(chunk.0 * 40 + j, l)).collect();
        let user = format!("Product and buyers: {}\n\nPeople:\n{}", serde_json::to_string(&json!({"product": icp["product"], "buyers": icp["buyers"], "industries": icp["industries"], "exclude": icp["exclude"]}))?, lines.join("\n"));
        let (txt, u) = llm::complete_with(app, model, crate::llm::Tier::Light, SCORE_SYS, &user, true, 6000).await?;
        tokens += u.prompt + u.completion;
        let v: Value = serde_json::from_str(&txt)?;
        for s in v["scores"].as_array().into_iter().flatten() {
            if let Some(i) = s["i"].as_u64() {
                out.push((i as usize, s["score"].as_i64().unwrap_or(0), s["reason"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    Ok((out, tokens))
}

/// Profiles (and optionally posts) for the given leads → one batch judgment.
pub async fn enrich(app: &AppState, tab: &Tab, model: Provider, icp: &Value, leads: &[Lead], with_posts: bool) -> anyhow::Result<(Vec<(usize, i64, String, String, String, Value, Value)>, u64)> {
    let mut docs = Vec::new();
    let mut raw = Vec::new();
    for (i, l) in leads.iter().enumerate() {
        let p = linkedin::profile(tab, &l.slug).await.unwrap_or(Value::Null);
        let ps = if with_posts { linkedin::posts(tab, &l.slug, &l.name).await.unwrap_or_default() } else { vec![] };
        let own: Vec<Value> = ps.iter().filter(|p| p.own).take(3).map(|p| json!({"url": p.url, "date": p.date, "text": p.text.chars().take(240).collect::<String>()})).collect();
        docs.push(format!("{i} | {} | {}\n  About: {}\n  Experience: {}\n  Activity: {}{}", l.name, p["top"].as_str().unwrap_or(&l.headline), p["about"].as_str().unwrap_or("").chars().take(400).collect::<String>(),
            p["experience"].as_str().unwrap_or("").chars().take(400).collect::<String>(), p["activity"].as_str().unwrap_or("").chars().take(240).collect::<String>(),
            if own.is_empty() { String::new() } else { format!("\n  Own posts: {}", serde_json::to_string(&own)?) }));
        raw.push((p, json!(own)));
    }
    let user = format!("Product and buyers: {}\n\nLeads:\n{}", serde_json::to_string(&json!({"product": icp["product"], "buyers": icp["buyers"], "industries": icp["industries"], "exclude": icp["exclude"]}))?, docs.join("\n\n"));
    let (txt, u) = llm::complete(app, model, ENRICH_SYS, &user, true, 8000).await?;
    let v: Value = serde_json::from_str(&txt)?;
    let mut out = Vec::new();
    for s in v["leads"].as_array().into_iter().flatten() {
        let Some(i) = s["i"].as_u64().map(|i| i as usize).filter(|i| *i < raw.len()) else { continue };
        let st = |k: &str| s[k].as_str().unwrap_or("").to_string();
        out.push((i, s["score"].as_i64().unwrap_or(0), st("reason"), st("angle"), st("post"), raw[i].0.clone(), raw[i].1.clone()));
    }
    Ok((out, u.prompt + u.completion))
}

#[derive(Debug, Clone)]
pub struct Options {
    pub searches: usize,
    pub pages: u32,
    pub enrich_top: usize,
    pub with_posts: bool,
    pub place: String,
}

impl Default for Options {
    /// Lab winner (leads lab).
    fn default() -> Self {
        // leads lab r1: 2 good searches × 2 pages already put 10/10 fits in the top 10; the top-10
        // profile check fixes card-score misses on bigger pools and gives the outreach angles.
        Self { searches: 3, pages: 2, enrich_top: 10, with_posts: false, place: String::new() }
    }
}

/// The whole run for one product → (campaign id, leads best first, tokens, page views).
pub async fn find(app: &AppState, tab: &Tab, model: Provider, input: &str, opt: &Options) -> anyhow::Result<(String, Vec<Value>, u64, u32)> {
    let st = store().context("leads store unavailable")?;
    let views0 = linkedin::views_today();
    let product = product_text(app, input).await;
    let (icp, mut tokens) = plan(app, model, &product, &opt.place).await?;
    let campaign = st.campaign(input, &icp)?;
    let mut leads: Vec<(Lead, String)> = Vec::new();
    for q in icp["searches"].as_array().into_iter().flatten().filter_map(|q| q.as_str()).take(opt.searches) {
        for page in 1..=opt.pages {
            match linkedin::search(tab, q, page).await {
                Ok(ls) => {
                    let n = ls.len();
                    for l in ls {
                        if !leads.iter().any(|(x, _)| x.slug == l.slug) {
                            leads.push((l, q.to_string()));
                        }
                    }
                    if n < 8 {
                        break;   // no further pages
                    }
                }
                Err(e) if e.to_string().contains("budget") || e.to_string().contains("sign in") => return Err(e),
                Err(e) => tracing::info!("linkedin search {q:?} p{page}: {e:#}"),
            }
        }
    }
    st.add(&campaign, &leads);
    let cards: Vec<Lead> = leads.iter().map(|(l, _)| l.clone()).collect();
    let (scores, t) = score_cards(app, model, &icp, &cards).await?;
    tokens += t;
    for (i, s, r) in &scores {
        if let Some(l) = cards.get(*i) {
            st.set_card_score(&campaign, &l.slug, *s, r);
        }
    }
    let mut ranked: Vec<(usize, i64)> = scores.iter().map(|(i, s, _)| (*i, *s)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let top: Vec<Lead> = ranked.iter().take(opt.enrich_top).filter(|(_, s)| *s >= 40).filter_map(|(i, _)| cards.get(*i).cloned()).collect();
    if !top.is_empty() {
        let (res, t) = enrich(app, tab, model, &icp, &top, opt.with_posts).await?;
        tokens += t;
        for (i, s, r, a, p, prof, posts) in res {
            st.set_enriched(&campaign, &top[i].slug, &prof, &posts, s, &r, &a, &p);
        }
    }
    Ok((campaign.clone(), st.leads(&campaign, 100), tokens, linkedin::views_today().saturating_sub(views0)))
}

// ---------------------------------------------------------------------------------------------
// Long-running campaigns: thousands of leads over weeks, in small paced slices.

const GRID_SYS: &str = "You expand a B2B lead search into many DISTINCT LinkedIn people searches so that together they reach thousands of different people. \
Combine buyer job titles (and their usual variants / languages) × industries × regions (countries, states, big cities) from the plan. Each search 2-4 words, \
decision makers only (no clerk or task phrases). Reply ONLY with JSON {\"searches\": [\"…\"]} — up to N searches, most promising first, no duplicates.";

/// Query grid for a campaign (one LLM call).
pub async fn grid(app: &AppState, model: Provider, icp: &Value, place: &str, n: usize) -> anyhow::Result<(Vec<String>, u64)> {
    let user = format!("N = {n}\nPlan: {}{}", json!({"product": icp["product"], "buyers": icp["buyers"], "industries": icp["industries"], "places": icp["places"], "searches": icp["searches"]}),
        if place.is_empty() { String::new() } else { format!("\nRegion: {place}") });
    let (txt, u) = llm::complete_with(app, model, crate::llm::Tier::Light, GRID_SYS, &user, true, 8000).await?;
    let v: Value = serde_json::from_str(&txt)?;
    let mut out: Vec<String> = icp["searches"].as_array().into_iter().flatten().filter_map(|q| q.as_str().map(str::to_string)).collect();
    for q in v["searches"].as_array().into_iter().flatten().filter_map(|q| q.as_str()) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(q)) {
            out.push(q.to_string());
        }
    }
    out.truncate(n);
    Ok((out, u.prompt + u.completion))
}

// ----- OpenAI Batch API for card scoring (≈50 % cheaper, results within 24 h): campaigns
// collect people for a while, send them in one batch, apply the scores when it completes.

/// Batch scoring only when the user's chosen provider is OpenAI (it has a batch API at about
/// half price); other providers score right away. LEADS_BATCH=0 turns it off.
pub fn batch_enabled(app: &AppState) -> bool {
    app.cfg.provider == Provider::OpenAI && app.cfg.openai_key.is_some() && crate::config::env_num("LEADS_BATCH", 1) == 1
}

/// Leads → one JSONL request per 40 cards → upload → batch. Returns {"id", "slugs"}.
pub async fn batch_submit(app: &AppState, icp: &Value, leads: &[Lead]) -> anyhow::Result<Value> {
    let key = app.cfg.openai_key.as_deref().context("no OpenAI key")?;
    let base = app.cfg.openai_url.trim_end_matches('/').to_string();
    let head = serde_json::to_string(&json!({"product": icp["product"], "buyers": icp["buyers"], "industries": icp["industries"], "exclude": icp["exclude"]}))?;
    let mut lines = String::new();
    let mut slugs: Vec<Vec<String>> = Vec::new();
    for (k, chunk) in leads.chunks(40).enumerate() {
        let people: Vec<String> = chunk.iter().enumerate().map(|(j, l)| crate::secrets::redact(&card_line(j, l))).collect();
        let mut body = json!({"model": app.cfg.openai_model, "max_completion_tokens": 6000, "response_format": {"type": "json_object"},
            "messages": [{"role": "system", "content": SCORE_SYS}, {"role": "user", "content": format!("Product and buyers: {head}\n\nPeople:\n{}", people.join("\n"))}]});
        let m = app.cfg.openai_model.as_str();
        if !app.cfg.openai_reasoning.is_empty() && (m.starts_with("gpt-5") || m.starts_with("gpt-6") || m.starts_with('o')) {
            body["reasoning_effort"] = json!(app.cfg.openai_reasoning);
        }
        lines.push_str(&json!({"custom_id": format!("c{k}"), "method": "POST", "url": "/v1/chat/completions", "body": body}).to_string());
        lines.push('\n');
        slugs.push(chunk.iter().map(|l| l.slug.clone()).collect());
    }
    let form = reqwest::multipart::Form::new().text("purpose", "batch").part("file", reqwest::multipart::Part::bytes(lines.into_bytes()).file_name("leads.jsonl").mime_str("application/jsonl")?);
    let f: Value = app.http.post(format!("{base}/files")).bearer_auth(key).multipart(form).send().await?.error_for_status()?.json().await?;
    let b: Value = app.http.post(format!("{base}/batches")).bearer_auth(key)
        .json(&json!({"input_file_id": f["id"], "endpoint": "/v1/chat/completions", "completion_window": "24h"})).send().await?.error_for_status()?.json().await?;
    Ok(json!({"id": b["id"], "slugs": slugs, "submitted": now()}))
}

/// Finished batch → (slug, score, reason) for every scored card; None while it's running.
pub async fn batch_poll(app: &AppState, b: &Value) -> anyhow::Result<Option<Vec<(String, i64, String)>>> {
    let key = app.cfg.openai_key.as_deref().context("no OpenAI key")?;
    let base = app.cfg.openai_url.trim_end_matches('/').to_string();
    let st: Value = app.http.get(format!("{base}/batches/{}", b["id"].as_str().unwrap_or(""))).bearer_auth(key).send().await?.error_for_status()?.json().await?;
    match st["status"].as_str().unwrap_or("") {
        "completed" => {}
        "failed" | "expired" | "cancelled" => anyhow::bail!("scoring batch {}: {}", st["status"], st["errors"]),
        _ => return Ok(None),
    }
    let file = st["output_file_id"].as_str().context("batch without output")?;
    let text = app.http.get(format!("{base}/files/{file}/content")).bearer_auth(key).send().await?.error_for_status()?.text().await?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let k: usize = v["custom_id"].as_str().and_then(|c| c.trim_start_matches('c').parse().ok()).unwrap_or(usize::MAX);
        let Some(chunk) = b["slugs"].get(k).and_then(|c| c.as_array()) else { continue };
        let content = v["response"]["body"]["choices"][0]["message"]["content"].as_str().unwrap_or("");
        let scores: Value = serde_json::from_str(content).unwrap_or_default();
        for s in scores["scores"].as_array().into_iter().flatten() {
            if let Some(slug) = s["i"].as_u64().and_then(|i| chunk.get(i as usize)).and_then(|x| x.as_str()) {
                out.push((slug.to_string(), s["score"].as_i64().unwrap_or(0), s["reason"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    Ok(Some(out))
}

/// Pages per search before moving on (LinkedIn's results thin out deeper down).
const MAX_PAGES: u32 = 10;
/// Page views per slice (a slice is a short "session", spread over the day).
const SLICE_VIEWS: u32 = 10;

/// One slice of a campaign: ~10 paced search pages, then scoring of the new people and a
/// profile check of the best two. Returns (new people, tokens, finished?).
pub async fn slice(app: &AppState, tab: &Tab, model: Provider, id: &str) -> anyhow::Result<(usize, u64, bool)> {
    let st = store().context("leads store unavailable")?;
    let c = st.campaign_row(id).context("no such campaign")?;
    let icp = c["icp"].clone();
    let grid: Vec<String> = c["grid"].as_array().into_iter().flatten().filter_map(|q| q.as_str().map(str::to_string)).collect();
    let (mut qi, mut page) = (c["cursor"]["q"].as_u64().unwrap_or(0) as usize, c["cursor"]["page"].as_u64().unwrap_or(1) as u32);
    let mut added = 0;
    let mut views = 0;
    let budget_left = c["daily_views"].as_i64().unwrap_or(100) as u32;
    while views < SLICE_VIEWS && linkedin::views_today() < budget_left.min(linkedin::DAILY_VIEWS) && qi < grid.len() {
        let q = &grid[qi];
        let r = linkedin::search(tab, q, page).await;
        views += 1;
        match r {
            Ok(ls) => {
                let n = ls.len();
                added += st.add(id, &ls.into_iter().map(|l| (l, q.clone())).collect::<Vec<_>>());
                if n < 8 || page >= MAX_PAGES {
                    qi += 1;
                    page = 1;
                } else {
                    page += 1;
                }
            }
            Err(e) => {
                st.set_campaign(id, None, Some(&json!({"q": qi, "page": page})), None, None, None);
                return Err(e);
            }
        }
        st.set_campaign(id, None, Some(&json!({"q": qi, "page": page})), None, None, None);
    }
    // Score what's new: through the OpenAI Batch API when available (cheaper; applied when it
    // completes), else right away in batches of 40. Then check the two best new profiles.
    let mut tokens = 0;
    if batch_enabled(app) {
        if let Some(b) = st.batch(id) {
            match batch_poll(app, &b).await {
                Ok(Some(scores)) => {
                    for (slug, sc, r) in &scores {
                        st.set_card_score(id, slug, *sc, r);
                    }
                    // Cards the batch didn't return get a 0 so they aren't resent forever.
                    for slug in b["slugs"].as_array().into_iter().flatten().flat_map(|c| c.as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_str().map(str::to_string)) {
                        if !scores.iter().any(|s| s.0 == slug) {
                            st.set_card_score(id, &slug, 0, "not scored");
                        }
                    }
                    st.set_batch(id, None);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::info!(campaign = id, "scoring batch: {e:#}; resending");
                    st.set_batch(id, None);
                }
            }
        }
        // A new batch once enough people wait (or the search grid is done).
        let waiting = st.unscored(id, 5000);
        if st.batch(id).is_none() && (waiting.len() >= 400 || (qi >= grid.len() && !waiting.is_empty())) {
            let b = batch_submit(app, &icp, &waiting).await?;
            st.set_batch(id, Some(&b));
        }
    }
    while !batch_enabled(app) {
        let todo = st.unscored(id, 80);
        if todo.is_empty() {
            break;
        }
        let (scores, t) = score_cards(app, model, &icp, &todo).await?;
        tokens += t;
        for (i, s, r) in &scores {
            if let Some(l) = todo.get(*i) {
                st.set_card_score(id, &l.slug, *s, r);
            }
        }
        // Lines the model skipped get a 0 so they don't loop forever.
        for (i, l) in todo.iter().enumerate() {
            if !scores.iter().any(|s| s.0 == i) {
                st.set_card_score(id, &l.slug, 0, "not scored");
            }
        }
    }
    let best = st.to_enrich(id, 2);
    if !best.is_empty() && linkedin::views_today() + 2 <= budget_left.min(linkedin::DAILY_VIEWS) {
        if let Ok((res, t)) = enrich(app, tab, model, &icp, &best, false).await {
            tokens += t;
            for (i, s, r, a, p, prof, posts) in res {
                st.set_enriched(id, &best[i].slug, &prof, &posts, s, &r, &a, &p);
            }
        }
    }
    let c = st.campaign_row(id).context("campaign vanished")?;
    // Done when enough people are collected (or the grid is exhausted) and nothing waits for scoring.
    let done = (c["leads"].as_i64().unwrap_or(0) >= c["target"].as_i64().unwrap_or(i64::MAX) || qi >= grid.len()) && st.batch(id).is_none() && c["unscored"].as_i64().unwrap_or(0) == 0;
    Ok((added, tokens, done))
}

/// Campaign scheduler: active campaigns get a slice every ~45–90 min between 8:00 and 20:00,
/// under their daily page budget; LinkedIn's monthly search limit pauses until the 1st.
pub fn spawn(app: std::sync::Arc<AppState>, log: crate::tasklog::TaskLog) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        loop {
            let hour = chrono::Timelike::hour(&chrono::Local::now());
            if let (Some(st), true) = (store(), (8..20).contains(&hour)) {
                for id in st.active_campaigns() {
                    let r = async {
                        let chrome = crate::browser::cdp::Chrome::launch(&crate::thinker::data_dir(), crate::thinker::accounts_screen(), crate::browser::cdp::Visibility::Offscreen).await?;
                        let tab = chrome.new_tab().await?;
                        tab.goto(linkedin::HOME, std::time::Duration::from_secs(30)).await?;
                        if !linkedin::signed_in(&tab).await {
                            tab.close().await;
                            anyhow::bail!("LinkedIn isn't signed in (Connectors › LinkedIn › Link)");
                        }
                        let r = slice(&app, &tab, app.cfg.provider, &id).await;
                        tab.close().await;
                        let _ = chrome.save_cookies(crate::thinker::accounts_screen()).await;
                        r
                    }.await;
                    let jitter = (now() % 45) * 60_000;
                    let c = st.campaign_row(&id).unwrap_or_default();
                    let name = c["product"].as_str().unwrap_or("").chars().take(60).collect::<String>();
                    match r {
                        Ok((added, tokens, done)) => {
                            tracing::info!(campaign = id, added, tokens, leads = c["leads"].as_i64(), "lead campaign slice");
                            let dir = crate::thinker::data_dir().join("artifacts/leads");
                            let csv = st.export_csv(&id, &dir).ok();
                            let leads = c["leads"].as_i64().unwrap_or(0);
                            let milestone = c["milestone"].as_i64().unwrap_or(0);
                            if done {
                                st.set_campaign(&id, Some("done"), None, None, Some("finished"), None);
                                let t = log.start("leads", "leads", &format!("Lead list finished: {name}"));
                                log.update(&t, |r| r.files = csv.iter().map(|p| p.display().to_string()).collect());
                                log.finish(&t, "done", &format!("{leads} people collected, {} good fits (score ≥ 70). The CSV is attached.", c["good"]));
                            } else {
                                // Morning budget used up → next slice tomorrow; else later today.
                                let next = if linkedin::views_today() >= c["daily_views"].as_i64().unwrap_or(100) as u32 { now() + 12 * 3_600_000 } else { now() + 45 * 60_000 + jitter };
                                st.set_campaign(&id, None, None, Some(next), None, None);
                                if leads / 1000 > milestone {
                                    st.set_milestone(&id, leads / 1000);
                                    let t = log.start("leads", "leads", &format!("Lead list progress: {name}"));
                                    log.update(&t, |r| r.files = csv.iter().map(|p| p.display().to_string()).collect());
                                    log.finish(&t, "done", &format!("{leads} people so far ({} good fits) of the {} wanted.", c["good"], c["target"]));
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            let until = if msg.contains(linkedin::SEARCH_LIMIT) {
                                // Paused until the 1st of next month (LinkedIn resets the limit monthly).
                                let d = chrono::Local::now().date_naive();
                                let first = chrono::NaiveDate::from_ymd_opt(if d.month() == 12 { d.year() + 1 } else { d.year() }, if d.month() == 12 { 1 } else { d.month() + 1 }, 1).unwrap_or(d);
                                let t = log.start("leads", "leads", &format!("Lead list paused: {name}"));
                                log.finish(&t, "failed", "LinkedIn's monthly search limit for free accounts is reached; the list continues on the 1st. (Sales Navigator / Premium raises the limit.)");
                                chrono::TimeZone::from_local_datetime(&chrono::Local, &first.and_hms_opt(9, 0, 0).unwrap_or_default()).single().map(|t| t.timestamp_millis()).unwrap_or(now() + 7 * 86_400_000)
                            } else {
                                now() + 2 * 3_600_000
                            };
                            tracing::info!(campaign = id, "lead campaign slice failed: {msg}");
                            st.set_campaign(&id, None, None, Some(until), Some(&msg), None);
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    #[test]
    fn dedup_per_campaign() {
        let d = std::env::temp_dir().join(format!("leads-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("leads.db"));
        let s = super::Store::open(&d).unwrap();
        let c = s.campaign("freight quotes", &serde_json::json!({"product": "AI freight quotes"})).unwrap();
        let l = |slug: &str| (super::Lead { slug: slug.into(), name: slug.into(), ..Default::default() }, "q".to_string());
        assert_eq!(s.add(&c, &[l("a"), l("b")]), 2);
        assert_eq!(s.add(&c, &[l("b"), l("c")]), 1, "known people aren't added twice");
        s.set_card_score(&c, "c", 90, "exact buyer");
        assert_eq!(s.leads(&c, 10)[0]["slug"], "c");
        assert_eq!(s.find_campaign("freight").as_deref(), Some(c.as_str()));
        assert_eq!(s.lead("https://www.linkedin.com/in/c/").unwrap()["slug"], "c");
        assert!(s.forget("c") >= 1 && s.lead("c").is_none());
        // Campaign state: grid, cursor, unscored queue, CSV.
        s.start_campaign(&c, 10_000, &["Geschäftsführer Spedition".into()], 100, "s");
        let row = s.campaign_row(&c).unwrap();
        assert_eq!((row["state"].as_str(), row["target"].as_i64(), row["cursor"]["q"].as_i64()), (Some("active"), Some(10_000), Some(0)));
        assert_eq!(s.unscored(&c, 10).len(), 2, "a and b wait for scoring");
        assert!(s.active_campaigns().contains(&c));
        let csv = s.export_csv(&c, &d).unwrap();
        assert!(std::fs::read_to_string(csv).unwrap().lines().count() == 3);
    }
}
