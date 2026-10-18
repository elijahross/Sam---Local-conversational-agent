//! market_analysis: what a used item really sells for (market-analysis lab L3).
//!
//! 1. eBay sold + completed listings (2 pages, newest first) in Sam's signed-in eBay session
//!    (sold prices need a login since 2026);
//! 2. ONE judge pass (light model, batches of 40): which listings are one unit of exactly the
//!    item, complete and working — not accessories, other generations, defects, bundles, lots;
//! 3. log-IQR trim (single-bid oddities), then median and p25-p75 of real sales;
//! 4. too few matching sales (< 8) → say so instead of a number (lab: eBay's "brompton" sold
//!    results held 120 parts and souvenirs and not one bike);
//! 5. asking prices (willhaben for AT, Kleinanzeigen for DE) through the same judge, labelled
//!    as asking prices, not sales.
//! Lab result: 3.6 % mean median error over 11 items, 10/11 within 10 %.

use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::{AppState, browser::cdp::{Chrome, Visibility}, monitors::price_num};

const MIN_SALES: usize = 8;

const SOLD_JS: &str = r#"
(() => [...document.querySelectorAll('li.s-card[data-listingid], li.s-item[data-listingid]')].map(li => {
  const T = s => (s || '').replace(/\s+/g, ' ').trim();
  const t = li.querySelector('.s-card__title, .s-item__title');
  const title = T(t && t.innerText).replace(/^(Neues Angebot|Neu eingestellt)\s*/i, '').replace(/Wird in neuem Fenster oder Tab geöffnet/ig, '').trim();
  const txt = T(li.innerText);
  const a = li.querySelector('a[href*="/itm/"]');
  return {id: li.dataset.listingid, title, price: T((li.querySelector('.s-card__price, .s-item__price') || {}).innerText),
          sold: (txt.match(/Verkauft\s+\d{1,2}\.?\s*\w+\.?\s*\d{4}/) || [''])[0].replace(/^Verkauft\s+/, ''),
          condition: (txt.match(/\b(Neu|Gebraucht|Neuwertig|Sehr gut|Gut|Akzeptabel|Generalüberholt|Als Ersatzteil[^|]*|Defekt[^|]*|Brandneu|Wie neu)\b/i) || [''])[0],
          url: a ? a.href.split('?')[0] : ''};
}).filter(x => x.title && x.id !== '123456' && !/^Shop on eBay$/i.test(x.title)))()
"#;

const JUDGE_SYS: &str = "You prepare a market-price estimate from marketplace listings. For EVERY numbered listing first name what is offered (t: 1-5 words), \
then decide m = true only if it is ONE unit of exactly the item asked for (same model / generation / edition, the standard version), complete and working. \
m = false for accessories or parts alone, other models or generations, defective / for-parts items, bundles with significant extras, lots of several units. \
Used items count. Reply ONLY with JSON {\"v\": [{\"n\": 1, \"t\": \"…\", \"m\": true|false}, ...]} covering every listing.";

struct Offer {
    title: String,
    price: f64,
    date: String,
    url: String,
}

async fn judge(app: &AppState, item: &str, rows: &[(String, String, String)]) -> (Vec<bool>, u64) {
    let mut out = vec![];
    let mut tokens = 0;
    for chunk in rows.chunks(40) {
        let lines: Vec<String> = chunk.iter().enumerate().map(|(k, (t, c, p))| format!("{}. {} | {c} | {p}", k + 1, t.chars().take(120).collect::<String>())).collect();
        let user = format!("Item asked for: {item}\n\nListings:\n{}", lines.join("\n"));
        let v: Option<std::collections::HashMap<usize, bool>> = match crate::llm::complete_tier(app, crate::llm::Tier::Light, JUDGE_SYS, &user, true, 5000).await {
            Ok((txt, u)) => {
                tokens += u.prompt + u.completion;
                txt.find('{').and_then(|i| serde_json::from_str::<Value>(&txt[i..txt.rfind('}').map(|e| e + 1).unwrap_or(txt.len())]).ok())
                    .and_then(|j| j["v"].as_array().map(|vs| vs.iter().filter_map(|x| Some((x["n"].as_u64()? as usize, x["m"].as_bool().unwrap_or(false)))).collect()))
            }
            Err(e) => {
                tracing::warn!("market judge unavailable: {e:#}");
                None
            }
        };
        // No judge: nothing is kept (a raw median can be off by 40 %; better to say so).
        out.extend((1..=chunk.len()).map(|k| v.as_ref().and_then(|v| v.get(&k).copied()).unwrap_or(false)));
    }
    (out, tokens)
}

