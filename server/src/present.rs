//! Result presentation: turns a finished task's material (research brief + kept sources, lists,
//! documents) into one structured report the overview renders Perplexity-style — summary, key
//! facts, highlight cards with images, sections, budget, a map location, the forecast when the
//! dates are close, numbered sources — so nobody has to open the markdown files.
//!
//! Facts come only from the task's own material (the LLM restates, never adds). Images are
//! Wikipedia/Wikimedia thumbnails (no key), downloaded next to the report so the app shows them
//! through /api/file (artifacts only). Saved as data/artifacts/reports/<task>/report.json.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::{AppState, llm, llm::Provider, thinker::StepResult};

/// Tools whose results deserve a report (quick answers like weather keep their small card).
const PRESENTED: &[&str] = &["research", "reddit_research", "scrape", "details", "code", "papers_similar", "doc_review", "leads_find", "knowledge_ask", "events_find"];

pub fn wanted(steps: &[StepResult]) -> bool {
    steps.iter().any(|s| s.ok && PRESENTED.contains(&s.tool.as_str()))
}

const SYS: &str = "You turn a background task's results into a report for a results screen (like Perplexity). \
Use ONLY facts stated in the material; never add outside knowledge, never invent prices, dates or names. \
Reply ONLY with JSON:\n\
{\"title\": \"short title\", \"subtitle\": \"one line: scope, dates, place\", \
\"summary\": \"3-5 sentences answering the request, with [n] source numbers\", \
\"facts\": [{\"icon\": \"SF Symbol name\", \"label\": \"Best time\", \"value\": \"short value\"}], \
\"highlights\": [{\"title\": \"name of a place / product / paper / item\", \"text\": \"1-2 sentences why, with [n]\", \"tag\": \"category\", \"image_query\": \"Wikipedia article title for a picture, or empty\", \"source\": n}], \
\"sections\": [{\"heading\": \"…\", \"text\": \"short markdown: bullets welcome, [n] citations\"}], \
\"budget\": [{\"item\": \"…\", \"cost\": \"as stated in the material\"}], \
\"place\": {\"name\": \"main city/region if the request is about a place, else empty\", \"start\": \"YYYY-MM-DD or empty\", \"end\": \"YYYY-MM-DD or empty\"}}\n\
facts: 3-6 key numbers or answers (weather/climate, costs, opening hours, duration, best time, ratings…); icons like thermometer.medium, eurosign, \
clock, calendar, star, mappin.and.ellipse, figure.walk, tram, fork.knife, bed.double. highlights: 4-8 of the best concrete items. \
sections: 2-4. budget: only if the material states costs, else []. Keep it scannable; the user's language. \
For opinion research (Reddit insights): facts = threads read, comments, overall sentiment, the top pain, willingness to pay; \
highlights = the top pains and wishes (title = the pain, text = a short verbatim quote + how many mention it, tag = pain|wish|alternative, image_query empty); \
a section 'What they use today' for alternatives.";

