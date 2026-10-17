//! "Search and read the top N results" as a code-driven pipeline, not an LLM agent loop.
//!
//! LLM (MiniMax) is called exactly twice: plan (request → query + relevance criteria) and
//! synthesis (from capped top chunks of relevant pages). Navigation, consent, parsing,
//! fetching and relevance are code + Laya + embeddings, so token use doesn't depend on how
//! many pages or retries there were. Every call is recorded in the run's ledger.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::{cdp::Chrome, consent, extract, search};
use crate::{
    AppState,
    artifacts::{self, Catalog},
    judge::{self, Judge},
    llm::{self, Provider, Usage},
    mind,
};

#[derive(Debug, Clone)]
pub struct Options {
    /// Results to read (lab target: the first 10).
    pub n: usize,
    pub static_first: bool,
    pub static_concurrency: usize,
    pub render_concurrency: usize,
    /// "laya" | "embed" | "both"
    pub relevance: String,
    pub laya_threshold: f32,
    pub embed_threshold: f32,
    /// Max chars of page text sent to the synthesis LLM in total.
    pub synth_budget: usize,
    pub planner: Provider,
    /// Lab only: also write rejected pages (rejected/) so relevance recall can be evaluated.
    pub keep_rejected: bool,
    /// Model for the final brief (lab: Gemini Flash-Lite was more consistent and 6x faster).
    pub synthesizer: Provider,
    pub mind_url: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            n: 10,
            static_first: true,
            static_concurrency: 6,
            render_concurrency: 2,
            relevance: "embed".into(),
            laya_threshold: 0.5,
            embed_threshold: 0.55,
            synth_budget: 3_000,
            planner: Provider::MiniMax,
            keep_rejected: false,
            synthesizer: Provider::MiniMax,
            mind_url: "http://127.0.0.1:47802".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LlmCall {
    pub step: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub input_chars: usize,
    pub ms: u128,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Ledger {
    pub llm: Vec<LlmCall>,
    pub laya_calls: u32,
    pub embed_texts: usize,
    pub page_chars_to_llm: usize,
    pub raw_html_bytes_seen: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageResult {
    pub rank: usize,
    pub url: String,
    pub title: String,
    pub mode: String,
    pub chars: usize,
    pub embed_score: f32,
    pub laya_p: Option<f32>,
    pub relevant: bool,
    pub consent: Option<consent::Outcome>,
    pub error: Option<String>,
    pub ms: u128,
    /// Kept source file (relevant pages only), relative to the run dir.
    pub file: Option<String>,
    /// "relevant" | "irrelevant" | "failed"
    pub status: String,
    pub used_in_brief: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub request: String,
    pub query: String,
    pub criteria: String,
    pub search: search::SearchInfo,
    pub pages: Vec<PageResult>,
    pub relevant: usize,
    pub stored: usize,
    pub ledger: Ledger,
    pub total_tokens: u64,
    pub wall_ms: u128,
    pub dir: String,
}

/// "Find a bakery that fits", "gift ideas for Markus", "things to do on Saturday": the brief
/// must name concrete candidates and match them to the person's tastes (friend.rs attaches
/// them), not summarise articles. The lab (companion lab) showed the plain brief
/// answering "the sources do not name a specific bakery".
pub fn recommend_mode(request: &str) -> bool {
    let r = request.to_lowercase();
    ["find a ", "find two", "find three", "suggest ", "recommend", "worth trying", "gift idea", "things to do", "where to go", "where to eat", "best place", "a place for", "match it to their tastes"].iter().any(|k| r.contains(k))
        && !["how does", "why ", "what is the", "explain", "history of", "compare"].iter().any(|k| r.contains(k))
}

const PLAN_SYS_REC: &str = "You plan a web search for a recommendation (a place, a thing to do, a gift). Reply with ONLY a JSON object {\"query\": \"<google query, max 10 words: the kind of thing + the area + the one taste word that narrows it most>\", \"criteria\": \"<one sentence: a relevant page names specific candidates of that kind in that area with details>\"}.";
const SYNTH_SYS_REC: &str = "You recommend from numbered source excerpts for a particular person whose tastes are given in the request. Use ONLY candidates named in the excerpts, never outside knowledge. \
First list the concrete candidates you found (name, where, what they offer, hours or price if stated), each with the [n] of its excerpt. Then pick up to three that fit this person best and for each say in one line why it fits their tastes and what to watch out for given their dislikes. \
If the excerpts name no concrete candidates, say exactly that in one sentence and name what kind of page would. 150-280 words, plain text, no headings.";

const PLAN_SYS: &str = "You plan web research. Reply with ONLY a JSON object {\"query\": \"<google query, max 10 words>\", \"criteria\": \"<one sentence: what a relevant page must contain>\"}.";
const SYNTH_SYS: &str = "You write a concise research brief from numbered source excerpts. Use ONLY facts stated in the excerpts, never outside knowledge. \
Every sentence ends with the [n] of the excerpt that states it. If the excerpts do not cover part of the question, say so in one sentence. 120-220 words, plain text, no headings.";

pub async fn run(app: &AppState, chrome: &Chrome, request: &str, out: &Path, catalog: Option<&Catalog>, opt: &Options) -> anyhow::Result<Report> {
    let t0 = Instant::now();
    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let judge = app.judge.as_ref();
    std::fs::create_dir_all(out.join("sources"))?;
    let mut kept: Vec<(usize, String, String, String)> = Vec::new(); // relevant: rank, title, url, text

    // 1. Plan (one small LLM call).
    let (query, criteria) = {
        let t = Instant::now();
        let (txt, u) = llm::complete_with(app, opt.planner, crate::llm::Tier::Light, if recommend_mode(request) { PLAN_SYS_REC } else { PLAN_SYS }, request, true, 2000).await?;
        record(&ledger, "plan", u, request.len(), t);
        let v: Value = serde_json::from_str(&txt).unwrap_or_default();
        (v["query"].as_str().unwrap_or(request).to_string(), v["criteria"].as_str().unwrap_or(request).to_string())
    };

    // 2. Search in a real tab; DuckDuckGo static fallback on captcha / no results.
    let tab = chrome.new_tab().await?;
    let (mut hits, mut info) = match search::google(&tab, judge, &query, opt.n).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("google search failed: {e:#}");
            (Vec::new(), search::SearchInfo { engine: format!("google failed: {e}"), ..Default::default() })
        }
    };
    tab.close().await;
    if info.captcha || hits.len() < 3 {
        let http = extract::http_client();
        match search::duckduckgo(&http, &query, opt.n).await {
            Ok(h) => {
                info.engine = format!("{}→duckduckgo", if info.engine.is_empty() { "google" } else { &info.engine });
                hits = h;
                info.hits = hits.len();
            }
            Err(e) => tracing::warn!("duckduckgo failed: {e:#}"),
        }
    }
    if hits.len() < 3 {
        let tab = chrome.new_tab().await?;
        match search::bing(&tab, judge, &query, opt.n).await {
            Ok((h, c)) if !h.is_empty() => {
                info.engine.push_str("→bing");
                info.consent = c;
                hits = h;
                info.hits = hits.len();
            }
            Ok(_) => tracing::warn!("bing: no results"),
            Err(e) => tracing::warn!("bing failed: {e:#}"),
        }
        tab.close().await;
    }

    // 3. Read every hit: static first, render only when needed; bounded concurrency.
    let http = extract::http_client();
    let static_sem = Arc::new(Semaphore::new(opt.static_concurrency));
    let render_sem = Arc::new(Semaphore::new(opt.render_concurrency));
    let reads = hits.iter().map(|h| {
        let (http, static_sem, render_sem, ledger) = (http.clone(), static_sem.clone(), render_sem.clone(), ledger.clone());
        async move {
            let t = Instant::now();
            let mut why = None;
            if opt.static_first {
                let _p = static_sem.acquire().await;
                match extract::fetch_static(&http, &h.url).await {
                    Ok(p) => {
                        ledger.lock().unwrap().raw_html_bytes_seen += p.html_bytes;
                        return (h, Ok(p), None, t.elapsed().as_millis());
                    }
                    Err(e) => why = Some(e),
                }
            }
            let _p = render_sem.acquire().await;
            let r = render(chrome, judge, &h.url).await;
            let (page, consent) = match r {
                Ok((p, c)) => (Ok(p), c),
                Err(e) => (Err(format!("{}; render: {e:#}", why.unwrap_or_default())), None),
            };
            if let Some(c) = &consent {
                ledger.lock().unwrap().laya_calls += c.laya_calls;
            }
            (h, page, consent, t.elapsed().as_millis())
        }
    });
    let pages = futures_util::future::join_all(reads).await;

    // 4. Relevance: embeddings over chunks (best chunk), Laya on the best chunk.
    let mut results = Vec::new();
    let mut texts = Vec::new();
    let qv = mind::embed(&app.http, &opt.mind_url, &[criteria.clone()], "query").await.ok().and_then(|mut v| v.pop());
    for (h, page, consent, ms) in pages {
        let mut pr = PageResult { rank: h.rank, url: h.url.clone(), title: h.title.clone(), mode: "-".into(), chars: 0, embed_score: 0.0, laya_p: None, relevant: false, consent, error: None, ms, file: None, status: "failed".into(), used_in_brief: false };
        match page {
            Err(e) => pr.error = Some(e),
            Ok(p) => {
                pr.mode = p.mode.into();
                pr.chars = p.text.len();
                if !p.title.is_empty() {
                    pr.title = p.title.clone();
                }
                let chunks = extract::chunks(&p.text, 900);
                let chunks: Vec<String> = chunks.into_iter().take(40).collect();
                let mut best = String::new();
                let mut scored: Vec<(f32, String)> = Vec::new();
                if let (Some(qv), false) = (&qv, chunks.is_empty()) {
                    if let Ok(vs) = mind::embed(&app.http, &opt.mind_url, &chunks, "doc").await {
                        ledger.lock().unwrap().embed_texts += chunks.len();
                        scored = vs.iter().map(|v| mind::dot(qv, v)).zip(chunks.iter().cloned()).collect();
                        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                        pr.embed_score = scored[0].0;
                        best = scored[0].1.clone();
                    }
                }
                // No embedding model (mind sidecar down): rank chunks by keyword overlap with the
                // criteria instead of giving up on every page.
                let lexical = scored.is_empty() && !chunks.is_empty();
                if lexical {
                    let terms: Vec<String> = criteria.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() > 3).map(str::to_string).collect();
                    let score = |c: &str| { let l = c.to_lowercase(); if terms.is_empty() { 0.0 } else { terms.iter().filter(|t| l.contains(t.as_str())).count() as f32 / terms.len() as f32 } };
                    scored = chunks.iter().map(|c| (score(c), c.clone())).collect();
                    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                    best = scored[0].1.clone();
                    // Relevance on the whole page (criteria are sentences; one chunk covers few words).
                    pr.embed_score = score(&p.text);
                }
                if opt.relevance != "embed" {
                    if let Some(j) = judge {
                        let excerpt: String = best.chars().take(900).collect();
                        let st = json!({"question": criteria, "title": pr.title, "excerpt": excerpt});
                        if let Some(a) = j.ask(st, judge::questions(&["doc_relevant"]), 5_000).await {
                            ledger.lock().unwrap().laya_calls += 1;
                            pr.laya_p = judge::noul(&a, "doc_relevant");
                        }
                    }
                }
                let by_laya = pr.laya_p.map(|p| p >= opt.laya_threshold);
                // Keyword fallback: at least half of the criteria's words on the page.
                let by_emb = if lexical { pr.embed_score >= 0.5 } else { pr.embed_score >= opt.embed_threshold };
                pr.relevant = match opt.relevance.as_str() {
                    "embed" => by_emb,
                    "laya" => by_laya.unwrap_or(by_emb),
                    _ => by_laya.map(|l| l && pr.embed_score >= opt.embed_threshold - 0.1).unwrap_or(by_emb),
                };
                // 5. Keep only relevant pages as files; everything else is a line in checked.jsonl.
                pr.status = if pr.relevant { "relevant".into() } else { "irrelevant".into() };
                if pr.relevant {
                    pr.file = Some(format!("sources/{}", artifacts::source_name(h.rank, &pr.title, &pr.url)));
                    kept.push((h.rank, pr.title.clone(), pr.url.clone(), p.text.clone()));
                } else if opt.keep_rejected {
                    let f = format!("rejected/{}", artifacts::source_name(h.rank, &pr.title, &pr.url));
                    std::fs::create_dir_all(out.join("rejected"))?;
                    std::fs::write(out.join(&f), front(&pr, &p.text))?;
                    pr.file = Some(f);
                }
                if pr.relevant {
                    for (sc, ch) in scored.into_iter().take(2) {
                        texts.push((sc, h.rank, pr.title.clone(), pr.url.clone(), ch));
                    }
                }
            }
        }
        results.push(pr);
    }