fn quantile(sorted: &[f64], p: f64) -> f64 {
    let k = (sorted.len() - 1) as f64 * p;
    let (lo, hi) = (k.floor() as usize, k.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo as f64)
}

/// Tukey fences on log prices.
fn trim(mut xs: Vec<Offer>) -> Vec<Offer> {
    if xs.len() < 4 {
        return xs;
    }
    let mut lg: Vec<f64> = xs.iter().map(|o| o.price.ln()).collect();
    lg.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (a, b) = (quantile(&lg, 0.25), quantile(&lg, 0.75));
    let (lo, hi) = (a - 1.5 * (b - a), b + 1.5 * (b - a));
    xs.retain(|o| (lo..=hi).contains(&o.price.ln()));
    xs
}

fn stats(xs: &[Offer]) -> Option<(f64, f64, f64)> {
    if xs.is_empty() {
        return None;
    }
    let mut p: Vec<f64> = xs.iter().map(|o| o.price).collect();
    p.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some((quantile(&p, 0.5), quantile(&p, 0.25), quantile(&p, 0.75)))
}

fn eur(x: f64) -> String {
    if x >= 100.0 { format!("€{:.0}", x) } else { format!("€{:.2}", x).replace('.', ",") }
}

pub struct Report {
    pub summary: String,
    pub data: Value,
    pub tokens: u64,
}