/// Builds and saves the report. `dir` = the task's output folder (research run dir etc.).
pub async fn build(app: &AppState, task_id: &str, request: &str, steps: &[StepResult], model: Provider) -> anyhow::Result<Value> {
    // Leads are already structured and scored: shown as they are (no LLM, nothing restated).
    if let Some(f) = steps.iter().rev().find(|s| s.ok && s.tool == "leads_find").and_then(|s| s.file.clone()) {
        let ls: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&f)?)?;
        return Ok(leads_report(request, &ls));
    }
    // Knowledge answers: already written with citations; [doc id] → numbered sources.
    if let Some(f) = steps.iter().rev().find(|s| s.ok && s.tool == "knowledge_ask").and_then(|s| s.file.clone()) {
        let k: Value = serde_json::from_str(&std::fs::read_to_string(&f)?)?;
        return Ok(knowledge_report(request, &k));
    }
    let (material, sources) = gather(steps);
    let now = chrono::Local::now();
    let user = format!("Today: {}.\nRequest: {}\n\nMaterial:\n{material}\n\nSources:\n{}", now.format("%Y-%m-%d (%A)"),
        request.split("\n\nContext").next().unwrap_or(request),
        sources.iter().enumerate().map(|(i, s)| format!("[{}] {} — {}", i + 1, s["title"].as_str().unwrap_or(""), s["url"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n"));
    let (txt, _) = llm::complete(app, model, SYS, &user, true, 8000).await?;
    let mut r: Value = serde_json::from_str(&txt).context("report json")?;
    r["sources"] = json!(sources);
    r["request"] = json!(request.split("\n\nContext").next().unwrap_or(request));
    r["created"] = json!(now.to_rfc3339());
    let out = crate::thinker::data_dir().join("artifacts/reports").join(task_id);
    std::fs::create_dir_all(&out)?;
    let http = reqwest::Client::builder().user_agent("Sam/0.1 (local voice assistant; result previews)").timeout(Duration::from_secs(10)).build()?;
    // Pictures for the highlights (+ the place as hero), fetched in parallel.
    let mut queries: Vec<(Option<usize>, String)> = r["highlights"].as_array().into_iter().flatten().enumerate()
        .filter_map(|(i, h)| h["image_query"].as_str().filter(|q| !q.trim().is_empty()).map(|q| (Some(i), q.to_string()))).take(8).collect();
    if let Some(p) = r["place"]["name"].as_str().filter(|p| !p.trim().is_empty()) {
        queries.push((None, p.to_string()));
    }
    let fetched = futures_util::future::join_all(queries.iter().enumerate().map(|(k, (_, q))| image(&http, q, &out, k))).await;
    for ((slot, _), img) in queries.iter().zip(fetched) {
        let Some((file, credit)) = img else { continue };
        match slot {
            Some(i) => {
                r["highlights"][*i]["image"] = json!(file);
                r["highlights"][*i]["image_credit"] = json!(credit);
            }
            None => r["hero"] = json!({"image": file, "credit": credit}),
        }
    }
    // Map position and, when the trip is within the forecast range, the forecast for those days.
    if let Some(p) = r["place"]["name"].as_str().filter(|p| !p.trim().is_empty()).map(str::to_string) {
        if let Some((lat, lon, label)) = geocode(&http, &p).await {
            r["place"]["lat"] = json!(lat);
            r["place"]["lon"] = json!(lon);
            r["place"]["label"] = json!(label);
            let start = r["place"]["start"].as_str().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            if let Some(start) = start {
                let days_out = (start - now.date_naive()).num_days();
                if (0..=6).contains(&days_out) {
                    if let Ok(w) = crate::apps::weather(&app.http, &p, (days_out + 7).clamp(1, 7) as u32).await {
                        r["weather"] = w;
                    }
                }
            }
        }
    }
    std::fs::write(out.join("report.json"), serde_json::to_string_pretty(&r)?)?;
    Ok(r)
}

fn knowledge_report(request: &str, k: &Value) -> Value {
    let mut answer = k["answer"].as_str().unwrap_or("").to_string();
    let mut sources = Vec::new();
    for d in k["sources"].as_array().into_iter().flatten() {
        let id = d["id"].as_str().unwrap_or("");
        if id.is_empty() || !answer.contains(id) {
            continue;
        }
        let n = sources.len() + 1;
        answer = answer.replace(id, &n.to_string());
        let url = d["path"].as_str().filter(|p| !p.is_empty()).map(|p| format!("file://{p}")).unwrap_or_default();
        sources.push(json!({"n": n, "title": format!("{} — {}", d["title"].as_str().unwrap_or(""), d["date"].as_str().unwrap_or("")), "url": url, "domain": d["source"]}));
    }
    json!({"title": k["question"].as_str().unwrap_or(request).chars().take(90).collect::<String>(), "subtitle": format!("From your {} sources", sources.len()),
           "summary": answer, "facts": [], "highlights": [], "sections": [], "budget": [], "place": {"name": ""}, "sources": sources, "request": request, "created": chrono::Local::now().to_rfc3339()})
}

/// Ranked leads → report: the best as highlight cards (reason + angle), sources = profiles.
fn leads_report(request: &str, ls: &[Value]) -> Value {
    let good: Vec<&Value> = ls.iter().filter(|l| l["score"].as_i64().unwrap_or(0) >= 60).collect();
    let sources: Vec<Value> = ls.iter().take(40).enumerate().map(|(i, l)| json!({"n": i + 1, "title": format!("{} — {}", l["name"].as_str().unwrap_or(""), l["headline"].as_str().unwrap_or("")), "url": l["url"], "domain": "linkedin.com"})).collect();
    let highlights: Vec<Value> = ls.iter().take(12).enumerate().filter(|(_, l)| l["score"].as_i64().unwrap_or(0) >= 40).map(|(i, l)| {
        let mut text = l["reason"].as_str().unwrap_or("").to_string();
        if let Some(a) = l["angle"].as_str().filter(|a| !a.is_empty()) {
            text.push_str(&format!(" — Angle: {a}"));
        }
        json!({"title": format!("{} · {}", l["name"].as_str().unwrap_or(""), l["score"]), "text": text, "tag": l["headline"].as_str().unwrap_or("").chars().take(60).collect::<String>(), "source": i + 1})
    }).collect();
    let checked = ls.iter().filter(|l| l["enriched"] == true).count();
    json!({"title": "Leads", "subtitle": format!("LinkedIn · {} people ranked", ls.len()),
           "summary": format!("{} of {} people look like good fits (score ≥ 60); the top {checked} were checked on their full profile. Each has a reason and, where checked, an angle for a first message.", good.len(), ls.len()),
           "facts": [{"icon": "person.2", "label": "Ranked", "value": ls.len().to_string()}, {"icon": "star", "label": "Good fits", "value": good.len().to_string()}, {"icon": "checkmark.seal", "label": "Profiles checked", "value": checked.to_string()}],
           "highlights": highlights, "sections": [], "budget": [], "place": {"name": ""}, "sources": sources, "request": request, "created": chrono::Local::now().to_rfc3339()})
}

/// The task's material: step summaries, research briefs and their kept sources, list files.
fn gather(steps: &[StepResult]) -> (String, Vec<Value>) {
    let mut m = String::new();
    let mut sources: Vec<Value> = Vec::new();
    for s in steps.iter().filter(|s| s.ok) {
        m.push_str(&format!("## {} result\n{}\n\n", s.tool, s.summary.chars().take(2000).collect::<String>()));
        if s.tool == "research" {
            if let Some(dir) = s.dir.as_ref().map(PathBuf::from) {
                if let Ok(b) = std::fs::read_to_string(dir.join("brief.md")) {
                    m.push_str(&format!("### Brief\n{}\n\n", b.chars().take(4000).collect::<String>()));
                }
                // Kept sources in rank order; their numbers become the report's [n].
                let checked = std::fs::read_to_string(dir.join("checked.jsonl")).unwrap_or_default();
                for l in checked.lines() {
                    let Ok(c) = serde_json::from_str::<Value>(l) else { continue };
                    let Some(f) = c["file"].as_str() else { continue };
                    let text = std::fs::read_to_string(dir.join(f)).unwrap_or_default();
                    let n = sources.len() + 1;
                    sources.push(json!({"n": n, "title": c["title"], "url": c["url"], "domain": c["domain"]}));
                    if sources.len() <= 6 {
                        m.push_str(&format!("### Source [{n}] {}\n{}\n\n", c["title"].as_str().unwrap_or(""), text.chars().take(3000).collect::<String>()));
                    }
                }
            }
        } else if s.tool == "reddit_research" {
            // Insights + the threads as numbered sources (T n = source n).
            if let Some(dir) = s.dir.as_ref().map(PathBuf::from) {
                if let Ok(i) = std::fs::read_to_string(dir.join("insights.json")).map(|t| serde_json::from_str::<Value>(&t).unwrap_or_default()) {
                    for t in i["threads"].as_array().into_iter().flatten() {
                        sources.push(json!({"n": t["n"], "title": t["title"], "url": t["url"], "domain": format!("r/{}", t["sub"].as_str().unwrap_or(""))}));
                    }
                    m.push_str(&format!("### Reddit insights (Tn = source n)\n{}\n\n", serde_json::to_string(&i).unwrap_or_default().chars().take(14_000).collect::<String>()));
                }
            }
        } else if let Some(f) = s.file.as_ref().filter(|f| f.ends_with(".md") || f.ends_with(".csv") || f.ends_with(".json")) {
            if let Ok(t) = std::fs::read_to_string(f) {
                m.push_str(&format!("### {}\n{}\n\n", Path::new(f).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default(), t.chars().take(5000).collect::<String>()));
            }
        }
    }
    (m.chars().take(24_000).collect(), sources)
}

/// Wikipedia's lead image for `q` → saved file + credit line.
async fn image(http: &reqwest::Client, q: &str, out: &Path, k: usize) -> Option<(String, String)> {
    let api = url::Url::parse_with_params("https://en.wikipedia.org/w/api.php", &[
        ("action", "query"), ("format", "json"), ("generator", "search"), ("gsrsearch", q), ("gsrlimit", "1"),
        ("prop", "pageimages|info"), ("inprop", "url"), ("piprop", "thumbnail"), ("pithumbsize", "900"),
    ]).ok()?;
    let v: Value = http.get(api).send().await.ok()?.json().await.ok()?;
    let page = v["query"]["pages"].as_object()?.values().next()?.clone();
    let src = page["thumbnail"]["source"].as_str()?;
    let bytes = http.get(src).send().await.ok()?.bytes().await.ok()?;
    if bytes.len() < 2000 || bytes.len() > 4 << 20 {
        return None;
    }
    let ext = if src.to_lowercase().ends_with(".png") { "png" } else { "jpg" };
    let file = out.join(format!("img_{k}.{ext}"));
    std::fs::write(&file, &bytes).ok()?;
    Some((file.display().to_string(), format!("Wikipedia: {}", page["title"].as_str().unwrap_or(q))))
}

async fn geocode(http: &reqwest::Client, place: &str) -> Option<(f64, f64, String)> {
    let u = url::Url::parse_with_params("https://geocoding-api.open-meteo.com/v1/search", &[("name", place.split(',').next().unwrap_or(place).trim()), ("count", "1"), ("language", "en")]).ok()?;
    let g: Value = http.get(u).send().await.ok()?.json().await.ok()?;
    let l = g["results"].get(0)?;
    Some((l["latitude"].as_f64()?, l["longitude"].as_f64()?, format!("{}, {}", l["name"].as_str().unwrap_or(place), l["country"].as_str().unwrap_or(""))))
}

#[cfg(test)]
mod live {
    /// `PRESENT_DIR=<research run dir> cargo test --release --lib present_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn present_live() {
        crate::config::load_dev_env();
        let app = crate::AppState::from_env().await.unwrap();
        let dir = std::env::var("PRESENT_DIR").unwrap();
        let step = crate::thinker::StepResult { status: String::new(), tool: "research".into(), ok: true, summary: "research done".into(), dir: Some(dir), file: None, tokens: 0 };
        let r = super::build(&app, "test-report", "I'm planning a trip to Shanghai, China from 28th December to the 10th January. Best places to visit and what to do there?", &[step], app.cfg.provider).await.unwrap();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}
