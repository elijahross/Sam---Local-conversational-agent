//! Listings from a search results page, for monitors (scrape lab). One cheap "plan" is
//! learned per saved search and then applied at ~0 tokens on every run:
//!
//! 1. `itemkey.js` finds which links are listings (the path segment linking the most distinct
//!    ids: /itm/, /expose/, /s-anzeige/, /dp/ …) — no LLM;
//! 2. `cards.js` turns every listing link into its card (largest ancestor about that one item);
//! 3. `embed.js` looks for the page's own data (JSON-LD, __NEXT_DATA__, app state) holding those
//!    ids; if found, ONE LLM call maps its fields (title/price/…) to JSON paths → plan "embed";
//! 4. otherwise the cards themselves with rule-based fields (title = the listing link's text,
//!    price = first currency amount, location = postcode / "aus …", posted = date phrase) →
//!    plan "cards", 0 tokens — if most cards have a title and a price;
//! 5. otherwise a CSS recipe (scrape.rs: repeated group → one LLM call) → plan "css";
//! 6. every run is checked against the cards: a plan that suddenly finds far fewer listings than
//!    the page links to is re-learned once (sites change their markup).
//! Ids are the dedup key: the site's own id, else the id token in the listing URL.

use std::time::Duration;

use anyhow::bail;
use serde_json::{Value, json};

use super::cdp::Tab;
use crate::{AppState, llm::{self, Provider}, monitors::Listing};

pub const ITEMKEY_JS: &str = include_str!("listings/itemkey.js");
pub const CARDS_JS: &str = include_str!("listings/cards.js");
pub const EMBED_JS: &str = include_str!("listings/embed.js");

const EMBED_SYS: &str = "You map a website's embedded listing data to fields. Given two sample elements of the listing array, \
reply ONLY with JSON {\"map\": {\"id\": path, \"url\": path, \"title\": path, \"price\": path, \"location\": path, \"posted\": path}, \"url_prefix\": \"…\"}. \
A path is relative to one element: dot keys and numeric indexes (a.b.0.c); lists of {name, value} objects are picked with [key=VALUE] \
(e.g. attributes.attribute[name=HEADING].values.0). Use null when a field is absent. price = the displayed price (prefer a display string). \
url_prefix: the site origin plus any path needed when the url path is relative (else empty).";

const FIELDS: &[&str] = &["id", "url", "title", "price", "location", "posted"];

