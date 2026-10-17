//! What's on: events in a city on a date, curated to the user's interests. Vienna reads the
//! Falter event calendar for that date (falter.at/events/suche?span=YYYY-MM-DD, paged) and
//! wien.info's highlights with the generic listing reader (browser/listings.rs, 0 tokens), then
//! ONE LLM call picks the best few with time, place, why and link. Other places: web research.

use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::{AppState, browser::{cdp::{Chrome, Tab, Visibility}, listings}, llm};

const PICK_SYS: &str = "You curate what's on for the user. From the event listings (title | card text with date, time, venue | link), \
pick up to 8 that best match their interests and are on the given date (exhibitions running that day count). Prefer the unusual and \
outstanding (premieres, one-off concerts, special exhibitions) over routine. Use only what the listings say. Reply ONLY with JSON \
{\"events\": [{\"title\": \"…\", \"when\": \"time or 'all day'\", \"where\": \"venue\", \"why\": \"one short line\", \"url\": \"…\"}], \"note\": \"one line if the listings were thin\"}.";

fn is_vienna(place: &str) -> bool {
    let p = place.to_lowercase();
    p.is_empty() || p.contains("wien") || p.contains("vienna")
}

async fn cards_of(tab: &Tab, url: &str) -> Vec<Value> {
    if listings::load(tab, url).await.is_err() {
        return vec![];
    }
    let key = tab.eval(listings::ITEMKEY_JS).await.unwrap_or_default();
    let Some(item) = key["item"].as_str() else { return vec![] };
    tab.eval(&format!("({})({})", listings::CARDS_JS, serde_json::to_string(item).unwrap_or_default())).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

/// (picked events, the listing lines read, tokens).
pub async fn find(app: &AppState, place: &str, date: chrono::NaiveDate, interests: &str) -> anyhow::Result<(Value, usize, u64)> {
    anyhow::ensure!(is_vienna(place), "only Vienna has an event calendar here so far");
    let chrome = Chrome::launch(&crate::thinker::data_dir(), crate::config::env_num("BG_SCREEN", 1u16), Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let mut lines: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for page in 1..=4 {
        let url = format!("https://www.falter.at/events/suche?span={}{}", date.format("%Y-%m-%d"), if page > 1 { format!("&page={page}") } else { String::new() });
        let cs = cards_of(&tab, &url).await;
        let n = cs.len();
        for c in cs {
            let u = c["url"].as_str().unwrap_or("").to_string();
            if u.contains("/event/") && seen.insert(u.clone()) {
                lines.push(format!("{} | {} | {u}", c["title"].as_str().unwrap_or(""), c["text"].as_str().unwrap_or("").chars().take(170).collect::<String>()));
            }
        }
        if n < 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    // wien.info lists today's highlights (no date filter): a few, as hints for big shows.
    for c in cards_of(&tab, "https://www.wien.info/de/aktuell/veranstaltungen").await.into_iter().take(15) {
        let u = c["url"].as_str().unwrap_or("").to_string();
        if seen.insert(u.clone()) {
            lines.push(format!("{} | {} | {u}", c["title"].as_str().unwrap_or(""), c["text"].as_str().unwrap_or("").chars().take(140).collect::<String>()));
        }
    }
    tab.close().await;
    anyhow::ensure!(!lines.is_empty(), "the event calendars showed nothing for that date");
    // The same listings for the same date and interests were picked before (an automation that
    // fires every morning, a repeated question): reuse that pick, no LLM call (4.9–9.3k tokens).
    let key = cache_key(date, interests, &lines);
    if let Some(v) = cached(&key) {
        tracing::info!(date = %date, "events: listings unchanged, reusing the earlier pick");
        return Ok((v, lines.len(), 0));
    }
    let lines = prerank(app, interests, lines).await;
    let user = format!("Date: {} ({})\nPlace: Vienna\nInterests: {}\n\nListings:\n{}", date.format("%Y-%m-%d"), date.format("%A"), if interests.is_empty() { "anything special" } else { interests }, lines.join("\n"));
    let (txt, u) = llm::complete_tier(app, llm::Tier::Standard, PICK_SYS, &user, true, 4000).await?;
    let v: Value = serde_json::from_str(&txt).context("events json")?;
    store(&key, &v);
    Ok((v, lines.len(), u.prompt + u.completion))
}

/// Up to four Falter pages are ~100 cards; the pick needs the ones near the interests. Keep the
/// PRERANK_KEEP closest by embedding (the mind sidecar; on any failure keep them all).
const PRERANK_KEEP: usize = 40;
async fn prerank(app: &AppState, interests: &str, lines: Vec<String>) -> Vec<String> {
    if lines.len() <= PRERANK_KEEP || interests.trim().is_empty() {
        return lines;
    }
    let url = crate::env_or("MIND_URL", "http://127.0.0.1:47802");
    let q = match crate::mind::embed(&app.http, &url, &[interests.to_string()], "query").await {
        Ok(mut v) if !v.is_empty() => v.remove(0),
        _ => return lines,
    };
    let docs: Vec<String> = lines.iter().map(|l| l.rsplit_once(" | ").map_or(l.as_str(), |(t, _)| t).to_string()).collect();
    let Ok(vs) = crate::mind::embed(&app.http, &url, &docs, "doc").await else { return lines };
    let mut scored: Vec<(f32, String)> = vs.iter().map(|v| crate::mind::dot(&q, v)).zip(lines).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().take(PRERANK_KEEP).map(|x| x.1).collect()
}

fn cache_key(date: chrono::NaiveDate, interests: &str, lines: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (date.to_string(), interests.trim().to_lowercase(), lines).hash(&mut h);
    format!("{:016x}", h.finish())
}

fn cache_path(key: &str) -> std::path::PathBuf {
    crate::thinker::data_dir().join("cache/events").join(format!("{key}.json"))
}

fn cached(key: &str) -> Option<Value> {
    let p = cache_path(key);
    let age = p.metadata().ok()?.modified().ok()?.elapsed().ok()?;
    if age > Duration::from_secs(3 * 86_400) {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn store(key: &str, v: &Value) {
    let p = cache_path(key);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
        // Old picks: only the last few days are ever asked again.
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).is_some_and(|a| a > Duration::from_secs(7 * 86_400)) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let _ = std::fs::write(p, v.to_string());
}

pub fn summary(v: &Value, date: chrono::NaiveDate) -> String {
    let items: Vec<String> = v["events"].as_array().into_iter().flatten().map(|e| format!("- **{}** — {}, {}: {} ({})", e["title"].as_str().unwrap_or(""), e["when"].as_str().unwrap_or(""), e["where"].as_str().unwrap_or(""), e["why"].as_str().unwrap_or(""), e["url"].as_str().unwrap_or(""))).collect();
    format!("What's on in Vienna on {}:\n{}{}", date.format("%A %-d %B"), items.join("\n"), v["note"].as_str().filter(|n| !n.is_empty()).map(|n| format!("\n({n})")).unwrap_or_default())
}

#[allow(dead_code)]
fn _unused() -> Value {
    json!({})
}
