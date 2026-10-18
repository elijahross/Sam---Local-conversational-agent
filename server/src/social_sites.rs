//! The rest of the social integrations, all through normal page visits in Sam's own Chrome
//! (accounts profile): Sam opens the page a person would open and reads the JSON the page loads
//! anyway (CDP network responses) or its visible text — no private API calls, no extra apps.
//! Every request counts against the site's budget (browser/sessions.rs); HTTP 429 trips the pause.
//!
//! - TikTok: what people say (comments under the most-watched videos on a topic), trends in a niche
//!   (hashtags and sounds ranked by recent plays)
//! - X: one account in depth, creator search (people search), trending topics
//! - Instagram: one account in depth, creators from hashtag pages
//! - Facebook: Ad Library (public: a brand's active ads, how long each has run)
//! - Amazon: Movers & Shakers (biggest sales-rank gains)

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::{
    AppState,
    browser::{cdp::{Chrome, Tab, Visibility}, sessions},
    llm::{self, Tier},
    social::{Post, Report},
};

fn jtext(txt: &str) -> Option<Value> {
    let i = txt.find('{')?;
    serde_json::from_str(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok()
}

fn k(n: i64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else if n >= 1000 { format!("{:.0}k", n as f64 / 1e3) } else { n.to_string() }
}

fn median(mut v: Vec<i64>) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort();
    v[v.len() / 2]
}

async fn pause() {
    tokio::time::sleep(Duration::from_millis(6000 + (chrono::Local::now().timestamp_millis() % 5000) as u64)).await;
}

/// Runs `f` with a tab in the accounts profile, after the site's sign-in check.
async fn with_tab<T, F, Fut>(site: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>, f: F) -> anyhow::Result<T>
where
    F: FnOnce(Tab) -> Fut,
    Fut: std::future::Future<Output = (Tab, anyhow::Result<T>)>,
{
    sessions::check(site)?;
    let data = crate::thinker::data_dir();
    let screen = crate::thinker::accounts_screen();
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    if sessions::site(site).is_some_and(|s| s.cookie.is_some()) {
        if let Err(e) = sessions::ready(&tab, site, on_user).await {
            tab.close().await;
            return Err(e);
        }
    }
    let (tab, r) = f(tab).await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    r
}

