//! Signed-in web sessions for research on eBay, X, Instagram and TikTok, in Sam's own "accounts"
//! profile (Connectors › <site> › Link: sign in once). Sam only reads pages the user could open by
//! hand, at a human pace, under a daily request budget per site; an HTTP 429 or a challenge pauses
//! that site for 24 h (a second one within 7 days: 7 days). Budgets from the connectors spec
//! (docs/CONNECTORS_SOCIAL_MARKET.md §1a) and the labs: X answered 429 after ~52 searches in 30 min.

use std::time::{Duration, Instant};

use anyhow::bail;
use serde_json::{Value, json};

use super::cdp::Tab;

pub struct Site {
    pub id: &'static str,
    pub name: &'static str,
    pub home: &'static str,
    pub login: &'static str,
    /// (cookie URL, cookie name) present only when signed in; None: check by page instead.
    pub cookie: Option<(&'static str, &'static str)>,
    /// Requests per day (page loads and JSON calls) and per 30 minutes.
    pub daily: u32,
    pub per_30min: u32,
}

pub const SITES: &[Site] = &[
    Site { id: "ebay", name: "eBay", home: "https://www.ebay.de/", login: "https://signin.ebay.de/", cookie: None, daily: 300, per_30min: 60 },
    Site { id: "x", name: "X (Twitter)", home: "https://x.com/home", login: "https://x.com/i/flow/login", cookie: Some(("https://x.com/", "auth_token")), daily: 150, per_30min: 40 },
    Site { id: "instagram", name: "Instagram", home: "https://www.instagram.com/", login: "https://www.instagram.com/accounts/login/", cookie: Some(("https://www.instagram.com/", "sessionid")), daily: 150, per_30min: 30 },
    Site { id: "facebook", name: "Facebook", home: "https://www.facebook.com/", login: "https://www.facebook.com/login/", cookie: Some(("https://www.facebook.com/", "c_user")), daily: 150, per_30min: 30 },
    Site { id: "tiktok", name: "TikTok", home: "https://www.tiktok.com/", login: "https://www.tiktok.com/login", cookie: Some(("https://www.tiktok.com/", "sessionid")), daily: 400, per_30min: 80 },
];

pub fn site(id: &str) -> Option<&'static Site> {
    SITES.iter().find(|s| s.id == id)
}

pub async fn signed_in(tab: &Tab, s: &Site) -> bool {
    match s.cookie {
        Some((url, name)) => tab.has_cookie(url, name).await,
        None => {
            // eBay: My eBay redirects to the sign-in page when there is no session.
            if tab.goto("https://www.ebay.de/mye/myebay/summary", Duration::from_secs(30)).await.is_err() {
                return false;
            }
            let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            !here.contains("signin")
        }
    }
}

/// Waits until Sam's session for the site is signed in; shows the window and asks once if not.
pub async fn ready(tab: &Tab, id: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<()> {
    let s = site(id).ok_or_else(|| anyhow::anyhow!("unknown site {id}"))?;
    tab.goto(s.home, Duration::from_secs(30)).await?;
    if signed_in(tab, s).await {
        return Ok(());
    }
    tab.goto(s.login, Duration::from_secs(30)).await?;
    tab.show_window(true).await;
    let ask = format!("Sam needs its own {} sign-in once: sign in in the browser window I just opened. I'll continue as soon as you're in.", s.name);
    match on_user {
        Some(tx) => {
            let _ = tx.send(ask);
        }
        None => eprintln!("\n>>> {ask}"),
    }
    let end = Instant::now() + Duration::from_secs(600);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let ok = match s.cookie {
            Some((url, name)) => tab.has_cookie(url, name).await,
            None => tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(|h| !h.contains("signin"))).unwrap_or(false),
        };
        if ok {
            tab.show_window(false).await;
            crate::connectors::mark_linked(s.id);
            return Ok(());
        }
    }
    bail!("{} wasn't signed in within 10 minutes", s.name)
}

