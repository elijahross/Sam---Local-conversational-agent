//! Reddit in Sam's own browser ("accounts" profile, signed in once via Connectors › Reddit).
//! Logged out, Reddit answers bots with "Prove your humanity" and plain HTTP with 403, and
//! old.reddit requires a login — so everything goes through the signed-in page: its own JSON
//! endpoints, fetched in page context (cookies, same origin), never HTML scraping.

use std::time::{Duration, Instant};

use anyhow::bail;
use serde_json::Value;

use super::cdp::Tab;

pub const HOME: &str = "https://www.reddit.com/";

/// GET a reddit.com path in the page (JSON).
pub async fn get(tab: &Tab, path: &str) -> anyhow::Result<Value> {
    let js = format!(
        "(async () => {{ try {{ const r = await fetch({}, {{credentials: 'include', headers: {{'Accept': 'application/json'}}}}); return [r.status, await r.text()]; }} catch (e) {{ return [0, String(e)]; }} }})()",
        serde_json::to_string(path)?
    );
    let v = tab.eval(&js).await?;
    let (status, body) = (v[0].as_i64().unwrap_or(0), v[1].as_str().unwrap_or(""));
    if status != 200 {
        bail!("reddit {path}: HTTP {status}");
    }
    Ok(serde_json::from_str(body)?)
}

/// The signed-in username, if any.
pub async fn me(tab: &Tab) -> Option<String> {
    get(tab, "/api/me.json").await.ok().and_then(|v| v["data"]["name"].as_str().or(v["name"].as_str()).map(str::to_string))
}

/// Opens Reddit and waits until a session is signed in; shows the window and asks the user to
/// sign in when needed (Connectors › Reddit › Link now, or the first Reddit task).
pub async fn ready(tab: &Tab, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<String> {
    let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if !here.starts_with(HOME) {
        tab.goto(HOME, Duration::from_secs(30)).await?;
    }
    if let Some(u) = me(tab).await {
        return Ok(u);
    }
    tab.goto("https://www.reddit.com/login/", Duration::from_secs(30)).await?;
    // The login saved under Connectors › Website logins signs in without the user.
    if matches!(super::login::sign_in(tab, "reddit.com").await, Ok(super::login::SignIn::Submitted)) {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Some(u) = me(tab).await {
                crate::connectors::mark_linked("reddit");
                return Ok(u);
            }
        }
        tracing::warn!("reddit: the saved login didn't sign in (wrong password, or a check the site added)");
    }
    tab.show_window(true).await;
    let ask = "Sam needs its own Reddit sign-in once: sign in in the browser window I just opened (your normal account is fine). I'll continue as soon as you're in.";
    match on_user {
        Some(tx) => {
            let _ = tx.send(ask.into());
        }
        None => eprintln!("\n>>> {ask}"),
    }
    // Unattended runs (tests: SAM_NO_REAL_APPS=1) have nobody to sign in: give up quickly so
    // the rest of the plan goes on and the result says Reddit wasn't available.
    let limit = if crate::native_chat::test_mode() { 15 } else { 600 };
    let end = Instant::now() + Duration::from_secs(limit);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Some(u) = me(tab).await {
            tab.show_window(false).await;
            crate::connectors::mark_linked("reddit");
            return Ok(u);
        }
    }
    bail!("Reddit wasn't signed in within 10 minutes")
}

// ---------------------------------------------------------------------------------------------
// Deep research on Reddit (reddit lab: compaction tuned for the fewest tokens at full recall).

/// How much of each thread reaches the model (defaults = reddit lab best-config.json).
#[derive(Debug, Clone)]
pub struct Compact {
    pub threads: usize,
    pub comments_per_thread: usize,
    pub max_depth: usize,
    pub min_score: i64,
    pub max_chars: usize,
}

impl Default for Compact {
    fn default() -> Self {
        Self { threads: 8, comments_per_thread: 50, max_depth: 8, min_score: 0, max_chars: 400 }
    }
}

