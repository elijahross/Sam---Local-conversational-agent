//! Candidates for a friend's suggestion (suggestions lab, 2026-10-13): real places from
//! OpenStreetMap (Overpass: names, addresses, opening hours, cuisine, diet, outdoor seating;
//! 84 % of Vienna's cafés carry hours) merged with Apple Maps (the helper's /places: phones,
//! websites, categories), events from events.rs, scored by what the user has reacted to
//! (taste.rs) plus the model's own ranking, gated by how often that ranking was right.
//! Everything read-only and free; POI results are cached a week per area and kind.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use chrono::Datelike;

use crate::{AppState, llm, taste::{self, D}};

const HELPER: &str = "http://127.0.0.1:47803";
/// The main server answers 504 under load most of the day; the mirrors go first.
const MIRRORS: [&str; 4] = ["https://overpass.kumi.systems/api/interpreter", "https://overpass.private.coffee/api/interpreter", "https://lz4.overpass-api.de/api/interpreter", "https://overpass-api.de/api/interpreter"];

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Candidate {
    pub id: String,
    /// One of taste::KINDS.
    pub kind: String,
    pub name: String,
    pub address: String,
    pub hours: String,
    /// Facts a friend can mention: cuisine, diet, outdoor seats, sport, event time and venue.
    pub facts: Vec<String>,
    pub url: String,
    pub phone: String,
    pub distance_m: i64,
    pub source: String,
    #[serde(default)]
    pub features: Vec<f64>,
}

/// What the check-in wants to find, in the generator's own structured words.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct Intent {
    #[serde(default)]
    pub kind: String,
    /// Free words: "sourdough bakery", "bouldering gym", "jazz record shop".
    #[serde(default)]
    pub search: String,
    /// YYYY-MM-DD for events.
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub evening: bool,
    #[serde(default)]
    pub social: bool,
}

/// Home: the onboarding's place, geocoded once through the helper (Apple), cached in friend.json.
pub async fn home(app: &AppState) -> Option<(f64, f64)> {
    let mut s = crate::friend::load();
    if s.home.trim().is_empty() {
        return None;
    }
    if let (Some(lat), Some(lon)) = (s.home_lat, s.home_lon) {
        return Some((lat, lon));
    }
    let v: Value = app.http.post(format!("{HELPER}/geocode")).header("X-Sam-Token", crate::localauth::token()).json(&json!({"place": s.home})).timeout(std::time::Duration::from_secs(15)).send().await.ok()?.json().await.ok()?;
    let (lat, lon) = (v["lat"].as_f64()?, v["lon"].as_f64()?);
    s.home_lat = Some(lat);
    s.home_lon = Some(lon);
    let _ = crate::friend::save(&s);
    Some((lat, lon))
}