/// One counted request: navigate or scroll and collect the JSON responses matching `part`.
async fn fetch(tab: &Tab, site: &str, action: &str, part: &str, wait_s: u64) -> anyhow::Result<Vec<Value>> {
    sessions::take(site, 1)?;
    let rs = tab.responses(action, part, Duration::from_secs(wait_s)).await?;
    let mut out = vec![];
    for (status, body) in rs {
        if status == 429 {
            sessions::trip(site, "HTTP 429");
            bail!("{site} is rate-limiting Sam's account; Sam pauses it for a day");
        }
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            out.push(v);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// TikTok: what people say (comments)

const TT_PLAN: &str = "You plan a TikTok search to find out what people say about a topic. Reply ONLY with JSON {\"target\": \"<what stance is measured toward>\", \"phrases\": [2 short video-search phrases in the audience's language that contain the topic]}.";

pub async fn tiktok_research(app: &AppState, question: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<Report> {
    sessions::check("tiktok")?;
    let (txt, u) = llm::complete_tier(app, Tier::Light, TT_PLAN, question, true, 800).await?;
    let plan = jtext(&txt).context("search plan")?;
    let target = plan["target"].as_str().unwrap_or(question).to_string();
    let phrases: Vec<String> = plan["phrases"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).take(2).collect();
    let (posts, requests) = with_tab("tiktok", on_user, |tab| async move {
        let r = async {
            let mut videos: Vec<(String, String, i64, String)> = vec![]; // (id, author, plays, caption)
            let mut req = 0;
            for (i, p) in phrases.iter().enumerate() {
                if i > 0 {
                    pause().await;
                }
                let url = format!("https://www.tiktok.com/search/video?q={}", url::form_urlencoded::byte_serialize(p.as_bytes()).collect::<String>());
                req += 1;
                for v in fetch(&tab, "tiktok", &format!("navigate:{url}"), "/api/search/", 8).await? {
                    let items: Vec<&Value> = v["item_list"].as_array().map(|a| a.iter().collect()).unwrap_or_else(|| v["data"].as_array().into_iter().flatten().filter_map(|x| x.get("item")).collect());
                    for it in items {
                        let id = it["id"].as_str().unwrap_or("").to_string();
                        if !id.is_empty() && !videos.iter().any(|x| x.0 == id) {
                            videos.push((id, it["author"]["uniqueId"].as_str().unwrap_or("").into(), it["stats"]["playCount"].as_i64().unwrap_or(0), it["desc"].as_str().unwrap_or("").into()));
                        }
                    }
                }
            }
            videos.sort_by_key(|v| -v.2);
            let mut posts = vec![];
            // The comments under the most-watched videos: what viewers actually say.
            for (vid, author, _, caption) in videos.iter().take(6) {
                pause().await;
                req += 1;
                let url = format!("https://www.tiktok.com/@{author}/video/{vid}");
                // Comments load when the comment panel opens (a click, like a viewer's).
                sessions::take("tiktok", 1)?;
                tab.goto(&url, Duration::from_secs(30)).await?;
                tokio::time::sleep(Duration::from_millis(2500)).await;
                for v in fetch(&tab, "tiktok", "click:[data-e2e=\"comment-icon\"]", "/api/comment/list", 8).await? {
                    for c in v["comments"].as_array().into_iter().flatten() {
                        let text = c["text"].as_str().unwrap_or("").to_string();
                        if text.chars().count() < 4 {
                            continue;
                        }
                        posts.push(Post { id: format!("{vid}?c={}", c["cid"].as_str().unwrap_or("")), user: c["user"]["unique_id"].as_str().unwrap_or("").into(),
                            text: format!("{text}  (on: {})", caption.chars().take(60).collect::<String>()), likes: c["digg_count"].as_i64().unwrap_or(0), reposts: 0,
                            replies: c["reply_comment_total"].as_i64().unwrap_or(0), followers: 0, is_reply: true, stance: String::new() });
                    }
                }
            }
            Ok::<_, anyhow::Error>((posts, req))
        }
        .await;
        (tab, r)
    })
    .await?;
    if posts.is_empty() {
        bail!("no TikTok comments found for this topic");
    }
    let mut r = crate::social::analyse(app, question, &target, posts, requests, "TikTok", "https://www.tiktok.com/video/{id}").await?;
    r.tokens += u.prompt + u.completion;
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// X: one account, creator search, trending topics

fn x_user(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(o) => {
            if o.get("__typename").and_then(|t| t.as_str()) == Some("User") && o.contains_key("legacy") {
                let lg = &v["legacy"];
                out.push(json!({"handle": v["core"]["screen_name"].as_str().or(lg["screen_name"].as_str()).unwrap_or(""), "name": v["core"]["name"].as_str().or(lg["name"].as_str()).unwrap_or(""),
                    "bio": lg["description"].as_str().or(v["profile_bio"]["description"].as_str()).unwrap_or(""), "followers": lg["followers_count"].as_i64().unwrap_or(0),
                    "following": lg["friends_count"].as_i64().unwrap_or(0), "posts": lg["statuses_count"].as_i64().unwrap_or(0), "verified": v["is_blue_verified"].as_bool().unwrap_or(false),
                    "location": v["location"]["location"].as_str().or(lg["location"].as_str()).unwrap_or(""), "created": v["core"]["created_at"].as_str().or(lg["created_at"].as_str()).unwrap_or("")}));
                return;
            }
            o.values().for_each(|x| x_user(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| x_user(x, out)),
        _ => {}
    }
}

/// Tweets with likes / views from any GraphQL body.
fn x_tweets(v: &Value, out: &mut Vec<(i64, i64, i64, String)>) {
    match v {
        Value::Object(o) => {
            if o.get("__typename").and_then(|t| t.as_str()) == Some("Tweet") && o.contains_key("legacy") {
                let lg = &v["legacy"];
                let views = v["views"]["count"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                out.push((lg["favorite_count"].as_i64().unwrap_or(0), lg["retweet_count"].as_i64().unwrap_or(0), views, lg["created_at"].as_str().unwrap_or("").to_string()));
                return;
            }
            o.values().for_each(|x| x_tweets(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| x_tweets(x, out)),
        _ => {}
    }
}

pub async fn x_profile(handle: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, Value)> {
    let h = handle.trim().trim_start_matches("https://x.com/").trim_start_matches("https://twitter.com/").trim_start_matches('@').split(['/', '?']).next().unwrap_or("").to_string();
    if h.is_empty() {
        bail!("which X account? (@name or profile link)");
    }
    let h2 = h.clone();
    let (users, tweets) = with_tab("x", on_user, |tab| async move {
        let r = async {
            let mut users = vec![];
            let mut tweets = vec![];
            for v in fetch(&tab, "x", &format!("navigate:https://x.com/{h2}"), "/graphql/", 9).await? {
                x_user(&v, &mut users);
                x_tweets(&v, &mut tweets);
            }
            Ok::<_, anyhow::Error>((users, tweets))
        }
        .await;
        (tab, r)
    })
    .await?;
    let u = users.iter().find(|u| u["handle"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(&h))).cloned().context("X shows no such account (or it is private)")?;
    let likes = median(tweets.iter().map(|t| t.0).collect());
    let views = median(tweets.iter().map(|t| t.2).collect());
    let summary = format!("@{} ({}): {} followers, {} following, {} posts{}. Median of the last {} posts: {} likes, {} views. Bio: {}",
        u["handle"].as_str().unwrap_or(""), u["name"].as_str().unwrap_or(""), k(u["followers"].as_i64().unwrap_or(0)), k(u["following"].as_i64().unwrap_or(0)), k(u["posts"].as_i64().unwrap_or(0)),
        if u["verified"] == true { ", verified" } else { "" }, tweets.len(), k(likes), k(views), u["bio"].as_str().unwrap_or("").chars().take(160).collect::<String>());
    Ok((summary, json!({"user": u, "recent": tweets.len(), "median_likes": likes, "median_views": views})))
}

const X_CREATORS_PLAN: &str = "You plan an X (Twitter) people search for outreach. Reply ONLY with JSON {\"queries\": [2-3 short people-search queries: topic words such accounts have in their name or bio, in their language], \"niche\": \"one sentence: who fits\"}.";
const FIT_SYS: &str = "You check social media accounts for an outreach list. For EVERY numbered account first say who it is (t: 3-8 words), then fit = true only if it is a real individual creator / voice in the requested niche AND audience/place. fit = false for brands, news outlets, bots, parody or fan pages. Ignore follower numbers. Reply ONLY with JSON {\"v\": [{\"n\": 1, \"t\": \"…\", \"fit\": true|false}, ...]}.";

pub async fn x_creators(app: &AppState, brief: &str, range: Option<(i64, i64)>, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, String, Value, u64)> {
    sessions::check("x")?;
    let (txt, u) = llm::complete_tier(app, Tier::Light, X_CREATORS_PLAN, brief, true, 800).await?;
    let mut tokens = u.prompt + u.completion;
    let plan = jtext(&txt).context("search plan")?;
    let queries: Vec<String> = plan["queries"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).take(3).collect();
    let users = with_tab("x", on_user, |tab| async move {
        let r = async {
            let mut all: Vec<Value> = vec![];
            for (i, q) in queries.iter().enumerate() {
                if i > 0 {
                    pause().await;
                }
                let url = format!("https://x.com/search?q={}&src=typed_query&f=user", url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>());
                for step in 0..2 {
                    let action = if step == 0 { format!("navigate:{url}") } else { "scroll".into() };
                    for v in fetch(&tab, "x", &action, "SearchTimeline", 8).await? {
                        let mut got = vec![];
                        x_user(&v, &mut got);
                        for g in got {
                            if !all.iter().any(|a| a["handle"] == g["handle"]) {
                                all.push(g);
                            }
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(all)
        }
        .await;
        (tab, r)
    })
    .await?;
    let mut cand: Vec<Value> = users.into_iter().filter(|u| range.is_none_or(|(lo, hi)| u["followers"].as_i64().is_some_and(|f| f >= lo && f <= hi))).collect();
    cand.truncate(40);
    if cand.is_empty() {
        bail!("no X accounts in that follower range for this search");
    }
    let lines: Vec<String> = cand.iter().enumerate().map(|(i, u)| format!("{}. @{} ({}) | bio: {} | location: {}", i + 1, u["handle"].as_str().unwrap_or(""), u["name"].as_str().unwrap_or(""), u["bio"].as_str().unwrap_or("").chars().take(160).collect::<String>(), u["location"].as_str().unwrap_or(""))).collect();
    let (txt, u) = llm::complete_tier(app, Tier::Light, FIT_SYS, &format!("Request: {brief}\nWho fits: {}\n\nAccounts:\n{}", plan["niche"].as_str().unwrap_or(""), lines.join("\n")), true, 4000).await?;
    tokens += u.prompt + u.completion;
    let fit: std::collections::HashMap<usize, (bool, String)> = jtext(&txt).and_then(|j| j["v"].as_array().map(|a| a.iter().filter_map(|x| Some((x["n"].as_u64()? as usize, (x["fit"].as_bool().unwrap_or(false), x["t"].as_str().unwrap_or("").to_string())))).collect())).unwrap_or_default();
    let mut fits: Vec<Value> = cand.iter().enumerate().filter_map(|(i, u)| fit.get(&(i + 1)).filter(|f| f.0).map(|f| { let mut u = u.clone(); u["about"] = json!(f.1); u["url"] = json!(format!("https://x.com/{}", u["handle"].as_str().unwrap_or(""))); u })).collect();
    fits.sort_by_key(|u| -u["followers"].as_i64().unwrap_or(0));
    let mut md = format!("# {brief}\n\n| Account | Followers | Posts | About | Bio |\n|---|---|---|---|---|\n");
    for u in &fits {
        md.push_str(&format!("| [@{}]({}) | {} | {} | {} | {} |\n", u["handle"].as_str().unwrap_or(""), u["url"].as_str().unwrap_or(""), k(u["followers"].as_i64().unwrap_or(0)), k(u["posts"].as_i64().unwrap_or(0)), u["about"].as_str().unwrap_or(""), u["bio"].as_str().unwrap_or("").replace('|', "/").chars().take(100).collect::<String>()));
    }
    let summary = if fits.is_empty() { format!("No fitting X accounts among {} checked.", cand.len()) } else {
        format!("{} X accounts fit: {}.", fits.len(), fits.iter().take(8).map(|u| format!("@{} ({} followers)", u["handle"].as_str().unwrap_or(""), k(u["followers"].as_i64().unwrap_or(0)))).collect::<Vec<_>>().join(", "))
    };
    Ok((summary, md, json!({"brief": brief, "checked": cand.len(), "qualified": fits}), tokens))
}

// ---------------------------------------------------------------------------------------------
// Instagram: one account, creators from hashtag pages

fn ig_user(v: &Value) -> Option<Value> {
    // web_profile_info: data.user; other shapes: find an object with edge_followed_by.
    fn find<'a>(v: &'a Value) -> Option<&'a Value> {
        match v {
            Value::Object(o) => {
                if o.contains_key("edge_followed_by") && o.contains_key("username") {
                    return Some(v);
                }
                o.values().find_map(find)
            }
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    let u = find(v)?;
    let media: Vec<&Value> = u["edge_owner_to_timeline_media"]["edges"].as_array().map(|e| e.iter().map(|x| &x["node"]).collect()).unwrap_or_default();
    let likes: Vec<i64> = media.iter().map(|m| m["edge_liked_by"]["count"].as_i64().or(m["edge_media_preview_like"]["count"].as_i64()).unwrap_or(0)).collect();
    let views: Vec<i64> = media.iter().filter_map(|m| m["video_view_count"].as_i64()).collect();
    let last = media.iter().filter_map(|m| m["taken_at_timestamp"].as_i64()).max();
    Some(json!({"handle": u["username"], "name": u["full_name"], "bio": u["biography"], "followers": u["edge_followed_by"]["count"], "following": u["edge_follow"]["count"],
        "posts": u["edge_owner_to_timeline_media"]["count"], "business": u["is_business_account"], "category": u["category_name"], "email": u["business_email"], "link": u["external_url"],
        "median_likes": median(likes), "median_video_views": median(views), "days_since_post": last.map(|t| (chrono::Local::now().timestamp() - t) / 86_400)}))
}

async fn ig_profile_on(tab: &Tab, h: &str) -> anyhow::Result<Value> {
    let bodies = fetch(tab, "instagram", &format!("navigate:https://www.instagram.com/{h}/"), "instagram.com/", 9).await?;
    if let Some(u) = bodies.iter().find_map(ig_user) {
        return Ok(u);
    }
    // The page's own description: "1,234 Followers, 56 Following, 78 Posts - …".
    let d = tab.eval("(document.querySelector('meta[property=\"og:description\"]')||{}).content||''").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    let num = |word: &str| regex::Regex::new(&format!(r"([\d.,]+\s*[KkMm]?)\s+{word}")).ok().and_then(|r| r.captures(&d)).map(|c| c[1].replace([',', ' '], "")).and_then(|s| {
        let (n, m) = if let Some(x) = s.strip_suffix(['K', 'k']) { (x.to_string(), 1e3) } else if let Some(x) = s.strip_suffix(['M', 'm']) { (x.to_string(), 1e6) } else { (s.replace('.', ""), 1.0) };
        n.parse::<f64>().ok().map(|x| (x * m) as i64)
    });
    if d.is_empty() {
        bail!("Instagram shows no such account (or the session needs a check)");
    }
    Ok(json!({"handle": h, "followers": num("Followers"), "following": num("Following"), "posts": num("Posts"), "bio": d}))
}

pub async fn ig_profile(handle: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, Value)> {
    let h = handle.trim().trim_start_matches("https://www.instagram.com/").trim_start_matches('@').split(['/', '?']).next().unwrap_or("").to_string();
    if h.is_empty() {
        bail!("which Instagram account? (@name or profile link)");
    }
    let h2 = h.clone();
    let u = with_tab("instagram", on_user, |tab| async move {
        let r = ig_profile_on(&tab, &h2).await;
        (tab, r)
    })
    .await?;
    let eng = match (u["median_likes"].as_i64(), u["followers"].as_i64()) { (Some(l), Some(f)) if f > 0 => format!(" ({:.1} % of followers)", l as f64 / f as f64 * 100.0), _ => String::new() };
    let summary = format!("@{h} on Instagram: {} followers, {} following, {} posts{}. Median likes of recent posts: {}{eng}{}. {}{}",
        k(u["followers"].as_i64().unwrap_or(0)), k(u["following"].as_i64().unwrap_or(0)), k(u["posts"].as_i64().unwrap_or(0)), u["category"].as_str().map(|c| format!(", {c}")).unwrap_or_default(),
        k(u["median_likes"].as_i64().unwrap_or(0)), u["days_since_post"].as_i64().map(|d| format!("; last post {d} days ago")).unwrap_or_default(),
        u["bio"].as_str().unwrap_or("").chars().take(160).collect::<String>(), u["email"].as_str().filter(|e| !e.is_empty()).map(|e| format!(" · contact: {e}")).unwrap_or_default());
    Ok((summary, u))
}

const IG_PLAN: &str = "You plan an Instagram creator search for outreach. Reply ONLY with JSON {\"hashtags\": [2-3 hashtags without # that such creators use, in their language / city, e.g. wienfood, fitnessdeutschland], \"niche\": \"one sentence: who fits\"}.";

pub async fn ig_creators(app: &AppState, brief: &str, range: Option<(i64, i64)>, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, String, Value, u64)> {
    sessions::check("instagram")?;
    let (txt, u) = llm::complete_tier(app, Tier::Light, IG_PLAN, brief, true, 800).await?;
    let mut tokens = u.prompt + u.completion;
    let plan = jtext(&txt).context("search plan")?;
    let tags: Vec<String> = plan["hashtags"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|t| t.trim_start_matches('#').to_string())).take(3).collect();
    let profiles = with_tab("instagram", on_user, |tab| async move {
        let r = async {
            let mut names: Vec<String> = vec![];
            for (i, t) in tags.iter().enumerate() {
                if i > 0 {
                    pause().await;
                }
                for v in fetch(&tab, "instagram", &format!("navigate:https://www.instagram.com/explore/tags/{t}/"), "instagram.com/", 9).await? {
                    let s = v.to_string();
                    for c in regex::Regex::new(r#""username":"([A-Za-z0-9._]{2,30})""#).unwrap().captures_iter(&s) {
                        if !names.contains(&c[1].to_string()) {
                            names.push(c[1].to_string());
                        }
                    }
                }
            }
            // Instagram's budget is the tightest (150/day): at most 10 profile views per run.
            let mut out = vec![];
            for n in names.iter().take(10) {
                pause().await;
                match ig_profile_on(&tab, n).await {
                    Ok(p) => out.push(p),
                    Err(e) if e.to_string().contains("budget") || e.to_string().contains("rate-limiting") => break,
                    Err(_) => {}
                }
            }
            Ok::<_, anyhow::Error>(out)
        }
        .await;
        (tab, r)
    })
    .await?;
    let cand: Vec<Value> = profiles.into_iter().filter(|p| range.is_none_or(|(lo, hi)| p["followers"].as_i64().is_some_and(|f| f >= lo && f <= hi))).collect();
    if cand.is_empty() {
        bail!("no Instagram accounts in that range on these hashtag pages");
    }
    let lines: Vec<String> = cand.iter().enumerate().map(|(i, p)| format!("{}. @{} | {} | bio: {}", i + 1, p["handle"].as_str().unwrap_or(""), p["category"].as_str().unwrap_or(""), p["bio"].as_str().unwrap_or("").chars().take(160).collect::<String>())).collect();
    let (txt, u) = llm::complete_tier(app, Tier::Light, FIT_SYS, &format!("Request: {brief}\nWho fits: {}\n\nAccounts:\n{}", plan["niche"].as_str().unwrap_or(""), lines.join("\n")), true, 3000).await?;
    tokens += u.prompt + u.completion;
    let fit: std::collections::HashSet<usize> = jtext(&txt).and_then(|j| j["v"].as_array().map(|a| a.iter().filter(|x| x["fit"] == true).filter_map(|x| x["n"].as_u64().map(|n| n as usize)).collect())).unwrap_or_default();
    let fits: Vec<Value> = cand.iter().enumerate().filter(|(i, _)| fit.contains(&(i + 1))).map(|(_, p)| p.clone()).collect();
    let mut md = format!("# {brief}\n\n| Account | Followers | Median likes | Last post | Contact | Bio |\n|---|---|---|---|---|---|\n");
    for p in &fits {
        md.push_str(&format!("| [@{0}](https://www.instagram.com/{0}/) | {1} | {2} | {3} | {4} | {5} |\n", p["handle"].as_str().unwrap_or(""), k(p["followers"].as_i64().unwrap_or(0)), k(p["median_likes"].as_i64().unwrap_or(0)),
            p["days_since_post"].as_i64().map(|d| format!("{d} d")).unwrap_or_default(), p["email"].as_str().unwrap_or(""), p["bio"].as_str().unwrap_or("").replace(['|', '\n'], " ").chars().take(100).collect::<String>()));
    }
    let summary = if fits.is_empty() { format!("No fitting Instagram creators among {} checked.", cand.len()) } else { format!("{} Instagram creators fit: {}.", fits.len(), fits.iter().map(|p| format!("@{} ({} followers)", p["handle"].as_str().unwrap_or(""), k(p["followers"].as_i64().unwrap_or(0)))).collect::<Vec<_>>().join(", ")) };
    Ok((summary, md, json!({"brief": brief, "qualified": fits, "checked": cand.len()}), tokens))
}

// ---------------------------------------------------------------------------------------------
// Trends: X trending, TikTok hashtags in a niche, Amazon Movers & Shakers

const TT_TREND_PLAN: &str = "You plan TikTok searches to find what is trending in a niche. Reply ONLY with JSON {\"phrases\": [3 short video-search phrases for the niche in the audience's language]}.";

pub async fn trends(app: &AppState, site: &str, niche: &str, country: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<(String, String, Value, u64)> {
    match site {
        "x" | "twitter" => {
            let items = with_tab("x", on_user, |tab| async move {
                let r = async {
                    sessions::take("x", 1)?;
                    tab.goto("https://x.com/explore/tabs/trending", Duration::from_secs(30)).await?;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let v = tab.eval("[...document.querySelectorAll('[data-testid=\"trend\"]')].map(e => e.innerText.split('\\n').map(x => x.trim()).filter(Boolean))").await?;
                    Ok::<_, anyhow::Error>(v.as_array().cloned().unwrap_or_default())
                }
                .await;
                (tab, r)
            })
            .await?;
            let rows: Vec<Value> = items.iter().filter_map(|lines| {
                let l: Vec<&str> = lines.as_array()?.iter().filter_map(|x| x.as_str()).collect();
                let name = l.iter().find(|x| !x.contains("Trending") && !x.contains("·") && !x.ends_with("posts") && !x.ends_with("Beiträge") && x.parse::<u32>().is_err())?;
                Some(json!({"topic": name, "context": l.iter().find(|x| x.contains("Trending") || x.contains("·")).unwrap_or(&""), "posts": l.iter().find(|x| x.ends_with("posts") || x.ends_with("Beiträge")).unwrap_or(&"")}))
            }).collect();
            if rows.is_empty() {
                bail!("X showed no trends (signed out or the page changed)");
            }
            let md = format!("# Trending on X\n\n| # | Topic | Posts | Context |\n|---|---|---|---|\n{}", rows.iter().enumerate().map(|(i, r)| format!("| {} | {} | {} | {} |\n", i + 1, r["topic"].as_str().unwrap_or(""), r["posts"].as_str().unwrap_or(""), r["context"].as_str().unwrap_or(""))).collect::<String>());
            let summary = format!("Trending on X for Sam's account location: {}.", rows.iter().take(12).map(|r| r["topic"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(", "));
            Ok((summary, md, json!({"site": "x", "trends": rows}), 0))
        }
        "tiktok" => {
            sessions::check("tiktok")?;
            let (txt, u) = llm::complete_tier(app, Tier::Light, TT_TREND_PLAN, &format!("Niche: {niche}\nCountry: {country}"), true, 600).await?;
            let phrases: Vec<String> = jtext(&txt).and_then(|j| j["phrases"].as_array().cloned()).unwrap_or_default().iter().filter_map(|x| x.as_str().map(str::to_string)).take(3).collect();
            let vids = with_tab("tiktok", on_user, |tab| async move {
                let r = async {
                    let mut vids: Vec<Value> = vec![];
                    for (i, p) in phrases.iter().enumerate() {
                        if i > 0 {
                            pause().await;
                        }
                        let url = format!("https://www.tiktok.com/search/video?q={}", url::form_urlencoded::byte_serialize(p.as_bytes()).collect::<String>());
                        for step in 0..2 {
                            let action = if step == 0 { format!("navigate:{url}") } else { "scroll".into() };
                            for v in fetch(&tab, "tiktok", &action, "/api/search/", 8).await? {
                                let items: Vec<&Value> = v["item_list"].as_array().map(|a| a.iter().collect()).unwrap_or_else(|| v["data"].as_array().into_iter().flatten().filter_map(|x| x.get("item")).collect());
                                vids.extend(items.into_iter().cloned());
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(vids)
                }
                .await;
                (tab, r)
            })
            .await?;
            // Hashtags and sounds weighted by plays of videos from the last 14 days ("rising"), vs all.
            let now = chrono::Local::now().timestamp();
            let mut tags: std::collections::HashMap<String, (i64, i64, usize)> = Default::default();
            let mut sounds: std::collections::HashMap<String, (i64, usize)> = Default::default();
            let mut seen = std::collections::HashSet::new();
            for v in &vids {
                if !seen.insert(v["id"].as_str().unwrap_or("").to_string()) {
                    continue;
                }
                let plays = v["stats"]["playCount"].as_i64().unwrap_or(0);
                let recent = now - v["createTime"].as_i64().unwrap_or(0) <= 14 * 86_400;
                let desc = v["desc"].as_str().unwrap_or("").to_lowercase();
                for t in regex::Regex::new(r"#([\p{L}\p{N}_]{2,40})").unwrap().captures_iter(&desc) {
                    let e = tags.entry(t[1].to_string()).or_default();
                    e.0 += plays;
                    if recent { e.1 += plays; }
                    e.2 += 1;
                }
                if let Some(m) = v["music"]["title"].as_str().filter(|m| !m.starts_with("original sound") && !m.starts_with("Originalton")) {
                    let e = sounds.entry(m.to_string()).or_default();
                    e.0 += plays;
                    e.1 += 1;
                }
            }
            let mut tl: Vec<(String, (i64, i64, usize))> = tags.into_iter().filter(|(_, v)| v.2 >= 2).collect();
            tl.sort_by_key(|(_, v)| -v.1);
            let mut sl: Vec<(String, (i64, usize))> = sounds.into_iter().filter(|(_, v)| v.1 >= 2).collect();
            sl.sort_by_key(|(_, v)| -v.0);
            let mut md = format!("# TikTok trends: {niche} ({country})\n\nFrom {} recent videos in TikTok search.\n\n| Hashtag | Videos | Plays (last 14 days) | Plays (all) |\n|---|---|---|---|\n", seen.len());
            for (t, (all, rec, n)) in tl.iter().take(25) {
                md.push_str(&format!("| #{t} | {n} | {} | {} |\n", k(*rec), k(*all)));
            }
            if !sl.is_empty() {
                md.push_str("\n| Sound | Videos | Plays |\n|---|---|---|\n");
                for (s, (p, n)) in sl.iter().take(10) {
                    md.push_str(&format!("| {s} | {n} | {} |\n", k(*p)));
                }
            }
            let summary = format!("Rising on TikTok in {niche}: {}{}.", tl.iter().take(10).map(|(t, v)| format!("#{t} ({} recent plays)", k(v.1))).collect::<Vec<_>>().join(", "),
                if sl.is_empty() { String::new() } else { format!("; sounds: {}", sl.iter().take(4).map(|(s, _)| s.clone()).collect::<Vec<_>>().join(", ")) });
            Ok((summary, md, json!({"site": "tiktok", "niche": niche, "videos": seen.len(), "hashtags": tl.iter().take(40).map(|(t, v)| json!({"tag": t, "recent_plays": v.1, "plays": v.0, "videos": v.2})).collect::<Vec<_>>()}), u.prompt + u.completion))
        }
        "amazon" => {
            // Category from the niche (Amazon.de list slugs); Movers & Shakers first, and when Amazon
            // shows none ("keine Aufsteiger verfügbar", common on quiet days) the New Releases list.
            let n = niche.to_lowercase();
            let cat = [("küche", "kitchen"), ("kitchen", "kitchen"), ("haushalt", "kitchen"), ("elektron", "ce-de"), ("electronic", "ce-de"), ("computer", "computers"), ("beauty", "beauty"),
                ("kosmetik", "beauty"), ("drogerie", "drugstore"), ("health", "drugstore"), ("gesundheit", "drugstore"), ("supplement", "drugstore"), ("sport", "sports"), ("fitness", "sports"), ("spielzeug", "toys"), ("toy", "toys"),
                ("baumarkt", "diy"), ("werkzeug", "diy"), ("tool", "diy"), ("garten", "garden"), ("garden", "garden"), ("haustier", "pet-supplies"), ("pet", "pet-supplies"), ("baby", "baby"),
                ("fashion", "fashion"), ("mode", "fashion"), ("buch", "books"), ("book", "books"), ("game", "videogames"), ("auto", "automotive"), ("büro", "officeproduct"), ("office", "officeproduct")]
                .iter().find(|(k, _)| n.contains(k)).map(|x| x.1).unwrap_or("kitchen");
            let data = crate::thinker::data_dir();
            let screen = crate::config::env_num("BG_SCREEN", 1u16);
            let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
            let tab = chrome.new_tab().await?;
            let r = async {
                let mut got = (String::new(), Value::Null);
                for (list, label) in [("movers-and-shakers", "Movers & Shakers (biggest sales-rank gains in 24 h)"), ("new-releases", "New Releases (best-selling new products)"), ("bestsellers", "Bestsellers")] {
                    tab.goto(&format!("https://www.amazon.de/gp/{list}/{cat}/"), Duration::from_secs(30)).await?;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if tab.eval("(()=>{const b=document.querySelector('#sp-cc-accept'); if(b){b.click(); return true} return false})()").await.ok().and_then(|v| v.as_bool()).unwrap_or(false) {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    for _ in 0..3 {
                        let _ = tab.eval("window.scrollBy(0, 1500)").await;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    }
                    let v = tab.eval(r#"[...document.querySelectorAll('[id^="gridItemRoot"]')].map(e => { const a = e.querySelector('a[href*="/dp/"]'); return {lines: (e.innerText || '').split('\n').map(x => x.trim()).filter(Boolean).slice(0, 8), url: a ? a.href.split('?')[0].split('/ref=')[0] : ''}; })"#).await?;
                    if v.as_array().is_some_and(|a| a.len() >= 5) {
                        got = (label.to_string(), v);
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Ok::<_, anyhow::Error>(got)
            }
            .await;
            tab.close().await;
            let (label, items) = r?;
            let rows: Vec<Value> = items.as_array().into_iter().flatten().filter_map(|it| {
                let l: Vec<&str> = it["lines"].as_array()?.iter().filter_map(|x| x.as_str()).collect();
                let rank = l.iter().find(|x| x.starts_with('#')).map(|x| x.to_string()).unwrap_or_default();
                let title = l.iter().filter(|x| x.len() > 15 && !x.contains('%') && !x.contains('€') && !x.contains("Sternen") && !x.contains("stars")).max_by_key(|x| x.len())?.to_string();
                Some(json!({"rank": rank, "title": title, "gain": l.iter().find(|x| x.contains('%')).unwrap_or(&""), "rating": l.iter().find(|x| x.contains("Sternen") || x.contains("stars")).unwrap_or(&""),
                    "price": l.iter().find(|x| x.contains('€')).map(|x| x.split("ab ").last().unwrap_or(x)).unwrap_or(""), "url": it["url"]}))
            }).collect();
            if rows.is_empty() {
                bail!("Amazon showed no list for this category (bot check or page change)");
            }
            let md = format!("# Amazon.de {label}: {cat}\n\n| # | Product | Gain | Rating | Price |\n|---|---|---|---|---|\n{}", rows.iter().map(|r| format!("| {} | [{}]({}) | {} | {} | {} |\n", r["rank"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or("").replace('|', "/").chars().take(80).collect::<String>(), r["url"].as_str().unwrap_or(""), r["gain"].as_str().unwrap_or(""), r["rating"].as_str().unwrap_or(""), r["price"].as_str().unwrap_or(""))).collect::<String>());
            let summary = format!("Amazon.de {label} in {cat}: {}.", rows.iter().take(8).map(|r| format!("{} {}{}", r["rank"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or("").chars().take(50).collect::<String>(), r["gain"].as_str().filter(|g| !g.is_empty()).map(|g| format!(" ({g})")).unwrap_or_default())).collect::<Vec<_>>().join("; "));
            Ok((summary, md, json!({"site": "amazon", "list": label, "category": cat, "items": rows}), 0))
        }
        other => bail!("trends on {other} aren't available (x, tiktok, amazon)"),
    }
}

// ---------------------------------------------------------------------------------------------
// Facebook Ad Library (public, no login)

pub async fn fb_ads(advertiser: &str, country: &str) -> anyhow::Result<(String, String, Value)> {
    let data = crate::thinker::data_dir();
    let screen = crate::config::env_num("BG_SCREEN", 1u16);
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let cc = match country.trim().to_lowercase().as_str() {
        "" | "at" | "austria" | "österreich" | "oesterreich" => "AT".to_string(),
        "de" | "germany" | "deutschland" => "DE".to_string(),
        "ch" | "switzerland" | "schweiz" => "CH".to_string(),
        "us" | "usa" | "united states" => "US".to_string(),
        "gb" | "uk" | "united kingdom" => "GB".to_string(),
        c if c.len() == 2 => c.to_uppercase(),
        _ => "AT".to_string(),
    };
    let url = format!("https://www.facebook.com/ads/library/?active_status=active&ad_type=all&country={cc}&q={}&search_type=keyword_unordered&media_type=all", url::form_urlencoded::byte_serialize(advertiser.as_bytes()).collect::<String>());
    let r = async {
        tab.goto(&url, Duration::from_secs(30)).await?;
        tokio::time::sleep(Duration::from_secs(6)).await;
        for _ in 0..3 {
            let _ = tab.eval("window.scrollBy(0, document.body.scrollHeight)").await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        tab.eval("document.body.innerText").await
    }
    .await;
    tab.close().await;
    let text = r?.as_str().unwrap_or("").to_string();
    // Each ad card starts with its library id ("Library ID: …" / "Bibliotheks-ID: …").
    let re = regex::Regex::new(r"(?:Library ID|Bibliotheks-ID|ID der Werbeanzeigenbibliothek):?\s*(\d{6,})").unwrap();
    let starts: Vec<(usize, String)> = re.captures_iter(&text).map(|c| (c.get(0).unwrap().start(), c[1].to_string())).collect();
    let mut ads = vec![];
    for (i, (pos, id)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map(|x| x.0).unwrap_or(text.len());
        let block = &text[*pos..end];
        let since = regex::Regex::new(r"(?i)(?:started running on|läuft seit|seit dem|seit)\s+(?:dem\s+)?(\d{1,2}\.\s?\d{1,2}\.\s?\d{4}|\d{1,2}\.?\s+\p{L}+\.?\s+\d{4}|\p{L}+\s+\d{1,2},\s+\d{4})").unwrap().captures(block).map(|c| c[1].trim().to_string()).unwrap_or_default();
        let body: String = block.lines().map(str::trim).filter(|l| l.len() > 40 && !l.contains("Library ID") && !l.contains("Bibliotheks-ID")).take(2).collect::<Vec<_>>().join(" ").chars().take(300).collect();
        let platforms = ["Facebook", "Instagram", "Messenger", "Audience Network"].iter().filter(|p| block.contains(*p)).cloned().collect::<Vec<_>>().join(", ");
        ads.push(json!({"id": id, "since": since, "text": body, "platforms": platforms, "url": format!("https://www.facebook.com/ads/library/?id={id}")}));
    }
    if ads.is_empty() {
        bail!("the Ad Library shows no active ads for \"{advertiser}\" in {cc} (or asked for a check)");
    }
    let md = format!("# Active ads: {advertiser} ({cc})\n\n| Running since | Platforms | Text | Ad |\n|---|---|---|---|\n{}", ads.iter().map(|a| format!("| {} | {} | {} | [{}]({}) |\n", a["since"].as_str().unwrap_or(""), a["platforms"].as_str().unwrap_or(""), a["text"].as_str().unwrap_or("").replace('|', "/"), a["id"].as_str().unwrap_or(""), a["url"].as_str().unwrap_or(""))).collect::<String>());
    let summary = format!("{} active ads for \"{advertiser}\" in the Meta Ad Library ({cc}). Longest running (usually the winners): {}.", ads.len(),
        ads.iter().rev().take(3).map(|a| format!("since {} — {}", a["since"].as_str().unwrap_or("?"), a["text"].as_str().unwrap_or("").chars().take(80).collect::<String>())).collect::<Vec<_>>().join("; "));
    Ok((summary, md, json!({"advertiser": advertiser, "country": cc, "ads": ads})))
}
