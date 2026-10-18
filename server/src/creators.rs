//! creators_find and social_profile on TikTok (creators lab L6).
//!
//! Discovery through VIDEO search with phrases in the target language: TikTok's user search for
//! "fitness deutsch" returned 0 % real German fitness creators (repost and meme pages named after
//! the keyword), video search 67 %. The search JSON already carries each author's follower count
//! and the captions, so the prefilter (follower range, caption language) costs no profile views.
//! Profiles (one request each) give the recent videos: median plays (not the mean: one viral
//! video inflates it ten times) and the last post date are computed in code; ONE batch judge
//! decides niche fit, person vs brand, repost pages. Paced and under the TikTok budget.

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::{AppState, browser::cdp::{Chrome, Tab, Visibility}, llm::{self, Tier}};

fn jtext(txt: &str) -> Option<Value> {
    let i = txt.find('{')?;
    serde_json::from_str(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok()
}

#[derive(Debug, Clone, Default)]
pub struct Creator {
    pub uid: String,
    pub nick: String,
    pub bio: String,
    pub followers: i64,
    pub likes: i64,
    pub videos: i64,
    pub captions: Vec<String>,
    pub plays: Vec<i64>,
    /// Unix seconds of the recent videos.
    pub posted: Vec<i64>,
    pub link: String,
    pub fit: Option<bool>,
    pub what: String,
}

impl Creator {
    pub fn median_plays(&self) -> i64 {
        let mut p: Vec<i64> = self.plays.iter().take(12).copied().collect();
        if p.is_empty() {
            return 0;
        }
        p.sort();
        p[p.len() / 2]
    }
    pub fn days_since_post(&self) -> Option<i64> {
        self.posted.iter().max().map(|t| (chrono::Local::now().timestamp() - t) / 86_400)
    }
    pub fn url(&self) -> String {
        format!("https://www.tiktok.com/@{}", self.uid)
    }
}

/// Share of common German words in captions (the "German-speaking" prefilter).
fn german(caps: &[String]) -> f64 {
    const DE: &[&str] = &["und", "der", "die", "das", "ist", "nicht", "mit", "für", "ich", "du", "wie", "auf", "ein", "eine", "zu", "den", "dem", "bei", "mehr", "dein", "deine", "mein", "meine", "heute", "wenn", "auch", "noch", "so", "aber", "oder"];
    // Hashtags and mentions say nothing about the caption's language.
    let words: Vec<String> = caps.join(" ").to_lowercase().split_whitespace().filter(|w| !w.starts_with('#') && !w.starts_with('@')).map(str::to_string).collect();
    if words.is_empty() {
        return 0.0;
    }
    words.iter().filter(|w| DE.contains(&w.as_str())).count() as f64 / words.len() as f64
}

async fn search_videos(tab: &Tab, phrase: &str) -> anyhow::Result<Vec<Creator>> {
    let url = format!("https://www.tiktok.com/search/video?q={}", url::form_urlencoded::byte_serialize(phrase.as_bytes()).collect::<String>());
    let mut by: std::collections::HashMap<String, Creator> = Default::default();
    for step in 0..2 {
        crate::browser::sessions::take("tiktok", 1)?;
        let action = if step == 0 { format!("navigate:{url}") } else { "scroll".into() };
        for (status, body) in tab.responses(&action, "/api/search/", Duration::from_secs(8)).await? {
            if status == 429 {
                crate::browser::sessions::trip("tiktok", "HTTP 429 on search");
                bail!("TikTok is rate-limiting Sam's account; Sam pauses TikTok for a day");
            }
            let Ok(v) = serde_json::from_str::<Value>(&body) else { continue };
            let items: Vec<&Value> = v["item_list"].as_array().map(|a| a.iter().collect()).unwrap_or_else(|| v["data"].as_array().into_iter().flatten().filter_map(|x| x.get("item")).collect());
            for it in items {
                let Some(uid) = it["author"]["uniqueId"].as_str() else { continue };
                let c = by.entry(uid.to_string()).or_insert_with(|| Creator { uid: uid.into(), followers: it["authorStats"]["followerCount"].as_i64().unwrap_or(0), ..Default::default() });
                c.captions.push(it["desc"].as_str().unwrap_or("").chars().take(150).collect());
            }
        }
    }
    Ok(by.into_values().collect())
}

/// One profile view: stats from the page's embedded data, recent videos from its item_list XHR.
pub async fn profile(tab: &Tab, uid: &str) -> anyhow::Result<Creator> {
    crate::browser::sessions::take("tiktok", 1)?;
    let uid = uid.trim_start_matches('@');
    let rs = tab.responses(&format!("navigate:https://www.tiktok.com/@{uid}"), "/api/post/item_list", Duration::from_secs(9)).await?;
    let mut c = Creator { uid: uid.into(), ..Default::default() };
    for (status, body) in rs {
        if status == 429 {
            crate::browser::sessions::trip("tiktok", "HTTP 429 on a profile");
            bail!("TikTok is rate-limiting Sam's account; Sam pauses TikTok for a day");
        }
        let Ok(v) = serde_json::from_str::<Value>(&body) else { continue };
        for it in v["itemList"].as_array().into_iter().flatten() {
            let st = if it["stats"].is_object() { &it["stats"] } else { &it["statsV2"] };
            let n = |x: &Value| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
            c.plays.push(n(&st["playCount"]));
            c.posted.push(n(&it["createTime"]));
            c.captions.push(it["desc"].as_str().unwrap_or("").chars().take(150).collect());
        }
    }
    let d = tab.eval("(()=>{const s=document.getElementById('__UNIVERSAL_DATA_FOR_REHYDRATION__'); if(!s) return null; const u=JSON.parse(s.textContent).__DEFAULT_SCOPE__['webapp.user-detail']; if(!u||!u.userInfo) return null; const x=u.userInfo.user, st=u.userInfo.stats; return {nick:x.nickname, bio:x.signature, link:(x.bioLink||{}).link||'', followers:st.followerCount, likes:st.heart, videos:st.videoCount}})()").await.unwrap_or(Value::Null);
    c.nick = d["nick"].as_str().unwrap_or("").into();
    c.bio = d["bio"].as_str().unwrap_or("").into();
    c.link = d["link"].as_str().unwrap_or("").into();
    c.followers = d["followers"].as_i64().unwrap_or(0);
    c.likes = d["likes"].as_i64().unwrap_or(0);
    c.videos = d["videos"].as_i64().unwrap_or(0);
    Ok(c)
}

const PLAN_SYS: &str = "You plan a TikTok creator search for outreach. Reply ONLY with JSON {\"phrases\": [3-4 short search phrases in the creators' language that such creators put in their video captions (topics they post about, not 'influencer' or 'creator')], \
\"lang\": \"de|en|…: the creators' language if the user named one or a place implies it, else empty\", \"followers\": [min, max] (null if not given), \"min_views\": number or null (average / median views per video the user asked for), \"niche\": \"one sentence: who fits\"}.";

const FIT_SYS: &str = "You check TikTok accounts for an outreach list. For EVERY numbered account first say what it posts and for whom (t: 3-8 words), then fit = true only if it is a real \
individual creator in the requested niche AND audience/place. fit = false for brands, shops, news outlets, repost/compilation/meme pages, fake model accounts, other languages or places. \
Ignore follower numbers. Reply ONLY with JSON {\"v\": [{\"n\": 1, \"t\": \"…\", \"fit\": true|false}, ...]}.";

pub struct Found {
    pub summary: String,
    pub markdown: String,
    pub data: Value,
    pub tokens: u64,
}

/// A follower range in any form a planner writes: [10000, 100000], "10000-100000", "10k–100k",
/// "10.000 bis 100.000".
pub fn parse_range(v: &Value) -> Option<(i64, i64)> {
    if let Some(a) = v.as_array() {
        return Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?));
    }
    let s = v.as_str()?.to_lowercase().replace(['.', ' '], "");
    let num = |x: &str| -> Option<i64> {
        let x = x.trim().trim_end_matches("follower");
        let (n, m) = if let Some(n) = x.strip_suffix('k') { (n, 1_000.0) } else if let Some(n) = x.strip_suffix('m') { (n, 1_000_000.0) } else { (x, 1.0) };
        Some((n.replace(',', ".").parse::<f64>().ok()? * m) as i64)
    };
    let parts: Vec<&str> = s.split(['-', '–', '—']).flat_map(|p| p.split("bis")).flat_map(|p| p.split("to")).filter(|p| !p.is_empty()).collect();
    Some((num(parts.first()?)?, num(parts.get(1)?)?))
}