    // 6. Synthesis: the globally best chunks (≤2 per relevant page) until the budget is used,
    //    so the same tokens carry more of the answer than an even split per page.
    texts.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut excerpts = String::new();
    let mut used = 0;
    let mut used_ranks = std::collections::BTreeSet::new();
    for (_, rank, title, url, chunk) in &texts {
        let chunk: String = chunk.chars().take(900).collect();
        if used + chunk.len() > opt.synth_budget && used > 0 {
            continue;
        }
        used += chunk.len();
        used_ranks.insert(*rank);
        excerpts.push_str(&format!("[{rank}] {title} ({url})\n{chunk}\n\n"));
    }
    for r in results.iter_mut() {
        r.used_in_brief = used_ranks.contains(&r.rank);
    }
    ledger.lock().unwrap().page_chars_to_llm += excerpts.len();
    let brief = if texts.is_empty() {
        "No relevant pages found.".to_string()
    } else {
        let t = Instant::now();
        let user = format!("Research question: {request}\nRelevance criteria: {criteria}\n\nSources:\n{excerpts}");
        let (txt, u) = llm::complete_with(app, opt.synthesizer, crate::llm::Tier::Heavy, if recommend_mode(request) { SYNTH_SYS_REC } else { SYNTH_SYS }, &user, false, 4000).await?;
        record(&ledger, "synthesize", u, user.len(), t);
        txt
    };
    // Kept sources (readable names) + brief with a source map.
    for (rank, title, url, text) in &kept {
        let pr = results.iter().find(|r| r.rank == *rank).unwrap();
        let path = out.join(pr.file.as_ref().unwrap());
        std::fs::write(&path, front(pr, text))?;
        if let Some(c) = catalog {
            let _ = c.add("source", title, Some(url), Some(request), &path);
        }
    }
    let mut src = String::new();
    for r in results.iter().filter(|r| r.relevant) {
        src.push_str(&format!("- [{}] {} — {}{} (`{}`)\n", r.rank, r.title, r.url, if r.used_in_brief { "" } else { " (kept, not cited)" }, r.file.as_deref().unwrap_or("")));
    }
    let brief_path = out.join("brief.md");
    std::fs::write(&brief_path, format!("# {request}\n\nSearch: `{query}` · {} of {} checked pages kept\n\n{brief}\n\n## Sources\n{src}", kept.len(), results.len()))?;
    if let Some(c) = catalog {
        let _ = c.add("brief", request, None, Some(&query), &brief_path);
    }
    let ledger = ledger.lock().unwrap().clone();
    let total_tokens = ledger.llm.iter().map(|c| c.prompt_tokens + c.completion_tokens).sum();
    let report = Report {
        request: request.into(),
        query,
        criteria,
        search: info,
        relevant: results.iter().filter(|r| r.relevant).count(),
        stored: results.iter().filter(|r| r.file.is_some()).count(),
        pages: results,
        ledger,
        total_tokens,
        wall_ms: t0.elapsed().as_millis(),
        dir: out.display().to_string(),
    };
    // Every checked URL, no content: what was looked at, why it was kept or dropped, what was used.
    let mut lines = String::new();
    for p in &report.pages {
        lines.push_str(&serde_json::to_string(&serde_json::json!({
            "rank": p.rank, "url": p.url, "domain": artifacts::domain_of(&p.url), "title": p.title, "status": p.status,
            "reason": p.error, "mode": p.mode, "chars": p.chars, "embed_score": p.embed_score, "laya_p": p.laya_p,
            "used_in_brief": p.used_in_brief, "file": p.file, "ms": p.ms}))?);
        lines.push('\n');
    }
    std::fs::write(out.join("checked.jsonl"), lines)?;
    std::fs::write(out.join("run.json"), serde_json::to_string_pretty(&report)?)?;
    Ok(report)
}

