//! Web search: Google in a real tab (DOM-parsed, no LLM), DuckDuckGo static HTML fallback.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::{cdp::Tab, consent, extract};
use crate::judge::Judge;

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub rank: usize,
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SearchInfo {
    pub engine: String,
    pub consent: consent::Outcome,
    pub captcha: bool,
    pub hits: usize,
}

/// Organic results only: `a` elements containing an `h3`, minus Google's own links.
const SERP_JS: &str = r#"(() => {
  if (location.pathname.startsWith('/sorry') || document.querySelector('form#captcha-form, #recaptcha')) return {captcha: true, hits: []};
  const seen = new Set(), hits = [];
  for (const a of document.querySelectorAll('a:has(h3)')) {
    let u = a.href; if (!u || !u.startsWith('http')) continue;
    try { const x = new URL(u); if (/(^|\.)google\./.test(x.host) && !/^\/(goto|url)$/.test(x.pathname)) continue; } catch (_) { continue; }
    if (seen.has(u)) continue; seen.add(u);
    const box = a.closest('div.g, div[data-hveid], div.MjjYud') || a.parentElement;
    const title = a.querySelector('h3').innerText.trim();
    let snip = ''; const s = box && box.querySelector('[data-sncf], .VwiC3b, [style*="-webkit-line-clamp"]');
    snip = (s ? s.innerText : (box ? box.innerText : '')).replace(title, '').replace(/\s+/g, ' ').trim().slice(0, 300);
    hits.push({title, url: u, snippet: snip});
  }
  return {captcha: false, hits};
})()"#;

pub async fn google(tab: &Tab, judge: Option<&Judge>, query: &str, n: usize) -> anyhow::Result<(Vec<Hit>, SearchInfo)> {
    let _ = tab.send("Network.enable", serde_json::json!({})).await;
    let _ = tab.set_cookies(consent::seed_cookies()).await;
    let url = format!("https://www.google.com/search?q={}&hl=en&num={}&pws=0", urlencode(query), n.max(10) + 2);
    tab.goto(&url, Duration::from_secs(20)).await?;
    let mut info = SearchInfo { engine: "google".into(), ..Default::default() };
    info.consent = consent::dismiss(tab, judge).await;
    let mut serp: Value = tab.eval(SERP_JS).await?;
    if serp["hits"].as_array().is_none_or(|h| h.is_empty()) && !serp["captcha"].as_bool().unwrap_or(false) {
        // Consent click may have navigated back to results; read once more.
        tokio::time::sleep(Duration::from_millis(800)).await;
        serp = tab.eval(SERP_JS).await?;
    }
    info.captcha = serp["captcha"].as_bool().unwrap_or(false);
    if serp["hits"].as_array().is_none_or(|h| h.is_empty()) {
        let dbg = tab.eval("({url: location.href, title: document.title, ah3: document.querySelectorAll('a:has(h3)').length, body: (document.body ? document.body.innerText : '').slice(0, 200)})").await;
        tracing::warn!("google: no hits parsed; page = {dbg:?}; serp = {serp}");
    }
    // Google wraps organic links in /goto?url=<opaque> (HTTP 302) or /url?q=<target>.
    let mut raw: Vec<Value> = serp["hits"].as_array().cloned().unwrap_or_default();
    // A results page often has fewer than 10 organic hits: read the next page(s) until n.
    let mut start = 10;
    while raw.len() < n + 2 && start <= 20 && !info.captcha && !raw.is_empty() {
        tab.goto(&format!("{url}&start={start}"), Duration::from_secs(20)).await?;
        let more: Value = tab.eval(SERP_JS).await?;
        let more = more["hits"].as_array().cloned().unwrap_or_default();
        if more.is_empty() {
            break;
        }
        let have: std::collections::HashSet<String> = raw.iter().filter_map(|h| h["url"].as_str().map(str::to_string)).collect();
        raw.extend(more.into_iter().filter(|h| !h["url"].as_str().is_some_and(|u| have.contains(u))));
        start += 10;
    }
    let resolved = futures_util::future::join_all(raw.iter().map(|h| resolve(h["url"].as_str().unwrap_or("")))).await;
    let mut seen = std::collections::HashSet::new();
    let hits: Vec<Hit> = raw
        .iter()
        .zip(resolved)
        .filter_map(|(h, u)| u.map(|u| (h, extract::clean_url(&u))))
        .filter(|(_, u)| seen.insert(u.clone()))
        .take(n)
        .enumerate()
        .map(|(i, (h, url))| Hit { rank: i + 1, title: h["title"].as_str().unwrap_or("").to_string(), url, snippet: h["snippet"].as_str().unwrap_or("").to_string() })
        .collect();
    info.hits = hits.len();
    Ok((hits, info))
}