// ---------------------------------------------------------------------------------------------
// Budgets and the tripwire: data/browser/usage-sessions.json
// {"<site>": {"day": "2026-10-18", "n": 12, "recent": [ms…], "paused_until": ms, "trips": [ms…]}}

fn usage_file() -> std::path::PathBuf {
    crate::thinker::data_dir().join("browser/usage-sessions.json")
}

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load() -> Value {
    std::fs::read_to_string(usage_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}

fn save(v: &Value) {
    let p = usage_file();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, v.to_string());
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// Counts `n` requests against the site's budgets; Err (with when it can continue) when a budget
/// is used up or the site is paused by the tripwire.
pub fn take(id: &str, n: u32) -> anyhow::Result<()> {
    let s = site(id).ok_or_else(|| anyhow::anyhow!("unknown site {id}"))?;
    let _g = LOCK.lock().unwrap();
    let mut v = load();
    let t = now_ms();
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let e = &mut v[id];
    if e["paused_until"].as_i64().is_some_and(|p| p > t) {
        let left = (e["paused_until"].as_i64().unwrap_or(t) - t) / 3_600_000;
        bail!("{} is paused for about {left} h more after it pushed back (rate limit / check); Sam reads it again then", s.name);
    }
    if e["day"].as_str() != Some(&day) {
        *e = json!({"day": day, "n": 0, "recent": [], "paused_until": e["paused_until"], "trips": e["trips"]});
    }
    let recent: Vec<i64> = e["recent"].as_array().into_iter().flatten().filter_map(|x| x.as_i64()).filter(|x| t - x < 30 * 60_000).collect();
    let used = e["n"].as_u64().unwrap_or(0) as u32;
    if used + n > s.daily {
        bail!("today's {} budget ({} requests) is used up; Sam continues tomorrow", s.name, s.daily);
    }
    if recent.len() as u32 + n > s.per_30min {
        bail!("{} budget for the last 30 minutes is used up; try again in a little while", s.name);
    }
    let mut r = recent;
    r.extend(std::iter::repeat(t).take(n as usize));
    e["n"] = json!(used + n);
    e["recent"] = json!(r);
    save(&v);
    Ok(())
}

/// Err when the site is paused by the tripwire (checked before any page load or LLM call).
pub fn check(id: &str) -> anyhow::Result<()> {
    let s = site(id).ok_or_else(|| anyhow::anyhow!("unknown site {id}"))?;
    let v = load();
    let t = now_ms();
    if let Some(p) = v[id]["paused_until"].as_i64().filter(|p| *p > t) {
        bail!("{} is paused for about {} h more after it pushed back (rate limit / check); Sam reads it again then", s.name, (p - t) / 3_600_000 + 1);
    }
    Ok(())
}

/// The site pushed back (HTTP 429, a challenge, "unusual activity"): pause it for 24 h, or 7 days
/// on the second time within a week.
pub fn trip(id: &str, why: &str) {
    let _g = LOCK.lock().unwrap();
    let mut v = load();
    let t = now_ms();
    let trips: Vec<i64> = v[id]["trips"].as_array().into_iter().flatten().filter_map(|x| x.as_i64()).filter(|x| t - x < 7 * 86_400_000).collect();
    let pause = if trips.is_empty() { 24 * 3_600_000 } else { 7 * 86_400_000 };
    let mut all = trips;
    all.push(t);
    v[id]["trips"] = json!(all);
    v[id]["paused_until"] = json!(t + pause);
    v[id]["last_trip"] = json!(why);
    save(&v);
    tracing::warn!(site = id, why, hours = pause / 3_600_000, "session tripwire: site paused");
}

/// Requests used today, for the Connectors window.
pub fn used_today(id: &str) -> u32 {
    let v = load();
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    if v[id]["day"].as_str() == Some(&day) { v[id]["n"].as_u64().unwrap_or(0) as u32 } else { 0 }
}