/// One thread as fetched: the post plus its comment tree (Reddit's `<permalink>.json`).
pub struct Thread {
    pub id: String,
    pub title: String,
    pub sub: String,
    pub score: i64,
    pub url: String,
    pub text: String,
    /// (depth, score, body) in tree order.
    pub comments: Vec<(usize, i64, String)>,
}

/// Search results (posts) for `query`, optionally inside one subreddit.
pub async fn search(tab: &Tab, query: &str, sub: Option<&str>, limit: usize) -> anyhow::Result<Vec<Value>> {
    let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let path = match sub {
        Some(s) => format!("/r/{s}/search.json?q={q}&restrict_sr=1&limit={limit}&sort=relevance&t=all"),
        None => format!("/search.json?q={q}&limit={limit}&sort=relevance&t=all"),
    };
    let v = get(tab, &path).await?;
    Ok(v["data"]["children"].as_array().cloned().unwrap_or_default().into_iter().map(|c| c["data"].clone()).collect())
}

/// A thread with its comments (top-sorted, up to `depth` levels).
pub async fn thread(tab: &Tab, permalink: &str, depth: usize) -> anyhow::Result<Thread> {
    let v = get(tab, &format!("{}.json?limit=500&depth={depth}&sort=top", permalink.trim_end_matches('/'))).await?;
    let p = &v[0]["data"]["children"][0]["data"];
    let mut comments = Vec::new();
    fn walk(cs: &Value, depth: usize, max: usize, out: &mut Vec<(usize, i64, String)>) {
        for c in cs.as_array().into_iter().flatten().filter(|c| c["kind"] == "t1") {
            let d = &c["data"];
            if d["author"] != "AutoModerator" {
                out.push((depth, d["score"].as_i64().unwrap_or(0), d["body"].as_str().unwrap_or("").to_string()));
            }
            if depth + 1 < max {
                walk(&d["replies"]["data"]["children"], depth + 1, max, out);
            }
        }
    }
    walk(&v[1]["data"]["children"], 0, depth, &mut comments);
    Ok(Thread {
        id: p["id"].as_str().unwrap_or("").into(),
        title: p["title"].as_str().unwrap_or("").into(),
        sub: p["subreddit"].as_str().unwrap_or("").into(),
        score: p["score"].as_i64().unwrap_or(0),
        url: format!("https://www.reddit.com{}", p["permalink"].as_str().unwrap_or("")),
        text: p["selftext"].as_str().unwrap_or("").into(),
        comments,
    })
}

fn clean(t: &str, max: usize) -> String {
    static RE: std::sync::OnceLock<[regex::Regex; 4]> = std::sync::OnceLock::new();
    let [url, quote, md, ws] = RE.get_or_init(|| [
        regex::Regex::new(r"https?://\S+").unwrap(),
        regex::Regex::new(r"(?m)^>.*$").unwrap(),
        regex::Regex::new(r"[*_~`#]+").unwrap(),
        regex::Regex::new(r"\s+").unwrap(),
    ]);
    let t = url.replace_all(t, "<url>");
    let t = quote.replace_all(&t, "");
    let t = md.replace_all(&t, "");
    let t = ws.replace_all(&t, " ");
    let t = t.trim();
    if t.chars().count() > max { format!("{}…", t.chars().take(max).collect::<String>()) } else { t.to_string() }
}

fn noise(body: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^\s*\[(deleted|removed|gelöscht|entfernt)\]\s*$|I am a bot|^\s*this\.?\s*$|^\s*\+1\s*$").unwrap()).is_match(body)
}