/// Resolve Google redirect links to the target URL without fetching the target.
async fn resolve(u: &str) -> Option<String> {
    let url = url::Url::parse(u).ok()?;
    if !url.host_str()?.contains("google.") {
        return Some(u.to_string());
    }
    if url.path() == "/url" {
        return url.query_pairs().find(|(k, _)| k == "q" || k == "url").map(|(_, v)| v.to_string());
    }
    static NOREDIR: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let c = NOREDIR.get_or_init(|| {
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(6)).user_agent(super::cdp::UA).build().unwrap()
    });
    let r = c.get(u).send().await.ok()?;
    r.headers().get("location")?.to_str().ok().filter(|l| l.starts_with("http")).map(str::to_string)
}

/// Bing in a real tab (second fallback). Result links are bing.com/ck/a?…&u=a1<base64url(target)>.
const BING_JS: &str = r#"(() => [...document.querySelectorAll('li.b_algo')].map(li => { const a = li.querySelector('h2 a'); const p = li.querySelector('.b_caption p, p');
  return a ? {title: a.innerText.trim(), url: a.href, snippet: p ? p.innerText.replace(/\s+/g, ' ').trim().slice(0, 300) : ''} : null; }).filter(Boolean))()"#;

pub async fn bing(tab: &Tab, judge: Option<&Judge>, query: &str, n: usize) -> anyhow::Result<(Vec<Hit>, consent::Outcome)> {
    tab.goto(&format!("https://www.bing.com/search?q={}&setlang=en&count={}", urlencode(query), n + 2), Duration::from_secs(20)).await?;
    let c = consent::dismiss(tab, judge).await;
    let v = tab.eval(BING_JS).await?;
    let mut seen = std::collections::HashSet::new();
    let hits = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let u = h["url"].as_str()?;
            let target = url::Url::parse(u)
                .ok()
                .filter(|x| x.host_str().is_some_and(|h| h.contains("bing.com")))
                .and_then(|x| x.query_pairs().find(|(k, _)| k == "u").map(|(_, v)| v.to_string()))
                .and_then(|b| {
                    use base64::Engine;
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b.trim_start_matches("a1").trim_end_matches('=')).ok()
                })
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_else(|| u.to_string());
            Some((h, extract::clean_url(&target)))
        })
        .filter(|(_, u)| u.starts_with("http") && seen.insert(u.clone()))
        .take(n)
        .enumerate()
        .map(|(i, (h, url))| Hit { rank: i + 1, title: h["title"].as_str().unwrap_or("").to_string(), url, snippet: h["snippet"].as_str().unwrap_or("").to_string() })
        .collect();
    Ok((hits, c))
}

/// No-JS fallback (captcha or Google unavailable): html.duckduckgo.com, parsed with scraper.
pub async fn duckduckgo(http: &reqwest::Client, query: &str, n: usize) -> anyhow::Result<Vec<Hit>> {
    let r = http
        .post("https://html.duckduckgo.com/html/")
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await?;
    let status = r.status();
    let html = r.text().await?;
    if !html.contains("result__a") {
        tracing::warn!("duckduckgo: status {status}, {} bytes, no results markup", html.len());
    }
    let doc = scraper::Html::parse_document(&html);
    let res = scraper::Selector::parse(".result:not(.result--ad)").unwrap();
    let a = scraper::Selector::parse("a.result__a").unwrap();
    let sn = scraper::Selector::parse(".result__snippet").unwrap();
    let mut out = Vec::new();
    for r in doc.select(&res) {
        let Some(link) = r.select(&a).next() else { continue };
        let href = link.value().attr("href").unwrap_or("");
        // DDG wraps links: //duckduckgo.com/l/?uddg=<encoded>
        let url = url::Url::parse(&format!("https:{href}"))
            .ok()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.to_string()))
            .unwrap_or_else(|| href.to_string());
        if !url.starts_with("http") {
            continue;
        }
        out.push(Hit {
            rank: out.len() + 1,
            title: link.text().collect::<String>().trim().to_string(),
            url: extract::clean_url(&url),
            snippet: r.select(&sn).next().map(|s| s.text().collect::<String>().trim().to_string()).unwrap_or_default(),
        });
        if out.len() >= n {
            break;
        }
    }
    Ok(out)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