/// Loads the page and lets lazy lists render (a few scroll steps, then back to the top).
pub async fn load(tab: &Tab, url: &str) -> anyhow::Result<()> {
    tab.goto(url, Duration::from_secs(30)).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for _ in 0..4 {
        let _ = tab.eval("window.scrollBy(0, window.innerHeight * 1.5)").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = tab.eval("window.scrollTo(0, 0)").await;
    let html_head = tab.eval("document.documentElement.outerHTML.slice(0, 20000)").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if super::extract::is_challenge(&html_head) {
        bail!("the site showed a bot check instead of results");
    }
    Ok(())
}

async fn cards(tab: &Tab, item: &str) -> Vec<Value> {
    tab.eval(&format!("({CARDS_JS})({})", serde_json::to_string(item).unwrap_or_default())).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

/// The id token in a listing URL (same rule as cards.js).
pub fn id_from_url(url: &str) -> Option<String> {
    let path = url::Url::parse(url).ok()?.path().to_string();
    let re = regex::Regex::new(r"(?:^|[/-])([A-Z0-9]{10}|\d{6,}|[0-9a-f]{20,})(?:[/-]|$)").ok()?;
    let ids: Vec<String> = re.captures_iter(&path).map(|c| c[1].to_string()).collect();
    if path.contains("/dp/") {
        // Amazon: the ASIN right after /dp/ (product slugs can hold model numbers too).
        return path.split("/dp/").nth(1).and_then(|r| r.split('/').next()).map(str::to_string).or_else(|| ids.first().cloned());
    }
    ids.last().cloned()
}

fn to_listings(items: &Value, cards: &[Value]) -> Vec<Listing> {
    let card_urls: std::collections::HashMap<String, String> = cards.iter().map(|c| (c["id"].as_str().unwrap_or("").to_string(), c["url"].as_str().unwrap_or("").to_string())).collect();
    items.as_array().into_iter().flatten().filter_map(|i| {
        let s = |k: &str| i[k].as_str().map(|x| x.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        // desc: the description (embedded data) or the card's text, for the monitor's judge.
        let desc = if i["desc"].is_string() { s("desc") } else { s("text") }.chars().take(240).collect();
        let mut l = Listing { id: s("id"), url: s("url"), title: s("title"), price: s("price"), location: s("location"), posted: s("posted"), desc };
        // The site id, else the id in the URL; ids the cards know keep their clean URL.
        if l.id.is_empty() || l.id.len() > 64 {
            l.id = id_from_url(&l.url).unwrap_or_default();
        }
        if l.url.is_empty() {
            l.url = card_urls.get(&l.id).cloned().unwrap_or_default();
        }
        (!l.id.is_empty() && !(l.title.is_empty() && l.price.is_empty())).then_some(l)
    }).collect()
}

/// Built-in plan for willhaben (listings lab L2): the results array in `__NEXT_DATA__` is fixed,
/// the learned map was the same in every run, and an empty search shows promoted ads that the
/// cards plan would read as listings (a €90 coat on a free-only search).
pub fn builtin_plan(url: &str) -> Option<Value> {
    url.contains("willhaben.at/iad/").then(|| json!({
        "kind": "embed", "item": "/d/", "known": true,
        "path": "#__NEXT_DATA__.props.pageProps.searchResult.advertSummaryList.advertSummary",
        "map": {"id": "id", "url": "attributes.attribute[name=SEO_URL].values.0", "title": "attributes.attribute[name=HEADING].values.0",
                "price": "attributes.attribute[name=PRICE_FOR_DISPLAY].values.0", "location": "attributes.attribute[name=LOCATION].values.0",
                "posted": "attributes.attribute[name=PUBLISHED_String].values.0", "desc": "attributes.attribute[name=BODY_DYN].values.0"},
        "url_prefix": "https://www.willhaben.at/iad/",
    }))
}

/// Learns a plan on the loaded page. Returns (plan, tokens).
pub async fn learn(app: &AppState, tab: &Tab, model: Provider) -> anyhow::Result<(Value, u64)> {
    let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if let Some(p) = builtin_plan(&here) {
        return Ok((p, 0));
    }
    let key = tab.eval(ITEMKEY_JS).await?;
    let Some(item) = key["item"].as_str().map(str::to_string) else { bail!("couldn't find listings on this page") };
    // A known marketplace with 0-2 results (itemkey.js knows its item path): a valid, tiny
    // result. Its cards plan is rule-based (0 tokens) and an empty page stays empty.
    if key["known"] == true {
        return Ok((json!({"kind": "cards", "item": item, "known": true}), 0));
    }
    let cs = cards(tab, &item).await;
    let ids: Vec<Value> = cs.iter().map(|c| c["id"].clone()).collect();
    let mut tokens = 0u64;
    // Embedded data first: exact values, every result (not only the rendered ones), ~0 tokens.
    let found = tab.eval(&format!("({EMBED_JS})({}, null)", Value::Array(ids.clone()))).await.unwrap_or(Value::Null);
    let enough = (ids.len() as u64 * 6 / 10).clamp(3, 5);
    if found["matched"].as_u64().unwrap_or(0) >= enough {
        let user = format!("Site origin: {}\nTwo sample elements:\n{}", found["origin"], serde_json::to_string(&found["samples"])?.chars().take(9000).collect::<String>());
        let (txt, u) = llm::complete_with(app, model, crate::llm::Tier::Light, EMBED_SYS, &user, true, 4000).await?;
        tokens += u.prompt + u.completion;
        let r: Value = serde_json::from_str(&txt)?;
        let map: serde_json::Map<String, Value> = r["map"].as_object().cloned().unwrap_or_default().into_iter().filter(|(_, v)| v.is_string()).collect();
        let plan = json!({"kind": "embed", "item": item, "path": found["path"], "map": map, "url_prefix": r["url_prefix"]});
        if !to_listings(&apply_embed(tab, &plan).await?, &cs).is_empty() {
            return Ok((plan, tokens));
        }
    }
    // The cards with rule-based fields, when they read well (lab: eBay / Kleinanzeigen 1.0, 0 tokens).
    if cards_ok(&cs) {
        return Ok((json!({"kind": "cards", "item": item}), tokens));
    }
    // Otherwise a CSS recipe over the repeated cards.
    let schema: Vec<String> = FIELDS.iter().map(|s| s.to_string()).collect();
    let g = tab.eval(super::scrape::GROUPS_JS).await?;
    if g.is_null() {
        bail!("couldn't find a list of results on this page");
    }
    let user = format!("Task: listings of a search results page\nSchema fields: {}\nItem group signature: {} (parent {}), {} items on this page.\nNext-page link candidates: {}\n{}",
        schema.join(", "), g["item_sig"], g["parent_sig"], g["count"], g["next"], super::scrape::samples_view(&g));
    let (txt, u) = llm::complete(app, model, super::scrape::RECIPE_SYS, &user, true, 4000).await?;
    tokens += u.prompt + u.completion;
    let mut recipe: super::scrape::Recipe = serde_json::from_str(&txt)?;
    let first: Vec<Value> = serde_json::from_value(tab.eval(&format!("({})({})", super::scrape::APPLY_JS, serde_json::to_string(&recipe)?)).await?).unwrap_or_default();
    let weak = super::scrape::weak_fields(&first, &schema);
    if first.is_empty() || weak.iter().any(|w| w == "title" || w == "price" || w == "url") {
        let user = format!("This recipe returned {} items; these fields were mostly empty: {:?}.\nRecipe: {}\n{}\nReturn the corrected full recipe JSON.", first.len(), weak, serde_json::to_string(&recipe)?, super::scrape::samples_view(&g));
        let (txt, u) = llm::complete(app, model, super::scrape::RECIPE_SYS, &user, true, 4000).await?;
        tokens += u.prompt + u.completion;
        if let Ok(r2) = serde_json::from_str::<super::scrape::Recipe>(&txt) {
            recipe = r2;
        }
    }
    Ok((json!({"kind": "css", "item": item, "recipe": recipe}), tokens))
}

/// Most cards carry a title (and, on priced sites, a price) → the rule-based fields hold.
fn cards_ok(cs: &[Value]) -> bool {
    let titled = cs.iter().filter(|c| c["title"].as_str().is_some_and(|t| t.len() >= 6)).count();
    cs.len() >= 3 && titled * 10 >= cs.len() * 8
}

/// Share of cards showing a price.
fn priced(cs: &[Value]) -> f32 {
    cs.iter().filter(|c| c["price"].as_str().is_some_and(|p| !p.is_empty())).count() as f32 / cs.len().max(1) as f32
}

async fn apply_embed(tab: &Tab, plan: &Value) -> anyhow::Result<Value> {
    tab.eval(&format!("({EMBED_JS})(null, {})", json!({"path": plan["path"], "map": plan["map"], "url_prefix": plan["url_prefix"]}))).await
}

/// Applies a plan on the loaded page. Err when the plan no longer fits the page (re-learn).
pub async fn apply(tab: &Tab, plan: &Value) -> anyhow::Result<Vec<Listing>> {
    let item = plan["item"].as_str().unwrap_or("/");
    let cs = cards(tab, item).await;
    let known = plan["known"] == true;
    let raw = match plan["kind"].as_str() {
        Some("embed") => apply_embed(tab, plan).await?,
        Some("cards") if known && cs.is_empty() => Value::Array(vec![]),
        Some("cards") => {
            if !known && !cards_ok(&cs) {
                bail!("cards no longer read well");
            }
            // Where nearly every listing shows a price, price-less cards are promos ("Amazon Visa",
            // banners); where many don't (Amazon newest-first), every card counts.
            let strict = priced(&cs) >= 0.7;
            Value::Array(cs.iter().filter(|c| !strict || c["price"].as_str().is_some_and(|p| !p.is_empty())).cloned().collect())
        }
        Some("css") => tab.eval(&format!("({})({})", super::scrape::APPLY_JS, plan["recipe"])).await?,
        _ => bail!("unknown plan"),
    };
    let items = to_listings(&raw, &cs);
    // Known sites: an empty result is a valid run (a rare item, nothing new). willhaben's embed
    // plan reads only the results array, so the promoted cards on an empty page don't count.
    if known && (items.is_empty() || plan["kind"] == "embed") {
        return Ok(items);
    }
    // The page links to listings the plan didn't see: markup or data layout changed.
    if items.is_empty() || (cs.len() >= 5 && items.len() * 2 < cs.len().min(20)) {
        bail!("plan found {} of {} listings", items.len(), cs.len());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_from_urls() {
        let f = super::id_from_url;
        assert_eq!(f("https://www.willhaben.at/iad/immobilien/d/mietwohnungen/wien/wien-1090-alsergrund/helle-loft-1305918793/").as_deref(), Some("1305918793"));
        assert_eq!(f("https://www.immobilienscout24.at/expose/69ce7e3d6fd3644eae4a06ca").as_deref(), Some("69ce7e3d6fd3644eae4a06ca"));
        assert_eq!(f("https://www.ebay.de/itm/880003724896").as_deref(), Some("880003724896"));
        assert_eq!(f("https://www.kleinanzeigen.de/s-anzeige/lenovo-x1/3499513526-278-2008").as_deref(), Some("3499513526"));
        assert_eq!(f("https://www.amazon.de/Lenovo-ThinkPad-21V700ASGE/dp/B0H6MPHB13/ref=sr_1_3").as_deref(), Some("B0H6MPHB13"));
        assert_eq!(f("https://www.immobilienscout24.de/expose/164436559").as_deref(), Some("164436559"));
    }
}