/// The compact text the model reads: "T<n> r/sub (score↑) title — text" + indented top comments.
pub fn compact(threads: &[Thread], c: &Compact) -> String {
    threads.iter().take(c.threads).enumerate().map(|(i, t)| {
        let mut cs: Vec<&(usize, i64, String)> = t.comments.iter().filter(|x| x.0 < c.max_depth && x.1 >= c.min_score && !noise(&x.2)).collect();
        cs.sort_by_key(|x| -x.1);
        cs.truncate(c.comments_per_thread);
        let text = clean(&t.text, c.max_chars * 2);
        let mut s = format!("T{} r/{} ({}↑) {}{}", i + 1, t.sub, t.score, clean(&t.title, 200), if text.is_empty() { String::new() } else { format!(" — {text}") });
        for x in cs {
            s.push_str(&format!("\n{}- {} ({})", "  ".repeat(x.0), clean(&x.2, c.max_chars), x.1));
        }
        s
    }).collect::<Vec<_>>().join("\n\n")
}

const PLAN_SYS: &str = "You plan Reddit research. Reply ONLY with JSON {\"searches\": [{\"query\": \"3-6 words people would use in a post\", \"sub\": \"subreddit name without r/, or empty for all of Reddit\"}]} — 1 to 3 searches: the most fitting community first, then all of Reddit.";

const EXTRACT_SYS: &str = "You analyse Reddit threads for a user researching what people say (customer pains, opinions, idea validation). \
Use ONLY the material; Tn = thread number. Reply ONLY with JSON:\n\
{\"summary\": \"3-5 sentences: the overall picture, with [Tn]\", \
\"pains\": [{\"pain\": \"one sentence\", \"mentions\": n, \"threads\": [n], \"quote\": \"a short verbatim quote\"}], \
\"wishes\": [{\"wish\": \"what people want / would pay for\", \"threads\": [n]}], \
\"alternatives\": [{\"name\": \"tool / workaround people use\", \"sentiment\": \"good|mixed|bad\", \"threads\": [n]}], \
\"willingness_to_pay\": \"what the threads say about paying (prices named, or none)\", \
\"sentiment\": \"positive|mixed|negative\"}\n\
pains: distinct, merged, most mentioned first (up to 15). wishes and alternatives: up to 8 each. \
Quotes verbatim, at most 20 words, inner double quotes as '. Keep reasoning brief and write the JSON directly.";