fn k(n: i64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else if n >= 1000 { format!("{:.0}k", n as f64 / 1e3) } else { n.to_string() }
}

/// creators_find on TikTok. `followers` / `min_views` from the planner win over the brief's parse.
pub async fn find(app: &AppState, brief: &str, followers: Option<(i64, i64)>, min_views: Option<i64>, count: usize, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<Found> {
    crate::browser::sessions::check("tiktok")?;
    let (txt, u) = llm::complete_tier(app, Tier::Light, PLAN_SYS, brief, true, 1200).await?;
    let mut tokens = u.prompt + u.completion;
    let plan = jtext(&txt).context("search plan")?;
    let phrases: Vec<String> = plan["phrases"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).take(4).collect();
    if phrases.is_empty() {
        bail!("no search phrases for this brief");
    }
    let range = followers.or_else(|| parse_range(&plan["followers"]));
    let min_views = min_views.or_else(|| plan["min_views"].as_i64()).unwrap_or(0);
    let lang = plan["lang"].as_str().unwrap_or("").to_string();

    let data = crate::thinker::data_dir();
    let screen = crate::thinker::accounts_screen();
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let res = async {
        crate::browser::sessions::ready(&tab, "tiktok", on_user).await?;
        let mut all: std::collections::HashMap<String, Creator> = Default::default();
        for (i, p) in phrases.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(7000 + (chrono::Local::now().timestamp_millis() % 4000) as u64)).await;
            }
            for c in search_videos(&tab, p).await? {
                let e = all.entry(c.uid.clone()).or_insert_with(|| Creator { uid: c.uid.clone(), followers: c.followers, ..Default::default() });
                e.captions.extend(c.captions);
            }
        }
        let found = all.len();
        // Prefilter in code: follower range (with slack: search cards lag) and caption language.
        let sized: Vec<Creator> = all.into_values().filter(|c| range.is_none_or(|(lo, hi)| c.followers as f64 >= lo as f64 * 0.7 && c.followers as f64 <= hi as f64 * 1.3)).collect();
        // Language prefilter only when it leaves enough to check (captions can be all hashtags).
        let german_ones: Vec<Creator> = sized.iter().filter(|c| german(&c.captions) >= 0.08).cloned().collect();
        let mut pre = if lang == "de" && german_ones.len() >= 5 { german_ones } else { sized };
        pre.sort_by_key(|c| -c.followers);
        let mut profiled = vec![];
        for (i, c) in pre.iter().take(15.max(count.min(25))).enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(6000 + (chrono::Local::now().timestamp_millis() % 5000) as u64)).await;
            }
            match profile(&tab, &c.uid).await {
                Ok(p) => profiled.push(p),
                Err(e) if e.to_string().contains("budget") || e.to_string().contains("rate-limiting") => {
                    tracing::info!("tiktok: {e:#}");
                    break;
                }
                Err(e) => tracing::info!(uid = c.uid, "tiktok profile failed: {e:#}"),
            }
        }
        Ok::<_, anyhow::Error>((found, profiled))
    }
    .await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    let (found, mut profiled) = res?;
    if profiled.is_empty() {
        bail!("no TikTok accounts to check for this brief ({found} found in video search)");
    }

    // One batch judge for fit.
    let lines: Vec<String> = profiled.iter().enumerate().map(|(i, c)| format!("{}. @{} | bio: {} | recent: {}", i + 1, c.uid, c.bio.chars().take(160).collect::<String>(), c.captions.iter().take(5).map(|x| x.chars().take(70).collect::<String>()).collect::<Vec<_>>().join(" / "))).collect();
    let (txt, u) = llm::complete_tier(app, Tier::Light, FIT_SYS, &format!("Request: {brief}\nWho fits: {}\n\nAccounts:\n{}", plan["niche"].as_str().unwrap_or(""), lines.join("\n")), true, 4000).await?;
    tokens += u.prompt + u.completion;
    if let Some(v) = jtext(&txt).and_then(|j| j["v"].as_array().cloned()) {
        for x in v {
            if let Some(c) = x["n"].as_u64().and_then(|n| profiled.get_mut(n as usize - 1)) {
                c.fit = x["fit"].as_bool();
                c.what = x["t"].as_str().unwrap_or("").into();
            }
        }
    }
    let active = |c: &Creator| c.days_since_post().is_some_and(|d| d <= 60);
    let in_range = |c: &Creator| range.is_none_or(|(lo, hi)| (lo..=hi).contains(&c.followers));
    let fits: Vec<&Creator> = profiled.iter().filter(|c| c.fit == Some(true) && active(c) && in_range(c)).collect();
    let mut qualified: Vec<&Creator> = fits.iter().filter(|c| c.median_plays() >= min_views).copied().collect();
    qualified.sort_by_key(|c| -c.median_plays());
    let below: Vec<&Creator> = fits.iter().filter(|c| c.median_plays() < min_views).copied().collect();

    let row = |c: &Creator| format!("| [@{}]({}) | {} | {} | {} | {} | {} |", c.uid, c.url(), k(c.followers), k(c.median_plays()), c.days_since_post().map(|d| format!("{d} d")).unwrap_or_default(), c.what, if c.link.is_empty() { String::new() } else { c.link.clone() });
    let mut md = format!("# {brief}\n\n{} accounts found in TikTok video search ({} phrases), {} checked in depth.\n\n| Creator | Followers | Median views | Last post | Posts about | Link in bio |\n|---|---|---|---|---|---|\n", found, phrases.len(), profiled.len());
    for c in &qualified {
        md.push_str(&row(c));
        md.push('\n');
    }
    if !below.is_empty() {
        md.push_str(&format!("\n**Below the views filter ({} median views):** real fits with fewer views per video\n\n| Creator | Followers | Median views | Last post | Posts about | Link in bio |\n|---|---|---|---|---|---|\n", k(min_views)));
        for c in &below {
            md.push_str(&row(c));
            md.push('\n');
        }
    }
    let names = |xs: &[&Creator]| xs.iter().take(8).map(|c| format!("@{} ({} followers, {} median views)", c.uid, k(c.followers), k(c.median_plays()))).collect::<Vec<_>>().join(", ");
    let summary = match (qualified.len(), below.len()) {
        (0, 0) => format!("No fitting creators among the {} accounts checked ({found} found in TikTok video search). Try other phrases or a broader niche.", profiled.len()),
        (q, 0) => format!("{q} creators fit: {}.", names(&qualified)),
        (q, b) => format!("{q} creators fit all filters{}. {b} more fit but have fewer than {} median views per video (typical for this range): {}.",
            if q > 0 { format!(": {}", names(&qualified)) } else { String::new() }, k(min_views), names(&below)),
    };
    let js = |c: &Creator| json!({"uid": c.uid, "url": c.url(), "nick": c.nick, "followers": c.followers, "median_views": c.median_plays(), "days_since_post": c.days_since_post(), "fit": c.fit, "about": c.what, "bio": c.bio, "link": c.link});
    let data = json!({"brief": brief, "phrases": phrases, "found": found, "profiled": profiled.len(), "qualified": qualified.iter().map(|c| js(c)).collect::<Vec<_>>(), "below_views": below.iter().map(|c| js(c)).collect::<Vec<_>>(),
                      "all": profiled.iter().map(js).collect::<Vec<_>>()});
    Ok(Found { summary, markdown: md, data, tokens })
}