fn front(p: &PageResult, text: &str) -> String {
    format!("---\ntitle: {}\nurl: {}\nrank: {}\nmode: {}\nrelevance: {:.3}\n---\n\n{}\n", p.title, p.url, p.rank, p.mode, p.embed_score, text)
}

/// Sites that show nothing without an account. Skipped in seconds unless this screen's
/// profile holds a session for them (log in once with the screen visible; Bops golden profile).
/// (domain, cookie that only exists when logged in)
const GATED: &[(&str, &str)] = &[
    ("facebook.com", "c_user"), ("instagram.com", "sessionid"), ("quora.com", "m-b"), ("linkedin.com", "li_at"),
    ("x.com", "auth_token"), ("twitter.com", "auth_token"), ("tiktok.com", "sessionid"), ("pinterest.", "_pinterest_sess"), ("threads.net", "sessionid"),
];

/// Render in its own tab; prune inside the page; clear a consent wall only if text is short.
async fn render(chrome: &Chrome, judge: Option<&Judge>, url: &str) -> anyhow::Result<(extract::Page, Option<consent::Outcome>)> {
    let tab = chrome.new_tab().await?;
    let res = async {
        let _ = tab.block_heavy().await;
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        if let Some((_, session)) = GATED.iter().find(|(d, _)| host.contains(d)) {
            if !tab.has_cookie(url, session).await {
                anyhow::bail!("login wall: no session for {host} in this screen's profile (log in once to read it)");
            }
        }
        tab.goto(url, Duration::from_secs(20)).await?;
        let mut v = tab.eval(extract::PRUNE_JS).await?;
        let mut c = None;
        let text_of = |v: &Value| v["text"].as_str().unwrap_or("").to_string();
        let short = |v: &Value| v["text"].as_str().map(str::len).unwrap_or(0) < extract::MIN_STATIC_CHARS;
        // Bot challenges (Cloudflare "Just a moment") usually clear by themselves in a real
        // browser within a few seconds: poll instead of failing.
        let title_challenge = |v: &Value| extract::is_challenge(v["title"].as_str().unwrap_or("")) || v["title"].as_str().is_some_and(|t| t.contains("einen Moment") || t.contains("un instant"));
        for i in 0..10 {
            if !(title_challenge(&v) || extract::is_challenge(&text_of(&v))) {
                break;
            }
            if i == 3 {
                // Blocked fonts/images change the fingerprint some challenges check: lift and reload.
                let _ = tab.send("Network.setBlockedURLs", json!({"urls": []})).await;
                let _ = tab.send("Page.reload", json!({})).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            v = tab.eval(extract::PRUNE_JS).await?;
        }
        if short(&v) {
            // Laya page_state (trained, 100% on its test set) decides what is in the way.
            let state = match judge {
                Some(j) => {
                    let snap = format!("{}\n{}", v["title"].as_str().unwrap_or(""), text_of(&v).chars().take(600).collect::<String>());
                    j.ask(json!(snap), judge::questions(&["page_state"]), 3_000).await.and_then(|a| judge::choice(&a, "page_state"))
                }
                None => None,
            };
            match state.as_ref().map(|(s, p)| (s.as_str(), *p)) {
                Some(("login_wall", p)) if p >= 0.6 => anyhow::bail!("login wall (log in once on this screen's profile to read it)"),
                Some(("captcha", p)) if p >= 0.6 => anyhow::bail!("captcha / bot check"),
                Some(("error", p)) if p >= 0.6 => anyhow::bail!("error page"),
                _ => {
                    c = Some(consent::dismiss(&tab, judge).await);
                    tokio::time::sleep(Duration::from_millis(1200)).await; // lazy content after consent
                    v = tab.eval(extract::PRUNE_JS).await?;
                }
            }
        }
        let text = extract::clean_markdown(v["text"].as_str().unwrap_or(""));
        if title_challenge(&v) {
            anyhow::bail!("challenge not cleared");
        }
        if text.len() < 200 || extract::is_challenge(&text) {
            anyhow::bail!("rendered page has no readable content ({} chars)", text.len());
        }
        Ok((
            extract::Page { url: extract::clean_url(url), title: v["title"].as_str().unwrap_or("").to_string(), text: extract::cap(&text, extract::MAX_TEXT), mode: "rendered", html_bytes: 0 },
            c,
        ))
    }
    .await;
    tab.close().await;
    res
}

fn record(l: &Arc<Mutex<Ledger>>, step: &str, u: Usage, chars: usize, t: Instant) {
    l.lock().unwrap().llm.push(LlmCall { step: step.into(), prompt_tokens: u.prompt, completion_tokens: u.completion, input_chars: chars, ms: t.elapsed().as_millis() });
}