/// `country`: "at" (asking prices on willhaben) or "de" (Kleinanzeigen).
pub async fn analyze(app: &AppState, item: &str, country: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<Report> {
    crate::browser::sessions::check("ebay")?;
    let data = crate::thinker::data_dir();
    let screen = crate::thinker::accounts_screen();
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let res = async {
        crate::browser::sessions::ready(&tab, "ebay", on_user).await.context("eBay sign-in (sold prices need it)")?;
        let mut rows: Vec<Value> = vec![];
        let mut seen = std::collections::HashSet::new();
        for page in 1..=2 {
            crate::browser::sessions::take("ebay", 1)?;
            let q: String = url::form_urlencoded::byte_serialize(item.as_bytes()).collect();
            let u = format!("https://www.ebay.de/sch/i.html?_nkw={q}&LH_Sold=1&LH_Complete=1&_sop=13&_ipg=60&_pgn={page}");
            tab.goto(&u, Duration::from_secs(30)).await?;
            tokio::time::sleep(Duration::from_millis(2500)).await;
            let got = tab.eval(SOLD_JS).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let fresh: Vec<Value> = got.into_iter().filter(|r| seen.insert(r["id"].as_str().unwrap_or("").to_string())).collect();
            let n = fresh.len();
            rows.extend(fresh);
            // eBay ignores the page parameter for some searches: page 2 repeats page 1.
            if n < 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(6000 + (chrono::Local::now().timestamp_millis() % 4000) as u64)).await;
        }
        let s = |r: &Value, k: &str| r[k].as_str().unwrap_or("").to_string();
        let priced: Vec<&Value> = rows.iter().filter(|r| price_num(&s(r, "price")).is_some()).collect();
        let (keep, mut tokens) = judge(app, item, &priced.iter().map(|r| (s(r, "title"), s(r, "condition"), s(r, "price"))).collect::<Vec<_>>()).await;
        let sold: Vec<Offer> = priced.iter().zip(&keep).filter(|(_, k)| **k).map(|(r, _)| Offer { title: s(r, "title"), price: price_num(&s(r, "price")).unwrap_or(0.0), date: s(r, "sold"), url: s(r, "url") }).filter(|o| o.price > 0.0).collect();
        let sold = trim(sold);

        // Asking prices on the local classifieds site (same judge).
        let (site, label) = if country == "de" { ("kleinanzeigen", "Kleinanzeigen") } else { ("willhaben", "willhaben") };
        let mut asking: Vec<Offer> = vec![];
        if let Some(u) = crate::monitors::search_url(site, "market", item, "", None, None) {
            if crate::browser::listings::load(&tab, &u).await.is_ok() {
                if let Ok((plan, t)) = crate::browser::listings::learn(app, &tab, app.cfg.provider).await {
                    tokens += t;
                    let items = crate::browser::listings::apply(&tab, &plan).await.unwrap_or_default();
                    let items: Vec<_> = items.into_iter().filter(|l| price_num(&l.price).is_some_and(|p| p > 0.0)).take(60).collect();
                    let (k, t) = judge(app, item, &items.iter().map(|l| (l.title.clone(), String::new(), l.price.clone())).collect::<Vec<_>>()).await;
                    tokens += t;
                    asking = trim(items.iter().zip(&k).filter(|(_, k)| **k).map(|(l, _)| Offer { title: l.title.clone(), price: price_num(&l.price).unwrap_or(0.0), date: l.posted.clone(), url: l.url.clone() }).collect());
                }
            }
        }

        let dates: Vec<&str> = sold.iter().map(|o| o.date.as_str()).filter(|d| !d.is_empty()).collect();
        let span = match (dates.last(), dates.first()) { (Some(a), Some(b)) if a != b => format!(" between {a} and {b}"), _ => String::new() };
        let mut summary = match stats(&sold) {
            Some((m, a, b)) if sold.len() >= MIN_SALES => format!(
                "{item}: sells for about {} on eBay (median of {} real sales{span}; typical range {}–{}). Based only on listings of exactly this item, complete and working ({} of {} sold listings; accessories, other models, defects and bundles left out).",
                eur(m), sold.len(), eur(a), eur(b), sold.len(), rows.len()),
            _ => format!(
                "{item}: not enough matching eBay sales to name a price ({} of {} sold listings are this item, complete and working). The sold results are mostly accessories, parts or other models; try a more exact name or a category (e.g. the bike itself).",
                sold.len(), rows.len()),
        };
        if let Some((m, a, b)) = stats(&asking).filter(|_| asking.len() >= 3) {
            summary.push_str(&format!(" Asking prices on {label} right now: median {} ({}–{}, {} listings) — asking, not selling prices.", eur(m), eur(a), eur(b), asking.len()));
        }
        let ex = |xs: &[Offer]| xs.iter().take(12).map(|o| json!({"title": o.title, "price": o.price, "date": o.date, "url": o.url})).collect::<Vec<_>>();
        let data = json!({"item": item, "sold_listings": rows.len(), "kept": sold.len(), "median": stats(&sold).map(|x| x.0), "p25": stats(&sold).map(|x| x.1), "p75": stats(&sold).map(|x| x.2),
                          "enough": sold.len() >= MIN_SALES, "examples": ex(&sold), "asking": {"site": label, "n": asking.len(), "median": stats(&asking).map(|x| x.0), "examples": ex(&asking)}});
        Ok::<_, anyhow::Error>(Report { summary, data, tokens })
    }
    .await;
    tab.close().await;
    let _ = chrome.save_cookies(screen).await;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    fn o(p: f64) -> Offer {
        Offer { title: String::new(), price: p, date: String::new(), url: String::new() }
    }
    #[test]
    fn trims_single_bid_oddities() {
        let xs: Vec<Offer> = [20.0, 430.0, 450.0, 460.0, 470.0, 480.0, 480.0, 490.0, 500.0, 520.0].into_iter().map(o).collect();
        let t = trim(xs);
        assert!(t.iter().all(|x| x.price > 100.0));
        let (m, _, _) = stats(&t).unwrap();
        assert!((m - 480.0).abs() < 1.0);
    }
}