/// social_profile on TikTok: one account in depth.
pub async fn analyze(handle: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, Value)> {
    crate::browser::sessions::check("tiktok")?;
    let uid = handle.trim().trim_start_matches("https://www.tiktok.com/").trim_start_matches('@').split(['/', '?']).next().unwrap_or("").to_string();
    if uid.is_empty() {
        bail!("which TikTok account? (@name or profile link)");
    }
    let data = crate::thinker::data_dir();
    let screen = crate::thinker::accounts_screen();
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let r = async {
        crate::browser::sessions::ready(&tab, "tiktok", on_user).await?;
        profile(&tab, &uid).await
    }
    .await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    let c = r?;
    let n = c.plays.len().min(12);
    let eng = if c.followers > 0 { c.median_plays() as f64 / c.followers as f64 * 100.0 } else { 0.0 };
    // Cadence from the newest posts by date (pinned old videos come first in the list).
    let mut ts: Vec<i64> = c.posted.iter().copied().filter(|t| *t > 0).collect();
    ts.sort_by(|a, b| b.cmp(a));
    ts.truncate(12);
    let span = if ts.len() > 1 { (ts[0] - ts[ts.len() - 1]) as f64 / 86_400.0 / (ts.len() - 1) as f64 } else { 0.0 };
    let summary = format!("@{} ({}): {} followers, {} likes, {} videos. Median views of the last {n} videos: {} ({eng:.0} % of followers); posts about every {span:.1} days, last {}. Bio: {}{}",
        c.uid, c.nick, k(c.followers), k(c.likes), c.videos, k(c.median_plays()), c.days_since_post().map(|d| format!("{d} days ago")).unwrap_or_else(|| "unknown".into()), c.bio.chars().take(160).collect::<String>(),
        if c.link.is_empty() { String::new() } else { format!(" · link: {}", c.link) });
    Ok((summary, json!({"uid": c.uid, "followers": c.followers, "likes": c.likes, "videos": c.videos, "median_views": c.median_plays(), "views_recent": c.plays, "captions": c.captions, "bio": c.bio, "link": c.link})))
}

#[cfg(test)]
mod tests {
    #[test]
    fn median_not_mean() {
        let c = super::Creator { plays: vec![900, 1200, 1500, 2_000_000, 1100], ..Default::default() };
        assert_eq!(c.median_plays(), 1200);
        let r = |x: serde_json::Value| super::parse_range(&x);
        assert_eq!(r(serde_json::json!("10000-100000")), Some((10000, 100000)));
        assert_eq!(r(serde_json::json!("10k–100k")), Some((10000, 100000)));
        assert_eq!(r(serde_json::json!("10.000 bis 100.000")), Some((10000, 100000)));
        assert_eq!(r(serde_json::json!([5000, 50000])), Some((5000, 50000)));
        assert!(super::german(&["Heute zeige ich dir die beste Übung für mehr Kraft und Ausdauer".into()]) > 0.1);
    }
}