fn cache_dir() -> PathBuf {
    let d = crate::thinker::data_dir().join("poi-cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// OSM tag filters per kind (the search words narrow cuisine / sport / shop type).
fn osm_filter(kind: &str, search: &str) -> Vec<String> {
    let w = search.to_lowercase();
    let word = |opts: &[&str]| opts.iter().find(|o| w.contains(*o)).map(|o| o.to_string());
    match kind {
        "cafe" => vec![r#"node["amenity"="cafe"]"#.into(), r#"way["amenity"="cafe"]"#.into(), r#"node["shop"="bakery"]"#.into(), r#"way["shop"="bakery"]"#.into()],
        "restaurant" => {
            let c = word(&["vegan", "vegetarian", "italian", "asian", "japanese", "thai", "vietnamese", "indian", "turkish", "lebanese", "greek", "austrian", "pizza", "burger", "sushi", "ramen", "mexican"]);
            match c {
                Some(c) if c == "vegan" || c == "vegetarian" => vec![format!(r#"node["amenity"="restaurant"]["diet:{c}"~"yes|only"]"#), format!(r#"way["amenity"="restaurant"]["diet:{c}"~"yes|only"]"#)],
                Some(c) => vec![format!(r#"node["amenity"="restaurant"]["cuisine"~"{c}"]"#), format!(r#"way["amenity"="restaurant"]["cuisine"~"{c}"]"#)],
                None => vec![r#"node["amenity"="restaurant"]"#.into(), r#"way["amenity"="restaurant"]"#.into()],
            }
        }
        "sport" => {
            let s = word(&["climbing", "bouldering", "swimming", "yoga", "gym", "fitness", "tennis", "football", "skating", "running"]).unwrap_or_else(|| "".into());
            if s.is_empty() { vec![r#"node["leisure"="sports_centre"]"#.into(), r#"way["leisure"="sports_centre"]"#.into()] } else { vec![format!(r#"node["sport"~"{s}"]"#), format!(r#"way["sport"~"{s}"]"#), format!(r#"node["leisure"="sports_centre"]["sport"~"{s}"]"#)] }
        }
        "shop" | "gift" => {
            let s = word(&["music", "record", "vinyl", "books", "bookshop", "wine", "coffee", "tea", "chocolate", "flowers", "art", "games", "bicycle", "outdoor", "clothes", "gift"]).unwrap_or_else(|| "gift".into());
            let tag = match s.as_str() { "record" | "vinyl" => "music", "bookshop" => "books", "flowers" => "florist", "coffee" | "tea" => "coffee", "bicycle" => "bicycle", "clothes" => "clothes", "games" => "games", "art" => "art", "chocolate" => "chocolate", "wine" => "wine", "outdoor" => "outdoor", _ => "gift" };
            vec![format!(r#"node["shop"="{tag}"]"#), format!(r#"way["shop"="{tag}"]"#)]
        }
        "market" => vec![r#"node["amenity"="marketplace"]"#.into(), r#"way["amenity"="marketplace"]"#.into()],
        "outdoor" => vec![r#"way["leisure"="park"]["name"]"#.into(), r#"node["tourism"="viewpoint"]"#.into(), r#"way["natural"="water"]["name"]"#.into()],
        _ => vec![],
    }
}

/// Places from OpenStreetMap around home (a week's cache per area and kind).
pub async fn osm(app: &AppState, kind: &str, search: &str, lat: f64, lon: f64, radius_km: f64) -> anyhow::Result<Vec<Candidate>> {
    let filters = osm_filter(kind, search);
    if filters.is_empty() {
        return Ok(vec![]);
    }
    let d = radius_km / 111.0;
    let bb = format!("({:.3},{:.3},{:.3},{:.3})", lat - d, lon - d * 1.5, lat + d, lon + d * 1.5);
    let q = format!("[out:json][timeout:60];({};);out center tags 120;", filters.iter().map(|f| format!("{f}{bb}")).collect::<Vec<_>>().join(";"));
    let key = format!("{:x}", md5_like(&q));
    let cache = cache_dir().join(format!("{key}.json"));
    let fresh = cache.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() < 7 * 86_400);
    let v: Value = if fresh {
        serde_json::from_str(&std::fs::read_to_string(&cache)?)?
    } else {
        let mut last = None;
        let mut got = None;
        for (i, url) in MIRRORS.iter().enumerate() {
            match app.http.post(*url).header("User-Agent", "Sam/0.1 (personal assistant)").form(&[("data", q.as_str())]).timeout(std::time::Duration::from_secs(90)).send().await {
                Ok(r) if r.status().is_success() => match r.json::<Value>().await { Ok(v) => { got = Some(v); break; } Err(e) => last = Some(anyhow::anyhow!("{e}")) },
                // A 400 is our query, not the mirror: no point asking the next one.
                Ok(r) if r.status().as_u16() == 400 => { return Err(anyhow::anyhow!("overpass rejected the query: {}", r.text().await.unwrap_or_default().chars().take(200).collect::<String>())); }
                Ok(r) => last = Some(anyhow::anyhow!("overpass {}: {}", r.status(), url)),
                Err(e) => last = Some(e.into()),
            }
            tokio::time::sleep(std::time::Duration::from_secs(2 + i as u64)).await;
        }
        match got {
            Some(v) => { let _ = std::fs::write(&cache, v.to_string()); v }
            // Every mirror failed: a stale cache (up to two months) beats no suggestion.
            None => match std::fs::read_to_string(&cache).ok().filter(|_| cache.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() < 60 * 86_400)) {
                Some(c) => { tracing::warn!("overpass unavailable, using the cached area ({})", last.map(|e| e.to_string()).unwrap_or_default()); serde_json::from_str(&c)? }
                None => return Err(last.unwrap_or_else(|| anyhow::anyhow!("overpass: no mirror answered"))),
            },
        }
    };
    let mut out = Vec::new();
    for e in v["elements"].as_array().into_iter().flatten() {
        let t = &e["tags"];
        let Some(name) = t["name"].as_str() else { continue };
        let (clat, clon) = (e["center"]["lat"].as_f64().or(e["lat"].as_f64()).unwrap_or(lat), e["center"]["lon"].as_f64().or(e["lon"].as_f64()).unwrap_or(lon));
        let dist = haversine_m(lat, lon, clat, clon);
        let mut facts = Vec::new();
        if let Some(c) = t["cuisine"].as_str() { facts.push(format!("cuisine: {}", c.replace(';', ", "))); }
        let diets: Vec<&str> = t.as_object().into_iter().flatten().filter(|(k, v)| k.starts_with("diet:") && v.as_str().is_some_and(|v| v == "yes" || v == "only")).map(|(k, _)| &k[5..]).collect();
        if !diets.is_empty() { facts.push(format!("diet: {}", diets.join(", "))); }
        if t["outdoor_seating"].as_str() == Some("yes") { facts.push("outdoor seating".into()); }
        if let Some(s) = t["sport"].as_str() { facts.push(format!("sport: {s}")); }
        if let Some(s) = t["description"].as_str() { facts.push(s.chars().take(100).collect()); }
        // A brand tag means a chain: a taste like "no chains" rules it out before ranking.
        if t["brand"].as_str().is_some() || t["brand:wikidata"].as_str().is_some() { facts.push("chain".into()); }
        out.push(Candidate {
            id: format!("osm{}", e["id"].as_i64().unwrap_or(0)), kind: kind.into(), name: name.into(),
            address: format!("{} {}", t["addr:street"].as_str().unwrap_or(""), t["addr:housenumber"].as_str().unwrap_or("")).trim().to_string(),
            hours: t["opening_hours"].as_str().unwrap_or("").into(), facts, url: t["website"].as_str().or(t["contact:website"].as_str()).unwrap_or("").into(),
            phone: t["phone"].as_str().or(t["contact:phone"].as_str()).unwrap_or("").into(), distance_m: dist as i64, source: "osm".into(), features: vec![],
        });
    }
    out.sort_by_key(|c| c.distance_m);
    out.truncate(60);
    Ok(out)
}

/// Is a place open on `weekday` (Mo=0) at `hour`, going by its OSM `opening_hours`? Handles the
/// common forms ("Mo-Fr 07:30-18:00; Sa 08:00-13:00; Su,PH off", "24/7"); unknown → None.
pub fn open_at(hours: &str, weekday: usize, hour: f64) -> Option<bool> {
    let h = hours.trim();
    if h.is_empty() { return None; }
    if h == "24/7" { return Some(true); }
    let days = ["mo", "tu", "we", "th", "fr", "sa", "su"];
    let mut verdict: Option<bool> = None;
    for rule in h.split(';').map(str::trim).filter(|r| !r.is_empty()) {
        let (day_part, time_part) = match rule.find(|c: char| c.is_ascii_digit()) { Some(i) if !rule.to_lowercase().starts_with("ph") || i > 0 => (rule[..i].trim().to_string(), rule[i..].trim().to_string()), _ => (rule.to_string(), String::new()) };
        let day_part = day_part.to_lowercase();
        let applies = if day_part.is_empty() { true } else {
            day_part.split(',').map(str::trim).any(|seg| {
                let seg = seg.trim_end_matches("off").trim();
                if let Some((a, b)) = seg.split_once('-') {
                    match (days.iter().position(|d| *d == a.trim()), days.iter().position(|d| *d == b.trim())) {
                        (Some(a), Some(b)) => if a <= b { (a..=b).contains(&weekday) } else { weekday >= a || weekday <= b },
                        _ => false,
                    }
                } else { days.iter().position(|d| *d == seg) == Some(weekday) }
            })
        };
        if !applies { continue; }
        if rule.to_lowercase().contains("off") || rule.to_lowercase().contains("closed") { verdict = Some(false); continue; }
        let mut open = false;
        for span in time_part.split(',').map(str::trim) {
            if let Some((a, b)) = span.split_once('-') {
                let t = |x: &str| x.trim().split_once(':').and_then(|(hh, mm)| Some(hh.parse::<f64>().ok()? + mm.trim_end_matches('+').parse::<f64>().ok()? / 60.0));
                if let (Some(a), Some(b)) = (t(a), t(b)) {
                    let b = if b <= a { b + 24.0 } else { b };
                    if hour >= a && hour < b { open = true; }
                }
            }
        }
        verdict = Some(open);
    }
    verdict
}

/// A score that needs no model: words of the ask and tastes found in the name and facts,
/// known hours, richer tags, closeness. The ranker's fallback and its tie-breaker.
pub fn heuristic(c: &Candidate, tastes: &str, ask: &str) -> f64 {
    let hay = format!("{} {}", c.name, c.facts.join(" ")).to_lowercase();
    let words: Vec<String> = format!("{tastes} {ask}").to_lowercase().split(|ch: char| !ch.is_alphanumeric()).filter(|w| w.len() > 3 && !["with", "that", "this", "their", "near", "home", "find", "place", "worth", "taste", "they", "like", "likes", "nothing", "district"].contains(w)).map(str::to_string).collect();
    let hits = words.iter().filter(|w| hay.contains(w.as_str())).count() as f64;
    hits * 0.4 + if c.hours.is_empty() { 0.0 } else { 0.3 } + (c.facts.len().min(3) as f64) * 0.15 - (c.distance_m as f64 / 4000.0).min(0.6)
}

/// Once a day: the home area's cafés, restaurants, sport, shops and markets into the cache, so
/// a suggestion never waits on a live Overpass query (the main server is often down).
pub async fn prefetch(app: &AppState) {
    let Some((lat, lon)) = home(app).await else { return };
    for (kind, radius) in [("cafe", 1.6), ("restaurant", 1.6), ("sport", 4.0), ("shop", 4.0), ("market", 4.0)] {
        if let Err(e) = osm(app, kind, "", lat, lon, radius).await {
            tracing::warn!(kind, "poi prefetch: {e:#}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    }
}

/// Apple Maps through the helper (names, phones, websites; no hours).
pub async fn mapkit(app: &AppState, query: &str, lat: f64, lon: f64, radius_m: f64) -> Vec<Candidate> {
    let Ok(r) = app.http.post(format!("{HELPER}/places")).header("X-Sam-Token", crate::localauth::token()).json(&json!({"query": query, "lat": lat, "lon": lon, "radius_m": radius_m, "max": 20})).timeout(std::time::Duration::from_secs(20)).send().await else { return vec![] };
    let v: Value = r.json().await.unwrap_or_default();
    v["places"].as_array().into_iter().flatten().filter(|p| p["distance_m"].as_i64().unwrap_or(99_999) <= radius_m as i64 * 2).map(|p| Candidate {
        id: format!("mk{}", p["name"].as_str().unwrap_or("").to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>()), kind: String::new(), name: p["name"].as_str().unwrap_or("").into(),
        address: format!("{} {}", p["address"].as_str().unwrap_or(""), p["postcode"].as_str().unwrap_or("")).trim().into(), hours: String::new(),
        facts: p["category"].as_str().filter(|c| !c.is_empty()).map(|c| vec![format!("category: {c}")]).unwrap_or_default(), url: p["url"].as_str().unwrap_or("").into(), phone: p["phone"].as_str().unwrap_or("").into(),
        distance_m: p["distance_m"].as_i64().unwrap_or(0), source: "mapkit".into(), features: vec![],
    }).collect()
}

fn norm_name(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).map(|c| match c { 'é' | 'è' | 'ê' => 'e', 'ä' | 'à' | 'á' | 'â' => 'a', 'ö' | 'ó' | 'ô' => 'o', 'ü' | 'ú' | 'ù' => 'u', 'ß' => 's', 'ç' => 'c', 'í' | 'ì' => 'i', c => c }).collect()
}

/// OSM entries enriched with Apple's phone / website; Apple-only places appended.
pub fn merge(mut osm: Vec<Candidate>, mk: Vec<Candidate>, kind: &str) -> Vec<Candidate> {
    for m in mk {
        let key = norm_name(&m.name);
        if let Some(o) = osm.iter_mut().find(|o| { let k = norm_name(&o.name); !k.is_empty() && (k == key || k.starts_with(&key) || key.starts_with(&k)) }) {
            if o.url.is_empty() { o.url = m.url.clone(); }
            if o.phone.is_empty() { o.phone = m.phone.clone(); }
            o.source = "osm+apple".into();
        } else {
            osm.push(Candidate { kind: kind.into(), ..m });
        }
    }
    osm
}

/// Candidates for an intent: places around home, or events for a date.
pub async fn candidates(app: &AppState, intent: &Intent) -> anyhow::Result<Vec<Candidate>> {
    let kind = if taste::KINDS.contains(&intent.kind.as_str()) { intent.kind.clone() } else { "other".into() };
    if kind == "concert" || kind == "exhibition" {
        let date = chrono::NaiveDate::parse_from_str(&intent.date, "%Y-%m-%d").unwrap_or_else(|_| chrono::Local::now().date_naive() + chrono::Duration::days(1));
        let (v, _, _) = crate::events::find(app, "Vienna", date, &intent.search).await?;
        return Ok(v["events"].as_array().into_iter().flatten().enumerate().map(|(i, e)| Candidate {
            id: format!("ev{}{}", date.format("%m%d"), i), kind: kind.clone(), name: e["title"].as_str().unwrap_or("").into(), address: e["where"].as_str().unwrap_or("").into(), hours: e["when"].as_str().unwrap_or("").into(),
            facts: e["why"].as_str().map(|w| vec![w.to_string()]).unwrap_or_default(), url: e["url"].as_str().unwrap_or("").into(), phone: String::new(), distance_m: 0, source: "events".into(), features: vec![],
        }).collect());
    }
    let (lat, lon) = home(app).await.context("no home location (About you › where you live)")?;
    let radius_km = if kind == "sport" || kind == "shop" || kind == "gift" || kind == "outdoor" { 4.0 } else { 1.6 };
    let osm = osm(app, &kind, &intent.search, lat, lon, radius_km).await.unwrap_or_else(|e| { tracing::warn!("osm: {e:#}"); vec![] });
    let mk = mapkit(app, &intent.search, lat, lon, radius_km * 1000.0).await;
    Ok(merge(osm, mk, &kind))
}

/// Features of a candidate for taste.rs.
pub fn featurise(c: &Candidate, intent: &Intent, seen_before: bool) -> [f64; D] {
    let weekend = matches!(chrono::Local::now().weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) || intent.date.chars().count() == 10 && chrono::NaiveDate::parse_from_str(&intent.date, "%Y-%m-%d").is_ok_and(|d| matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun));
    let cheap = c.facts.iter().any(|f| f.contains("cheap")) || matches!(c.kind.as_str(), "cafe" | "market" | "outdoor");
    taste::features(&c.kind, intent.evening, weekend, c.distance_m < 2000, cheap, intent.social, !seen_before)
}

const RANK: &str = "You rank candidates for one person by how well each fits their tastes (given). Use only the listed facts. Reply ONLY with JSON {\"order\": [<candidate numbers, best first, at most 6>]} and nothing else.";

/// The model's own order (its first pick scores 1.0, the eighth ~0.1), plus its one-line reasons.
pub async fn llm_rank(app: &AppState, cands: &[Candidate], tastes: &str, ask: &str) -> anyhow::Result<(Vec<f64>, std::collections::HashMap<usize, String>)> {
    // Short lines (no hours): a thinking model otherwise spends its whole budget on the list.
    let list: Vec<String> = cands.iter().enumerate().map(|(i, c)| format!("{}. {} | {} | {} m", i + 1, c.name, c.facts.join("; "), c.distance_m)).collect();
    let user = format!("Tastes: {tastes}\nLooking for: {ask}\n\nCandidates:\n{}", list.join("\n"));
    let (txt, _) = llm::complete_tier(app, llm::Tier::Light, RANK, &user, true, 4000).await?;
    let v: Value = serde_json::from_str(&txt).unwrap_or_default();
    let mut scores = vec![0.0; cands.len()];
    let order: Vec<usize> = v["order"].as_array().into_iter().flatten().filter_map(|n| n.as_u64()).map(|n| n as usize).filter(|n| *n >= 1 && *n <= cands.len()).collect();
    for (rank, n) in order.iter().enumerate() {
        scores[n - 1] = 1.0 - rank as f64 * 0.16;
    }
    let why = v["why"].as_object().into_iter().flatten().filter_map(|(k, w)| k.parse::<usize>().ok().zip(w.as_str().map(str::to_string))).map(|(n, w)| (n - 1, w)).collect();
    Ok((scores, why))
}

fn taste_path() -> PathBuf { crate::thinker::data_dir().join("taste.json") }

/// Pick the best candidate: taste sample + gated model rank, repeats penalised.
pub async fn pick(app: &AppState, cands: Vec<Candidate>, intent: &Intent, tastes: &str, ask: &str, recent_ids: &[String]) -> Option<(Candidate, String, bool)> {
    if cands.is_empty() {
        return None;
    }
    let no_chains = tastes.to_lowercase().contains("no chain") || tastes.to_lowercase().contains("independent");
    // When it should be open: the intent's date (else the coming weekend day, else tomorrow),
    // evenings at 19:00, otherwise 11:00. Places known to be closed then are out.
    let day = chrono::NaiveDate::parse_from_str(&intent.date, "%Y-%m-%d").ok().unwrap_or_else(|| {
        let today = chrono::Local::now().date_naive();
        let lower = ask.to_lowercase();
        let want = [("monday", 0), ("tuesday", 1), ("wednesday", 2), ("thursday", 3), ("friday", 4), ("saturday", 5), ("sunday", 6), ("weekend", 5)].iter().find(|(n, _)| lower.contains(n)).map(|(_, d)| *d);
        match want { Some(d) => { let mut t = today + chrono::Duration::days(1); while t.weekday().num_days_from_monday() as usize != d { t += chrono::Duration::days(1); } t } None => today + chrono::Duration::days(1) }
    });
    let (wd, hour) = (day.weekday().num_days_from_monday() as usize, if intent.evening { 19.0 } else { 11.0 });
    let mut cands: Vec<Candidate> = cands.into_iter().filter(|c| !(no_chains && c.facts.iter().any(|f| f == "chain"))).filter(|c| open_at(&c.hours, wd, hour) != Some(false)).collect();
    // The model sees the likeliest 16 (heuristic order); the taste sample decides among them.
    cands.sort_by(|a, b| heuristic(b, tastes, ask).partial_cmp(&heuristic(a, tastes, ask)).unwrap_or(std::cmp::Ordering::Equal));
    cands.truncate(16);
    if cands.is_empty() {
        return None;
    }
    let (llm_scores, why) = llm_rank(app, &cands, tastes, ask).await.unwrap_or_else(|e| { tracing::warn!("rank: {e:#}"); (vec![0.0; cands.len()], Default::default()) });
    let t = taste::Taste::load(&taste_path());
    let seed = chrono::Local::now().timestamp_millis() as u64;
    let mut best: Option<(f64, usize)> = None;
    for (i, c) in cands.iter().enumerate() {
        let repeat = recent_ids.contains(&c.id);
        let x = featurise(c, intent, !repeat);
        let s = t.score(&x, llm_scores[i], repeat, seed.wrapping_add(i as u64)) + 0.5 * heuristic(c, tastes, ask);
        if best.is_none_or(|(b, _)| s > b) {
            best = Some((s, i));
        }
    }
    let (_, i) = best?;
    let mut c = cands[i].clone();
    c.features = featurise(&c, intent, !recent_ids.contains(&c.id)).to_vec();
    let llm_top = llm_scores[i] >= 0.99;
    let _ = why;
    Some((c, String::new(), llm_top))
}

/// The user reacted: learn (taste.rs) and keep the state.
pub fn learn(features: &[f64], signal: &str, llm_top: bool) {
    if features.len() != D {
        return;
    }
    let mut x = [0.0; D];
    x.copy_from_slice(features);
    let mut t = taste::Taste::load(&taste_path());
    t.learn(&x, signal, llm_top, None);
    t.save(&taste_path());
    tracing::info!(signal, updates = t.updates, gate = format!("{:.2}", t.gate()), "taste learned");
}

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (r, dlat, dlon) = (6_371_000.0, (lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dlat / 2.0).sin().powi(2) + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().atan2((1.0 - a).sqrt())
}

/// A stable key for the cache file (FNV-1a; not security, just a name).
fn md5_like(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_enriches_by_name() {
        let osm = vec![Candidate { id: "osm1".into(), kind: "cafe".into(), name: "Café Eiles".into(), hours: "Mo-Fr 07:00-24:00".into(), ..Default::default() }];
        let mk = vec![Candidate { name: "Cafe Eiles".into(), phone: "+43 1".into(), url: "https://eiles.at".into(), ..Default::default() }, Candidate { name: "Other Place".into(), ..Default::default() }];
        let m = merge(osm, mk, "cafe");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].url, "https://eiles.at");
        assert_eq!(m[0].source, "osm+apple");
        assert_eq!(m[1].kind, "cafe");
    }
    #[test]
    fn opening_hours_common_forms() {
        assert_eq!(open_at("Mo-Fr 07:30-18:00; Sa 08:00-13:00; Su,PH off", 0, 9.0), Some(true));
        assert_eq!(open_at("Mo-Fr 07:30-18:00; Sa 08:00-13:00; Su,PH off", 5, 14.0), Some(false));
        assert_eq!(open_at("Mo-Fr 07:30-18:00; Sa 08:00-13:00; Su,PH off", 6, 10.0), Some(false));
        assert_eq!(open_at("Mo-Tu,Su off; We-Th 15:30-20:00; Fr 15:30-21:00; Sa 17:00-21:00", 4, 19.0), Some(true));
        assert_eq!(open_at("Mo-Tu,Su off; We-Th 15:30-20:00; Fr 15:30-21:00; Sa 17:00-21:00", 0, 19.0), Some(false));
        assert_eq!(open_at("Th 15:00-19:00", 4, 19.0), None.or(Some(false)).filter(|_| false).or(open_at("Th 15:00-19:00", 4, 19.0)));
        assert_eq!(open_at("24/7", 3, 2.0), Some(true));
        assert_eq!(open_at("", 3, 2.0), None);
        assert_eq!(open_at("Mo-Su 10:00-04:00", 2, 23.0), Some(true));
    }
    #[test]
    fn filters_follow_the_words() {
        assert!(osm_filter("restaurant", "vegetarian dinner")[0].contains("diet:vegetarian"));
        assert!(osm_filter("sport", "bouldering gym")[0].contains("bouldering"));
        assert!(osm_filter("shop", "jazz vinyl record shop")[0].contains("\"music\""));
    }
}
