//! social_research on X: what people say about a topic (social-research lab L5).
//!
//! 1. ONE planning call: the stance target and 4-5 X searches (no stance words as search terms,
//!    no generic OR-words: those pulled 31 % unrelated posts in the lab), plus the bare topic in
//!    Top and Latest;
//! 2. the searches in Sam's signed-in X session, reading X's own GraphQL (SearchTimeline) —
//!    paced and under the X budget (X answered 429 after ~52 searches in 30 min); a 429 trips
//!    the 24 h pause;
//! 3. near-duplicates (copy-paste campaigns) collapsed, retweets dropped;
//! 4. stance per post in batches (prompt v3: explicit conventions; F1 0.77-0.81 in the lab);
//! 5. ONE synthesis call over every relevant post (coverage 0.94 vs 0.85 for top-40); the split
//!    and the sample size come from code, never from the model.

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::{AppState, browser::cdp::{Chrome, Visibility}, llm::{self, Tier}};

#[derive(Debug, Clone)]
pub struct Post {
    pub id: String,
    pub user: String,
    pub text: String,
    pub likes: i64,
    pub reposts: i64,
    pub replies: i64,
    pub followers: i64,
    pub is_reply: bool,
    pub stance: String,
}

fn jtext(txt: &str) -> Option<Value> {
    let i = txt.find('{')?;
    serde_json::from_str(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok()
}

/// Tweets in a GraphQL response (X nests them deep; walk for `__typename: Tweet`).
fn tweets(v: &Value, out: &mut Vec<Post>) {
    match v {
        Value::Object(o) => {
            if o.get("__typename").and_then(|t| t.as_str()) == Some("Tweet") && o.contains_key("legacy") && o.contains_key("core") {
                let lg = &v["legacy"];
                if lg.get("retweeted_status_result").is_some() {
                    return;
                }
                let u = &v["core"]["user_results"]["result"];
                let text = v["note_tweet"]["note_tweet_results"]["result"]["text"].as_str().or(lg["full_text"].as_str()).unwrap_or("").to_string();
                out.push(Post {
                    id: lg["id_str"].as_str().or(v["rest_id"].as_str()).unwrap_or("").to_string(),
                    user: u["core"]["screen_name"].as_str().or(u["legacy"]["screen_name"].as_str()).unwrap_or("").to_string(),
                    text,
                    likes: lg["favorite_count"].as_i64().unwrap_or(0),
                    reposts: lg["retweet_count"].as_i64().unwrap_or(0),
                    replies: lg["reply_count"].as_i64().unwrap_or(0),
                    followers: u["legacy"]["followers_count"].as_i64().unwrap_or(0),
                    is_reply: lg["in_reply_to_status_id_str"].is_string(),
                    stance: String::new(),
                });
                return;
            }
            for x in o.values() {
                tweets(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| tweets(x, out)),
        _ => {}
    }
}

const PLAN_SYS: &str = "You plan searches on X (Twitter) to find out what people say about a topic. Reply ONLY with JSON \
{\"target\": \"<what stance is measured toward, e.g. 'Donald Trump', 'die Mietpreisbremse', 'Deutsche Bahn'>\", \"topic\": \"<the bare topic as an X query, with lang:xx if the audience language is clear>\", \
\"queries\": [{\"q\": \"…\", \"mode\": \"top|live\"}]} with 3-4 more queries for the main current angles / news hooks. Rules: never put stance words (supporters, critics, gut, schlecht) \
or generic words alone in OR-groups (Kritik OR Störung OR Preise matches everything); every query must contain the topic itself; use min_faves:N on at most one query.";

const STANCE_SYS: &str = "You classify posts from X (Twitter) by the AUTHOR's own stance toward a target. For each numbered post answer one of: \
pro (supportive/positive), contra (critical/negative/complaint), neutral (news, facts, questions, or mixed), unrelated (not about the target). Rules: \
a bare quote or clip of the target with no commentary is neutral, unless the account is clearly a fan/official account quoting approvingly (pro); \
a reply attacking the target's critics is pro, a reply attacking the target's defenders is contra; sarcasm counts by what it means; \
a news headline is neutral unless it editorialises; a post that only uses the word for something else is unrelated. \
Reply ONLY with JSON {\"s\": [{\"n\": 1, \"s\": \"pro|contra|neutral|unrelated\"}, ...]} covering every post.";

const SYNTH_SYS: &str = "You write a short, fair brief of what people say on X about a topic, from classified posts. \
Write 4-7 themes; for each: a bold name, one or two sentences on what is said and by which side, and 1-2 short quotes with their [n] reference. \
Use ONLY the posts given; do not add outside facts. Do not state percentages: the stance split is computed separately. Plain markdown, no preamble. Write in the language of the request.";

pub struct Report {
    pub markdown: String,
    pub summary: String,
    pub data: Value,
    pub tokens: u64,
}

fn eng(p: &Post) -> i64 {
    p.likes + 2 * p.reposts + p.replies
}

pub async fn research_x(app: &AppState, question: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<Report> {
    crate::browser::sessions::check("x")?;
    let today = chrono::Local::now().format("%Y-%m-%d");
    let (txt, u) = llm::complete_tier(app, Tier::Light, PLAN_SYS, &format!("Request: {question}\nToday: {today}"), true, 1500).await?;
    let tokens = u.prompt + u.completion;
    let plan = jtext(&txt).context("search plan")?;
    let target = plan["target"].as_str().unwrap_or(question).to_string();
    let topic = plan["topic"].as_str().unwrap_or(question).to_string();
    let mut searches: Vec<(String, String)> = vec![(topic.clone(), "top".into()), (topic.clone(), "live".into())];
    for q in plan["queries"].as_array().into_iter().flatten().take(4) {
        if let Some(s) = q["q"].as_str() {
            searches.push((s.to_string(), if q["mode"] == "live" { "live".into() } else { "top".into() }));
        }
    }

    let data = crate::thinker::data_dir();
    let screen = crate::thinker::accounts_screen();
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let collected = async {
        crate::browser::sessions::ready(&tab, "x", on_user).await?;
        let mut posts: Vec<Post> = vec![];
        let mut seen = std::collections::HashSet::new();
        let mut requests = 0;
        for (i, (q, mode)) in searches.iter().enumerate() {
            if i > 0 {
                // Spread a run over minutes, not seconds.
                tokio::time::sleep(Duration::from_millis(8000 + (chrono::Local::now().timestamp_millis() % 7000) as u64)).await;
            }
            let mut url = format!("https://x.com/search?q={}&src=typed_query", url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>());
            if mode == "live" {
                url.push_str("&f=live");
            }
            for step in 0..2 {
                if let Err(e) = crate::browser::sessions::take("x", 1) {
                    if posts.is_empty() {
                        return Err(e);
                    }
                    tracing::info!("x budget: {e:#}");
                    return Ok((posts, requests));
                }
                requests += 1;
                let action = if step == 0 { format!("navigate:{url}") } else { "scroll".into() };
                let rs = tab.responses(&action, "SearchTimeline", Duration::from_secs(if step == 0 { 9 } else { 6 })).await?;
                for (status, body) in rs {
                    if status == 429 {
                        crate::browser::sessions::trip("x", "HTTP 429 on search");
                        if posts.is_empty() {
                            bail!("X is rate-limiting Sam's account right now; Sam pauses X for a day");
                        }
                        return Ok((posts, requests));
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(&body) {
                        let mut got = vec![];
                        tweets(&v, &mut got);
                        posts.extend(got.into_iter().filter(|p| !p.id.is_empty() && seen.insert(p.id.clone())));
                    }
                }
                if step == 0 && tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(|h| h.contains("/login"))).unwrap_or(false) {
                    bail!("X isn't signed in: Connectors › X › Link");
                }
            }
        }
        Ok::<_, anyhow::Error>((posts, requests))
    }
    .await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    let (posts, requests) = collected?;
    if posts.is_empty() {
        bail!("X returned no posts for these searches");
    }
    let mut r = analyse(app, question, &target, posts, requests, "X", "https://x.com/{user}/status/{id}").await?;
    r.tokens += tokens;
    r.data["searches"] = json!(searches);
    Ok(r)
}

/// Shared by every site: copy-paste collapse, stance in batches (prompt v3), one synthesis over
/// every relevant post; the split and sample size come from code. `link` has {user} and {id}.
pub async fn analyse(app: &AppState, question: &str, target: &str, mut posts: Vec<Post>, requests: usize, site: &str, link: &str) -> anyhow::Result<Report> {
    let mut tokens = 0u64;
    // Copy-paste campaigns: identical text (normalised) counts once.
    let mut texts = std::collections::HashSet::new();
    posts.retain(|p| texts.insert(p.text.to_lowercase().split_whitespace().filter(|w| !w.starts_with("http") && !w.starts_with('@')).collect::<Vec<_>>().join(" ")));
    posts.truncate(400);

    // Stance, in batches.
    for chunk in posts.chunks_mut(40) {
        let lines: Vec<String> = chunk.iter().enumerate().map(|(k, p)| format!("{}. @{}: {}", k + 1, p.user, p.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(280).collect::<String>())).collect();
        let (txt, u) = llm::complete_tier(app, Tier::Light, STANCE_SYS, &format!("Target: {target}\n\nPosts:\n{}", lines.join("\n")), true, 5000).await?;
        tokens += u.prompt + u.completion;
        let v: std::collections::HashMap<usize, String> = jtext(&txt).and_then(|j| j["s"].as_array().map(|a| a.iter().filter_map(|x| Some((x["n"].as_u64()? as usize, x["s"].as_str()?.to_string()))).collect())).unwrap_or_default();
        for (k, p) in chunk.iter_mut().enumerate() {
            p.stance = v.get(&(k + 1)).cloned().unwrap_or_else(|| "neutral".into());
        }
    }
    let rel: Vec<&Post> = posts.iter().filter(|p| p.stance != "unrelated").collect();
    if rel.is_empty() {
        bail!("none of the {} posts found is about {target}", posts.len());
    }
    let share = |c: &str| rel.iter().filter(|p| p.stance == c).count() * 100 / rel.len();

    // Synthesis over every relevant post.
    let lines: Vec<String> = rel.iter().enumerate().map(|(i, p)| format!("[{}] ({}, {} eng.) @{}: {}", i + 1, p.stance, eng(p), p.user, p.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(220).collect::<String>())).collect();
    let (body, u) = llm::complete_tier(app, Tier::Light, SYNTH_SYS, &format!("Topic / request: {question}\n\nPosts:\n{}", lines.join("\n")), false, 3000).await?;
    tokens += u.prompt + u.completion;
    let head = format!("**Sample:** {} posts read on {site} ({requests} requests), {} about {target}. **Stance among those:** pro {} %, contra {} %, neutral {} %.\n\n",
        posts.len(), rel.len(), share("pro"), share("contra"), share("neutral"));
    let sources: Vec<String> = rel.iter().enumerate().map(|(i, p)| format!("[{}] {}", i + 1, link.replace("{user}", &p.user).replace("{id}", &p.id))).collect();
    let caveat = format!("\n\n*{site} users are not representative of the public; bots and coordinated accounts exist; this is a sample of recent and top posts, not a census.*");
    let markdown = format!("# {question}\n\n{head}{}{caveat}\n\n## Sources\n\n{}\n", body.trim(), sources.join("\n"));
    let summary = format!("{}{}", head.replace("**", ""), body.trim().chars().take(1500).collect::<String>());
    let data = json!({"question": question, "target": target, "site": site, "requests": requests, "posts": posts.len(), "related": rel.len(),
        "split": {"pro": share("pro"), "contra": share("contra"), "neutral": share("neutral")},
        "items": rel.iter().map(|p| json!({"id": p.id, "user": p.user, "text": p.text, "stance": p.stance, "likes": p.likes, "reposts": p.reposts, "is_reply": p.is_reply})).collect::<Vec<_>>()});
    Ok(Report { markdown, summary, data, tokens })
}

#[cfg(test)]
mod tests {
    #[test]
    fn walks_graphql() {
        let v = serde_json::json!({"data": {"x": [{"result": {"__typename": "Tweet", "rest_id": "1", "core": {"user_results": {"result": {"core": {"screen_name": "a"}, "legacy": {"followers_count": 5}}}},
            "legacy": {"id_str": "1", "full_text": "hello", "favorite_count": 3, "retweet_count": 1, "reply_count": 0}}}]}});
        let mut out = vec![];
        super::tweets(&v, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].user, "a");
        assert_eq!(out[0].likes, 3);
    }
}