/// Reddit deep research: plan searches → fetch the best threads → compact → one extraction.
/// Writes threads.md (what the model read), insights.json and summary.md into `out`.
pub async fn research(app: &crate::AppState, tab: &Tab, question: &str, out: &std::path::Path, c: &Compact, model: crate::llm::Provider) -> anyhow::Result<(Value, u64)> {
    let mut tokens = 0u64;
    let (plan, u) = crate::llm::complete_with(app, model, crate::llm::Tier::Light, PLAN_SYS, &format!("Research question: {question}"), true, 1500).await?;
    tokens += u.prompt + u.completion;
    let plan: Value = serde_json::from_str(&plan).unwrap_or_default();
    // Merge the searches' results (first search first), unique threads with discussion.
    let mut seen = std::collections::HashSet::new();
    let mut posts: Vec<Value> = Vec::new();
    for s in plan["searches"].as_array().into_iter().flatten().take(3) {
        let q = s["query"].as_str().unwrap_or(question);
        let sub = s["sub"].as_str().map(|x| x.trim().trim_start_matches("r/")).filter(|x| !x.is_empty());
        match search(tab, q, sub, 15).await {
            Ok(r) => {
                for p in r {
                    let id = p["id"].as_str().unwrap_or("").to_string();
                    if p["num_comments"].as_i64().unwrap_or(0) >= 3 && seen.insert(id) {
                        posts.push(p);
                    }
                }
            }
            Err(e) => tracing::info!("reddit search {q:?} in {sub:?}: {e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    if posts.is_empty() {
        bail!("Reddit had no discussions for that");
    }
    let mut threads = Vec::new();
    for p in posts.iter().take(c.threads) {
        if let Some(link) = p["permalink"].as_str() {
            match thread(tab, link, c.max_depth.max(1)).await {
                Ok(t) => threads.push(t),
                Err(e) => tracing::info!("reddit thread {link}: {e:#}"),
            }
            tokio::time::sleep(Duration::from_millis(800)).await;   // polite pace
        }
    }
    let text = compact(&threads, c);
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("threads.md"), &text)?;
    let (txt, u) = crate::llm::complete(app, model, EXTRACT_SYS, &format!("Question: {question}\n\n{text}"), true, 8000).await?;
    tokens += u.prompt + u.completion;
    let mut ins: Value = serde_json::from_str(&txt)?;
    ins["question"] = serde_json::json!(question);
    ins["threads"] = serde_json::json!(threads.iter().take(c.threads).enumerate().map(|(i, t)| serde_json::json!({"n": i + 1, "title": t.title, "sub": t.sub, "url": t.url, "score": t.score, "comments": t.comments.len()})).collect::<Vec<_>>());
    std::fs::write(out.join("insights.json"), serde_json::to_string_pretty(&ins)?)?;
    std::fs::write(out.join("summary.md"), summary_md(&ins))?;
    Ok((ins, tokens))
}

/// Readable markdown of the insights (overview fallback, voice summary source).
pub fn summary_md(i: &Value) -> String {
    let link = |n: &Value| -> String {
        let n = n.as_u64().unwrap_or(0);
        i["threads"].as_array().and_then(|ts| ts.iter().find(|t| t["n"].as_u64() == Some(n))).map(|t| format!("[T{n}]({})", t["url"].as_str().unwrap_or(""))).unwrap_or_else(|| format!("T{n}"))
    };
    let refs = |v: &Value| v.as_array().map(|a| a.iter().map(&link).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    let mut s = format!("# What Reddit says: {}\n\n{}\n", i["question"].as_str().unwrap_or(""), i["summary"].as_str().unwrap_or(""));
    s.push_str("\n## Pains\n");
    for p in i["pains"].as_array().into_iter().flatten() {
        s.push_str(&format!("- **{}** ({}×) {}\n  > {}\n", p["pain"].as_str().unwrap_or(""), p["mentions"].as_u64().unwrap_or(1), refs(&p["threads"]), p["quote"].as_str().unwrap_or("")));
    }
    if let Some(w) = i["wishes"].as_array().filter(|a| !a.is_empty()) {
        s.push_str("\n## What people want\n");
        for x in w {
            s.push_str(&format!("- {} {}\n", x["wish"].as_str().unwrap_or(""), refs(&x["threads"])));
        }
    }
    if let Some(a) = i["alternatives"].as_array().filter(|a| !a.is_empty()) {
        s.push_str("\n## What they use today\n");
        for x in a {
            s.push_str(&format!("- {} — {} {}\n", x["name"].as_str().unwrap_or(""), x["sentiment"].as_str().unwrap_or(""), refs(&x["threads"])));
        }
    }
    s.push_str(&format!("\n## Willingness to pay\n{}\n\n## Threads\n", i["willingness_to_pay"].as_str().unwrap_or("—")));
    for t in i["threads"].as_array().into_iter().flatten() {
        s.push_str(&format!("{}. [{}]({}) — r/{} · {}↑ · {} comments\n", t["n"], t["title"].as_str().unwrap_or(""), t["url"].as_str().unwrap_or(""), t["sub"].as_str().unwrap_or(""), t["score"], t["comments"]));
    }
    s
}

#[cfg(test)]
mod live {
    /// The production extraction on saved material (no Reddit traffic):
    /// `REDDIT_THREADS=<run>/threads.md cargo test --release --lib reddit_extract_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn reddit_extract_live() {
        let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
        let app = crate::AppState::from_env().await.unwrap();
        let text = std::fs::read_to_string(std::env::var("REDDIT_THREADS").unwrap()).unwrap();
        let t0 = std::time::Instant::now();
        let (txt, u) = crate::llm::complete(&app, app.cfg.provider, super::EXTRACT_SYS, &format!("Question: what do people complain about with Notion?\n\n{text}"), true, 8000).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        println!("{} pains, {} wishes, prompt {} completion {} in {:?}", v["pains"].as_array().map_or(0, |a| a.len()), v["wishes"].as_array().map_or(0, |a| a.len()), u.prompt, u.completion, t0.elapsed());
    }
}
