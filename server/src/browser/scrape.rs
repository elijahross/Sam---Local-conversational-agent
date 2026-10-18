//! Structured scraping at constant token cost: the LLM writes a small extraction recipe once
//! (CSS selectors per schema field + pagination), code applies it to every page.
//!
//! 1. find the repeated item group in the DOM without an LLM (siblings with the same shape);
//! 2. one LLM call: 3 pruned item samples + schema → recipe JSON;
//! 3. apply the recipe deterministically on every page (dedupe by key), paginate by next-link,
//!    URL pattern or infinite scroll; a field that comes back mostly empty triggers ONE small
//!    repair call with only failing samples;
//! 4. recipes are cached per (domain, page shape) → a repeat run costs ~0 tokens;
//! 5. login walls: stored credentials → agent login; otherwise the window comes on screen, the
//!    user logs in, we poll until the wall is gone and continue (session saved).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};

use super::cdp::Tab;
use crate::{
    AppState,
    artifacts::{self, Catalog},
    judge,
    llm::{self, Provider, Usage},
};

#[derive(Debug, Clone)]
pub struct Options {
    pub max_items: usize,
    pub max_pages: usize,
    pub model: Provider,
    /// Politeness delay between page loads (ms); not an evasion measure.
    pub page_delay_ms: u64,
    pub recipes_dir: PathBuf,
    /// Opt-in: on a login wall, let the browser agent use stored Keychain credentials.
    /// Default off — the user is asked to log in in the browser window instead.
    pub auto_login: bool,
    /// Where login / check hand-off requests go (voice session notices); stderr if None.
    pub on_user: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Ledger {
    pub llm_calls: Vec<(String, u64, u64)>,
    pub pages: usize,
    pub items: usize,
    pub recipe_cached: bool,
    pub repairs: usize,
    pub login_handoffs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub task: String,
    pub items: usize,
    pub fields: Vec<String>,
    pub coverage: Value,
    pub tokens: u64,
    pub ledger: Ledger,
    pub wall_ms: u128,
    pub dir: String,
    pub status: String,
}

/// Finds the largest group of same-shaped siblings with real content, tags them and returns
/// pruned samples + the group's CSS signature. No LLM.
pub const GROUPS_JS: &str = r#"(() => {
  const sig = (e) => e.tagName.toLowerCase() + [...e.classList].filter(c => !/\d{3,}|active|selected|hover/.test(c)).slice(0, 2).map(c => '.' + CSS.escape(c)).join('');
  let best = null;
  // An open dialog (followers list, filters, results panel) is the user's focus: search there.
  const dialogs = [...document.querySelectorAll('[role=dialog], dialog[open], [aria-modal=true]')].filter(d => d.getBoundingClientRect().height > 100 && d.querySelectorAll('a[href]').length >= 4);
  const scope = dialogs.length ? dialogs[dialogs.length - 1] : document.body;
  for (const p of scope.querySelectorAll('*')) {
    if (p.children.length < 4) continue;
    const counts = {};
    // Two keys per child: tag+classes, and tag+child-tag shape (survives generated/varying
    // class names like Instagram's). The better-scoring grouping wins below.
    for (const c of p.children) {
      const s = sig(c); (counts[s] = counts[s] || []).push(c);
      const shape = 'shape:' + c.tagName.toLowerCase() + '>' + [...c.children].map(k => k.tagName.toLowerCase()).join(',');
      (counts[shape] = counts[shape] || []).push(c);
    }
    for (const [s, els] of Object.entries(counts)) {
      if (els.length < 4) continue;
      const txt = els.slice(0, 8).map(e => (e.innerText || '').trim().length); const avg = txt.reduce((a, b) => a + b, 0) / txt.length;
      const links = els.slice(0, 8).filter(e => e.querySelector('a[href]')).length / Math.min(8, els.length);
      if (avg < 15) continue;
      const score = els.length * Math.min(avg, 400) * (0.5 + links);
      if (!best || score > best.score) best = {score, sig: s, parent: p, els};
    }
  }
  if (!best) return null;
  if (best.sig.startsWith('shape:')) {
    // Report something selectable: classes shared by all items, else the parent's child tag.
    const common = [...best.els[0].classList].filter(c => best.els.every(e => e.classList.contains(c)));
    best.sig = best.els[0].tagName.toLowerCase() + common.slice(0, 2).map(c => '.' + CSS.escape(c)).join('') + ' (class names vary between items; select by shared class or by parent > tag)';
  }
  const prune = (e) => {
    const c = e.cloneNode(true);
    c.querySelectorAll('script,style,svg,noscript,template,iframe').forEach(x => x.remove());
    const keep = ['class', 'href', 'src', 'alt', 'aria-label', 'title', 'datetime', 'content', 'itemprop'];
    const walk = (n) => { for (const a of [...(n.attributes || [])]) if (!keep.includes(a.name) && !(a.name.startsWith('data-') && a.value.length < 40)) n.removeAttribute(a.name);
      if (n.getAttribute && n.getAttribute('class')) n.setAttribute('class', n.getAttribute('class').split(/\s+/).slice(0, 3).join(' '));
      for (const k of n.children) walk(k); };
    walk(c);
    return c.outerHTML.replace(/\s+/g, ' ').replace(/>\s+</g, '><').slice(0, 2500);
  };
  // Compact "path | text | aria | href" lines per item: every value is visible, no markup.
  const leaves = (root) => {
    const out = [], seen = new Set();
    const ds = Object.entries(root.dataset || {}).filter(([k, v]) => v && v.length < 40).map(([k, v]) => `data-${k.replace(/[A-Z]/g, m => '-' + m.toLowerCase())}=${v}`);
    if (ds.length) out.push(':scope | ' + ds.join(' '));
    for (const e of root.querySelectorAll('*')) {
      if (/^(SCRIPT|STYLE|SVG|NOSCRIPT|TEMPLATE|PATH)$/i.test(e.tagName)) continue;
      const own = [...e.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent).join(' ').replace(/\s+/g, ' ').trim();
      const lab = e.getAttribute('aria-label') || e.getAttribute('title') || e.getAttribute('alt') || '';
      const href = e.tagName === 'A' ? (e.getAttribute('href') || '').slice(0, 50) : '';
      if (!own && !lab && !href) continue;
      const path = []; for (let n = e; n && n !== root && path.length < 3; n = n.parentElement) path.unshift(sig(n));
      const line = `${path.join(' > ')} | ${own ? 'text="' + own.slice(0, 70) + '"' : ''}${lab ? ' label="' + lab.slice(0, 70) + '"' : ''}${href ? ' href=' + href : ''}`;
      if (!seen.has(line)) { seen.add(line); out.push(line); }
      if (out.length >= 40) break;
    }
    return out.join('\n');
  };
  const parentSig = (() => { const p = best.parent; return p.id ? '#' + CSS.escape(p.id) : sig(p); })();
  return {item_sig: best.sig, parent_sig: parentSig, count: best.els.length, samples: [0, Math.floor(best.els.length / 2)].map(i => leaves(best.els[i])), html: prune(best.els[0]).slice(0, 1200),
          next: [...document.querySelectorAll('a[href]')].filter(a => /^(next|weiter|›|»|→|next page)$/i.test((a.innerText || a.getAttribute('aria-label') || '').trim()) || a.rel === 'next').slice(0, 3).map(a => ({text: (a.innerText || a.getAttribute('aria-label') || '').trim(), cls: a.className.toString().slice(0, 60), href: a.getAttribute('href')})),
          url: location.href, title: document.title};
})()"#;

/// Applies a recipe on the current page.
pub const APPLY_JS: &str = r#"(r => {
  // Money in code, never by the model's regex (price-tracking lab L4: the recipe's
  // "([0-9.,]+\s*€)" missed every eBay price, which reads "EUR 234,99").
  const MONEY = /(?:€|EUR|US\s?\$|\$|£|CHF)\s?\d[\d.,']*|\d[\d.,']*\s?(?:€|EUR|CHF)/;
  const priceLike = (k) => /price|preis|cost|kosten|betrag|amount/i.test(k || '');
  const val = (root, f, key) => {
    if (!f || (!f.sel && !f.label)) return null;
    let e = null;
    if (f.label) {
      // Label lookup for spec tables / profile stats: the value next to a label element.
      const want = f.label.toLowerCase().replace(/[:\s]+$/, '');
      const lab = [...root.querySelectorAll('th,dt,td,span,div,li,b,strong,label,p')].find(x => (x.childElementCount === 0 || x.tagName === 'TH' || x.tagName === 'DT') && (x.textContent || '').trim().toLowerCase().replace(/[:\s]+$/, '') === want);
      if (lab) {
        const sib = lab.nextElementSibling;
        if (sib) e = sib; else if (lab.parentElement) { const t = lab.parentElement.textContent.replace(lab.textContent, '').trim(); if (t) return t.replace(/\s+/g, ' '); }
      }
    }
    if (!e && f.sel) e = f.sel === ':scope' ? root : root.querySelector(f.sel);
    if (!e) return null;
    const a = f.attr || 'text';
    let v = a === 'text' ? (e.innerText || e.textContent) : a === 'href' ? e.href : a === 'src' ? (e.currentSrc || e.src) : e.getAttribute(a);
    v = (v || '').replace(/\s+/g, ' ').trim();
    // Models often write ([0-9.,]+) and drop "45K"/"1,2 Mio.": let numeric groups take a suffix.
    const fixRe = (re) => re.replace(/\((\[0-9\.,\]\+|\[\\d\.,\]\+|\[\\d,\.\]\+|\\d\+)\)/g, '($1(?:\\s?(?:[KkMm]|Mio\\.?|Tsd\\.?))?)');
    if (priceLike(key)) { const m = v.match(MONEY); if (m) return m[0].trim(); }
    if (f.re) { const m = v.match(new RegExp(fixRe(f.re))); v = m ? (m[1] || m[0]) : ''; }
    // "13.8K" / "1,2 Mio." → integer; plain numbers stay as written.
    if (/^https?%3A/i.test(v)) { try { v = decodeURIComponent(v); } catch (_) {} }
    // "4,956" / "4.956" / "1,234,567" → plain integer (thousands separators only; 57.36 stays).
    if (/^\d{1,3}([.,]\d{3})+$/.test(v)) v = v.replace(/[.,]/g, '');
    const km = v.match(/^\(?([\d.,]+)\s*(K|k|M|Tsd\.?|Mio\.?)\+?\)?$/);
    if (km) { const n = parseFloat(km[1].replace(/,(?=\d{1,2}$)/, '.').replace(/,/g, '')); v = String(Math.round(n * (/^(M|Mio)/.test(km[2]) ? 1e6 : 1e3))); }
    return v || null;
  };
  return [...document.querySelectorAll(r.item)].map(e => { const o = {}; for (const [k, f] of Object.entries(r.fields)) o[k] = val(e, f, k); return o; });
})"#;

/// Offer data a product page states itself: schema.org Product / Offer JSON-LD (eBay and many
/// shops), plus two known gaps (lab L4): Amazon's Buy Box seller row and eBay's "N verkauft".
/// → {price, currency, availability, seller, sold} (missing values null). 0 tokens.
pub const OFFER_JS: &str = r#"(() => {
  const out = {price: null, currency: null, availability: null, seller: null, sold: null};
  for (const s of document.querySelectorAll('script[type="application/ld+json"]')) {
    let j; try { j = JSON.parse(s.textContent); } catch (_) { continue; }
    for (const x of [].concat(j['@graph'] || j)) {
      if (!x || !/Product/.test([].concat(x['@type']).join(' '))) continue;
      const o = [].concat(x.offers || [])[0] || {};
      const oo = o.offers ? [].concat(o.offers)[0] : o;
      out.price = out.price || oo.price || o.lowPrice || null;
      out.currency = out.currency || oo.priceCurrency || o.priceCurrency || null;
      out.availability = out.availability || (oo.availability ? String(oo.availability).replace(/^.*\//, '') : null);
      out.seller = out.seller || (oo.seller && oo.seller.name) || null;
    }
  }
  const t = (sel) => { const e = document.querySelector(sel); return e ? (e.innerText || e.textContent || '').replace(/\s+/g, ' ').trim() : null; };
  if (/amazon\./.test(location.host)) out.seller = out.seller || t('[offer-display-feature-name="desktop-merchant-info"] .offer-display-feature-text') || t('#sellerProfileTriggerId');
  if (/ebay\./.test(location.host)) {
    const m = (document.body.innerText || '').match(/(\d[\d.]*)\s+verkauft/); if (m) out.sold = m[1].replace(/\./g, '');
    out.seller = out.seller || t('.x-sellercard-atf__info__about-seller a') || t('[data-testid="str-title"] a');
  }
  return out;
})()"#;

/// Fills a details item from the page's own offer data: the stated price wins over the recipe
/// (exact, never a misparse); availability / seller / sold only fill gaps.
fn merge_offer(it: &mut Value, offer: &Value, schema: &[String]) {
    let like = |f: &str, words: &[&str]| words.iter().any(|w| f.to_lowercase().contains(w));
    for f in schema {
        let v = if like(f, &["price", "preis", "cost", "kosten"]) {
            offer["price"].as_str().map(|p| p.to_string()).or_else(|| offer["price"].as_f64().map(|p| p.to_string())).map(|p| format!("{p} {}", offer["currency"].as_str().unwrap_or("")).trim().to_string())
        } else if it[f.as_str()].is_null() && like(f, &["availab", "stock", "lager", "verfügbar"]) {
            offer["availability"].as_str().map(str::to_string)
        } else if it[f.as_str()].is_null() && like(f, &["seller", "verkäufer", "merchant", "buy_box", "buybox"]) {
            offer["seller"].as_str().map(str::to_string)
        } else if it[f.as_str()].is_null() && like(f, &["sold", "verkauft"]) {
            offer["sold"].as_str().map(str::to_string)
        } else {
            None
        };
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            it[f.as_str()] = json!(v);
        }
    }
}

/// Scroll the nearest scrollable ancestor of the items (dialogs, panels), else the window.
/// Returns false when nothing could scroll further.
const SCROLL_JS: &str = r#"(sel => {
  const first = document.querySelector(sel);
  let box = null;
  for (let n = first && first.parentElement; n && n !== document.body; n = n.parentElement) {
    const st = getComputedStyle(n);
    if (/(auto|scroll)/.test(st.overflowY) && n.scrollHeight > n.clientHeight + 10) { box = n; break; }
  }
  const items = document.querySelectorAll(sel); const last = items[items.length - 1];
  if (box) { const before = box.scrollTop; box.scrollTop = box.scrollHeight; if (last) last.scrollIntoView({block: 'end'}); return box.scrollTop > before || true; }
  const before = window.scrollY; window.scrollTo(0, document.body.scrollHeight); if (last) last.scrollIntoView({block: 'end'});
  return window.scrollY > before || document.body.scrollHeight > window.innerHeight;
})"#;

/// One page as compact lines for a page-level ("details") recipe: meta tags + JSON-LD keys +
/// leaves of the main content (path | own text | label | href). No markup.
const PAGE_JS: &str = r#"(() => {
  const sig = (e) => e.tagName.toLowerCase() + [...e.classList].filter(c => !/\d{3,}/.test(c)).slice(0, 2).map(c => '.' + CSS.escape(c)).join('');
  const out = [];
  for (const m of document.querySelectorAll('meta[name=description], meta[property^="og:"], meta[name^="twitter:"], meta[itemprop]')) {
    const k = m.getAttribute('name') || m.getAttribute('property') || m.getAttribute('itemprop');
    const v = (m.getAttribute('content') || '').replace(/\s+/g, ' ').slice(0, 220);
    if (v) out.push(`meta[${m.hasAttribute('name') ? 'name' : m.hasAttribute('property') ? 'property' : 'itemprop'}="${k}"] | content="${v}"`);
  }
  out.push(`title | text="${document.title.slice(0, 120)}"`);
  const root = document.querySelector('main') || document.body;
  const seen = new Set();
  for (const e of root.querySelectorAll('*')) {
    if (/^(SCRIPT|STYLE|SVG|NOSCRIPT|TEMPLATE|PATH|NAV|FOOTER)$/i.test(e.tagName) || e.closest('nav,footer')) continue;
    const own = [...e.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent).join(' ').replace(/\s+/g, ' ').trim();
    const lab = e.getAttribute('aria-label') || e.getAttribute('title') || e.getAttribute('alt') || '';
    const href = e.tagName === 'A' ? (e.getAttribute('href') || '').slice(0, 60) : '';
    if (!own && !lab && !href) continue;
    const path = []; for (let n = e; n && n !== root && path.length < 3; n = n.parentElement) path.unshift(sig(n));
    const line = `${path.join(' > ')} | ${own ? 'text="' + own.slice(0, 90) + '"' : ''}${lab ? ' label="' + lab.slice(0, 70) + '"' : ''}${href ? ' href=' + href : ''}`;
    if (!seen.has(line)) { seen.add(line); out.push(line); }
    if (out.length >= 90) break;
  }
  return out.join('\n');
})()"#;

const PAGE_RECIPE_SYS: &str = "You write CSS extraction recipes for ONE kind of detail page (e.g. a product or profile page). Given the page as lines \
(selector-ish path | own text | label | href; meta tags listed as meta[...] | content=...) and a schema, reply ONLY with JSON:\n\
{\"item\": \"html\", \"fields\": {\"<field>\": {\"sel\": \"<CSS selector in the document>\", \"attr\": \"text|href|src|content|<attribute>\", \"re\": \"<optional regex, group 1 kept>\"}}, \"key\": null, \"next\": null, \"scroll\": false}\n\
Prefer meta tags (attr content + regex) and semantic/stable selectors over generated class names; they work on every page of the site. \
For values shown next to a label (spec tables, \"UPC  123\", \"Followers  4.2M\"), use {\"label\": \"<exact label text>\"} instead of a selector (attr/re still apply). \
Browsers insert <tbody> into tables: never write table>tr. \
Regexes must keep magnitude suffixes (e.g. 13.8K, 1.2M). Prefer patterns that do not depend on the UI language (numbers by position, \
structural selectors) when the page is localized. Use null for a field the page doesn't show.";

pub const RECIPE_SYS: &str = "You write CSS extraction recipes. Given repeated page items (pruned HTML) and a schema, reply ONLY with JSON:\n\
{\"item\": \"<CSS selector matching every item on the page>\", \"fields\": {\"<field>\": {\"sel\": \"<CSS relative to the item, or :scope>\", \"attr\": \"text|href|src|<attribute>\", \"re\": \"<optional regex, group 1 kept>\"}}, \"key\": \"<field that uniquely identifies an item>\", \"next\": \"<CSS of the next-page link or null>\", \"scroll\": <true if items load by infinite scroll>}\n\
Prefer stable selectors (semantic tags, data-* / itemprop / aria attributes, meaningful class names); avoid long chains and positional :nth-child. Use :scope when the item itself carries the value (e.g. a data-* id). Values may sit in aria-label/title attributes or visually hidden text; pick attr accordingly. Regexes must keep magnitude suffixes (e.g. 13.8K, 1.2M). If a stable id is available (data-* on the item), add it as the key field only if it is in the schema.";

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Recipe {
    pub item: String,
    pub fields: serde_json::Map<String, Value>,
    pub key: Option<String>,
    pub next: Option<String>,
    #[serde(default)]
    pub scroll: bool,
}

pub async fn run(app: &AppState, tab: &Tab, task: &str, start_url: &str, schema: &[String], catalog: &Catalog, opt: &Options) -> anyhow::Result<Report> {
    let t0 = Instant::now();
    let mut led = Ledger::default();
    let mut tokens = 0u64;
    let rec = |led: &mut Ledger, step: &str, u: Usage, tokens: &mut u64| {
        led.llm_calls.push((step.into(), u.prompt, u.completion));
        *tokens += u.prompt + u.completion;
    };
    tab.goto(start_url, Duration::from_secs(25)).await?;
    ensure_access(app, tab, &mut led, opt).await?;

    // 1. Repeated structure (no LLM). SPAs render lists (and open dialogs) after the load
    // event: poll until the best group is the same twice in a row (max ~8 s).
    let mut g = Value::Null;
    let mut last_sig = String::new();
    for _ in 0..8 {
        g = tab.eval(GROUPS_JS).await?;
        let sig = format!("{}|{}", g["item_sig"], g["count"]);
        if !g.is_null() && sig == last_sig {
            break;
        }
        last_sig = sig;
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    if g.is_null() {
        bail!("no repeated item structure found on {start_url}");
    }
    let domain = artifacts::domain_of(start_url).unwrap_or_default();
    let cache = opt.recipes_dir.join(&domain).join(format!("{}.json", artifacts::slug(&format!("{} {}", g["item_sig"].as_str().unwrap_or(""), schema.join(" ")), 80)));

    // 2. Recipe: cache hit → zero tokens; else one LLM call.
    let cached: Option<Recipe> = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str(&s).ok());
    // Self-heal: a cached recipe that no longer fills the schema is rewritten.
    let cached = match cached {
        Some(r) if weak_fields(&apply(tab, &r).await?, schema).is_empty() => Some(r),
        _ => None,
    };
    let mut recipe: Recipe = match cached {
        Some(r) => {
            led.recipe_cached = true;
            r
        }
        None => {
            let user = format!(
                "Task: {task}\nSchema fields: {}\nItem group signature: {} (parent {}), {} items on this page.\nNext-page link candidates: {}\n{}",
                schema.join(", "),
                g["item_sig"], g["parent_sig"], g["count"], g["next"],
                samples_view(&g)
            );
            let (txt, u) = llm::complete(app, opt.model, RECIPE_SYS, &user, true, 3000).await?;
            rec(&mut led, "recipe", u, &mut tokens);
            serde_json::from_str(&txt).context("recipe json")?
        }
    };

    // 3. Validate on the first page; one repair call if fields come back mostly empty.
    let mut first = apply(tab, &recipe).await?;
    let weak = weak_fields(&first, schema);
    if (first.is_empty() || !weak.is_empty()) && led.repairs == 0 {
        led.repairs += 1;
        let user = format!(
            "This recipe returned {} items; these fields were mostly empty: {:?}.\nRecipe: {}\n{}\nReturn the corrected full recipe JSON.",
            first.len(),
            weak,
            serde_json::to_string(&recipe)?,
            samples_view(&g)
        );
        let (txt, u) = llm::complete(app, opt.model, RECIPE_SYS, &user, true, 3000).await?;
        rec(&mut led, "repair", u, &mut tokens);
        if let Ok(r) = serde_json::from_str::<Recipe>(&txt) {
            let again = apply(tab, &r).await?;
            if again.len() >= first.len() && weak_fields(&again, schema).len() <= weak.len() {
                recipe = r;
                first = again;
            }
        }
    }
    if !first.is_empty() {
        if let Some(d) = cache.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&cache, serde_json::to_string_pretty(&recipe)?);
    }

    // 4. Collect across pages (deterministic; no LLM).
    // Identity: the item's own data-* id (stable across sponsored/tracking copies), else a
    // link (titles repeat), else the model's key, else field 0.
    let field_key = |pred: &dyn Fn(&Value) -> bool| recipe.fields.iter().find(|(k, f)| pred(f) && schema.contains(*k)).map(|(k, _)| k.clone());
    let key = field_key(&|f| f["sel"] == ":scope" && f["attr"].as_str().is_some_and(|a| a.starts_with("data-")))
        .or_else(|| field_key(&|f| f["attr"] == "href"))
        .or_else(|| recipe.key.clone().filter(|k| schema.contains(k)))
        .unwrap_or_else(|| schema[0].clone());
    let mut seen = HashSet::new();
    let mut items: Vec<Value> = Vec::new();
    let mut add = |batch: Vec<Value>, items: &mut Vec<Value>| -> usize {
        let mut n = 0;
        for it in batch {
            let k = it[&key].as_str().map(str::to_string).unwrap_or_else(|| it.to_string());
            if it.as_object().is_some_and(|o| o.values().filter(|v| !v.is_null()).count() >= 2) && seen.insert(k) {
                items.push(it);
                n += 1;
            }
        }
        n
    };
    add(first, &mut items);
    led.pages = 1;
    let mut stale = 0;
    while items.len() < opt.max_items && led.pages < opt.max_pages {
        tokio::time::sleep(Duration::from_millis(opt.page_delay_ms)).await;
        let has_next = recipe.next.as_ref().is_some_and(|n| !n.is_empty());
        let moved = if recipe.scroll || !has_next {
            // Scroll whatever holds the items (a dialog / side panel) or the window. Pages
            // without a next link get one scroll attempt too; `stale` ends it if nothing loads.
            let r = tab.eval(&format!("{SCROLL_JS}({})", json!(recipe.item))).await?;
            tokio::time::sleep(Duration::from_millis(1800)).await;
            r.as_bool() == Some(true)
        } else if let Some(n) = recipe.next.as_ref().filter(|n| !n.is_empty()) {
            let href = tab.eval(&format!("(() => {{ const a = document.querySelector({}); return a ? (a.href || null) : null; }})()", json!(n))).await?;
            match href.as_str() {
                Some(h) if h.starts_with("http") => {
                    tab.goto(h, Duration::from_secs(25)).await?;
                    true
                }
                _ => {
                    let clicked = tab.eval(&format!("(() => {{ const a = document.querySelector({}); if (!a) return false; a.click(); return true; }})()", json!(n))).await?;
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                    clicked.as_bool() == Some(true)
                }
            }
        } else {
            false
        };
        if !moved {
            break;
        }
        ensure_access(app, tab, &mut led, opt).await?;
        led.pages += 1;
        let got = add(apply(tab, &recipe).await?, &mut items);
        stale = if got == 0 { stale + 1 } else { 0 };
        if stale >= 2 {
            break;
        }
    }
    items.truncate(opt.max_items);
    led.items = items.len();

    // 5. Store: items.jsonl + items.csv + recipe + run.json; catalog entry.
    let dir = artifacts::run_dir(&catalog.root, "scrape", task);
    std::fs::create_dir_all(&dir)?;
    let jsonl: String = items.iter().map(|i| format!("{i}\n")).collect();
    std::fs::write(dir.join("items.jsonl"), jsonl)?;
    std::fs::write(dir.join("items.csv"), to_csv(&items, schema))?;
    std::fs::write(dir.join("recipe.json"), serde_json::to_string_pretty(&recipe)?)?;
    let coverage = json!(schema.iter().map(|f| (f.clone(), json!(round2(items.iter().filter(|i| !i[f].is_null()).count() as f64 / items.len().max(1) as f64)))).collect::<serde_json::Map<_, _>>());
    let report = Report { task: task.into(), items: items.len(), fields: schema.to_vec(), coverage, tokens, ledger: led, wall_ms: t0.elapsed().as_millis(), dir: dir.display().to_string(), status: "done".into() };
    std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(&report)?)?;
    let _ = catalog.add("dataset", &format!("{task} ({} items)", report.items), Some(start_url), Some(task), &dir.join("items.csv"));
    Ok(report)
}

async fn apply(tab: &Tab, r: &Recipe) -> anyhow::Result<Vec<Value>> {
    let v = tab.eval(&format!("{APPLY_JS}({})", serde_json::to_string(r)?)).await?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// Two items as leaf listings (`path | text | label | href`) + one short pruned HTML sample.
pub fn samples_view(g: &Value) -> String {
    let leaves: Vec<&str> = g["samples"].as_array().into_iter().flatten().filter_map(|s| s.as_str()).collect();
    format!("Item leaves (CSS path relative to the item | own text | aria-label/title/alt | href):\n{}\nFirst item HTML (truncated):\n{}", leaves.join("\n---\n"), g["html"].as_str().unwrap_or(""))
}

/// Fields that are mostly empty, or that duplicate another field's value on most items (a
/// selector without its regex returns the same raw text for several fields).
pub fn weak_fields(items: &[Value], schema: &[String]) -> Vec<String> {
    if items.is_empty() {
        return schema.to_vec();
    }
    let n = items.len() as f64;
    let mut weak: Vec<String> = schema.iter().filter(|f| (items.iter().filter(|i| !i[f.as_str()].is_null()).count() as f64) / n < 0.6).cloned().collect();
    for (a, fa) in schema.iter().enumerate() {
        for fb in &schema[a + 1..] {
            let same = items.iter().filter(|i| !i[fa.as_str()].is_null() && i[fa.as_str()] == i[fb.as_str()]).count() as f64;
            if same / n >= 0.6 {
                for f in [fa, fb] {
                    if !weak.contains(f) {
                        weak.push(f.clone());
                    }
                }
            }
        }
    }
    weak
}

/// Login walls / blockers: Laya page_state + a password-field heuristic.
/// Default: the user is asked (window on screen) and we wait until the wall is gone.
/// With `auto_login` and stored credentials, the browser agent logs in first.
pub async fn ensure_access(app: &AppState, tab: &Tab, led: &mut Ledger, opt: &Options) -> anyhow::Result<()> {
    let probe = tab
        .eval("({t: document.title, x: (document.body ? document.body.innerText : '').slice(0, 600), pw: !!document.querySelector('input[type=password]'), url: location.href})")
        .await?;
    let text = format!("{}\n{}", probe["t"].as_str().unwrap_or(""), probe["x"].as_str().unwrap_or(""));
    let state = match &app.judge {
        Some(j) => j.ask(json!(text), judge::questions(&["page_state"]), 3000).await.and_then(|a| judge::choice(&a, "page_state")),
        None => None,
    };
    let walled = probe["pw"].as_bool() == Some(true) || state.as_ref().is_some_and(|(s, p)| (s == "login_wall" || s == "captcha") && *p >= 0.7);
    if !walled {
        return Ok(());
    }
    let url = probe["url"].as_str().unwrap_or("").to_string();
    let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    led.login_handoffs += 1;
    if opt.auto_login && crate::secrets::has_login(&host) && probe["pw"].as_bool() == Some(true) {
        let out = super::agent::run(app, tab, "Log in with my stored credentials, then stop with done.", None, &super::agent::Options { max_steps: 6, ..Default::default() }).await?;
        if out.status == "done" {
            return Ok(());
        }
    }
    // Human hand-off: bring the window on screen and wait for the wall to disappear.
    tab.show_window(true).await;
    let ask = format!("Please log in (or complete the check) for {host} in the browser window I just opened; I'll continue automatically once that's done.");
    match &opt.on_user {
        Some(tx) => {
            let _ = tx.send(ask);
        }
        None => eprintln!("\n>>> {ask}"),
    }
    let end = Instant::now() + Duration::from_secs(600);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let p = tab.eval("({pw: !!document.querySelector('input[type=password]'), t: document.title})").await.unwrap_or_default();
        if p["pw"].as_bool() == Some(false) && !p["t"].as_str().unwrap_or("").to_lowercase().contains("log in") {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tab.show_window(false).await;
            if tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).as_deref() != Some(url.as_str()) {
                tab.goto(&url, Duration::from_secs(25)).await?;
            }
            return Ok(());
        }
    }
    bail!("login not completed within 10 minutes")
}

fn to_csv(items: &[Value], schema: &[String]) -> String {
    let esc = |s: &str| if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut out = schema.join(",") + "\n";
    for i in items {
        out += &schema.iter().map(|f| esc(i[f].as_str().unwrap_or(""))).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

pub fn default_recipes_dir(data: &Path) -> PathBuf {
    data.join("browser/recipes")
}

/// Detail pages: one page recipe (from the first URL), applied to every URL. For profiles,
/// product pages, articles. Recipes are cached per (domain, "page", schema).
pub async fn run_details(app: &AppState, tab: &Tab, task: &str, urls: &[String], schema: &[String], catalog: &Catalog, opt: &Options) -> anyhow::Result<Report> {
    let t0 = Instant::now();
    let mut led = Ledger::default();
    let mut tokens = 0u64;
    let urls: Vec<String> = urls.iter().take(opt.max_items).cloned().collect();
    let first_url = urls.first().context("no urls")?.clone();
    let domain = artifacts::domain_of(&first_url).unwrap_or_default();
    let cache = opt.recipes_dir.join(&domain).join(format!("page-{}.json", artifacts::slug(&schema.join(" "), 80)));
    let mut recipe: Option<Recipe> = std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str(&s).ok());
    led.recipe_cached = recipe.is_some();
    let mut items: Vec<Value> = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(opt.page_delay_ms)).await;
        }
        if let Err(e) = tab.goto(url, Duration::from_secs(25)).await {
            items.push(json!({"url": url, "_error": format!("{e:#}")}));
            continue;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        ensure_access(app, tab, &mut led, opt).await?;
        led.pages += 1;
        if recipe.is_none() {
            let page = tab.eval(PAGE_JS).await?;
            let user = format!("Task: {task}\nSchema fields: {}\nPage {url}:\n{}", schema.join(", "), page.as_str().unwrap_or(""));
            let (txt, u) = llm::complete(app, opt.model, PAGE_RECIPE_SYS, &user, true, 2500).await?;
            led.llm_calls.push(("page_recipe".into(), u.prompt, u.completion));
            tokens += u.prompt + u.completion;
            let mut r: Recipe = serde_json::from_str(&txt).context("page recipe json")?;
            r.item = "html".into();
            tracing::debug!(recipe = %txt, "page recipe");
            let got = apply(tab, &r).await?;
            let weak = weak_fields(&got, schema);
            if !weak.is_empty() {
                led.repairs += 1;
                let user = format!("This page recipe left these fields empty: {weak:?}.\nRecipe: {}\nPage {url}:\n{}\nReturn the corrected full recipe JSON (null only if the page truly lacks the value).", serde_json::to_string(&r)?, page.as_str().unwrap_or(""));
                let (txt, u) = llm::complete(app, opt.model, PAGE_RECIPE_SYS, &user, true, 2500).await?;
                led.llm_calls.push(("page_repair".into(), u.prompt, u.completion));
                tokens += u.prompt + u.completion;
                if let Ok(mut r2) = serde_json::from_str::<Recipe>(&txt) {
                    r2.item = "html".into();
                    if weak_fields(&apply(tab, &r2).await?, schema).len() < weak.len() {
                        r = r2;
                    }
                }
            }
            recipe = Some(r);
        }
        if led.recipe_cached && i == 0 && !weak_fields(&apply(tab, recipe.as_ref().unwrap()).await?, schema).is_empty() {
            // The site changed (new class names, other UI language): drop the cached recipe
            // and write a fresh one from this page.
            led.recipe_cached = false;
            let page = tab.eval(PAGE_JS).await?;
            let user = format!("Task: {task}\nSchema fields: {}\nPage {url}:\n{}", schema.join(", "), page.as_str().unwrap_or(""));
            let (txt, u) = llm::complete(app, opt.model, PAGE_RECIPE_SYS, &user, true, 2500).await?;
            led.llm_calls.push(("page_recipe_heal".into(), u.prompt, u.completion));
            tokens += u.prompt + u.completion;
            let mut r: Recipe = serde_json::from_str(&txt).context("page recipe json")?;
            r.item = "html".into();
            recipe = Some(r);
        }
        let r = recipe.as_ref().unwrap();
        let mut it = apply(tab, r).await?.into_iter().next().unwrap_or_else(|| json!({}));
        if let Ok(offer) = tab.eval(OFFER_JS).await {
            merge_offer(&mut it, &offer, schema);
        }
        it["url"] = json!(url);
        items.push(it);
    }
    let mut recipe = recipe.context("no page could be read")?;
    // Second chance for layout variants: a field found on some pages but missing on most was
    // probably written from one layout. Show ONE page where it's missing, ask for a selector
    // list that covers both, and revisit the other gaps only if that page now yields it.
    for f in schema {
        let have: Vec<&Value> = items.iter().filter(|i| !i[f.as_str()].is_null()).collect();
        let cov = have.len() as f64 / items.len().max(1) as f64;
        if have.is_empty() || cov >= 0.6 || led.repairs >= 2 {
            continue;
        }
        let gaps: Vec<usize> = (0..items.len()).filter(|&i| items[i][f.as_str()].is_null() && items[i].get("_error").is_none()).collect();
        let Some(&probe) = gaps.first() else { continue };
        let url = items[probe]["url"].as_str().unwrap_or("").to_string();
        tab.goto(&url, Duration::from_secs(25)).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let page = tab.eval(PAGE_JS).await?;
        let user = format!(
            "Field \"{f}\" was found on other pages of this site (e.g. {}) but is empty on this one, probably a different layout.\nRecipe: {}\nPage {url}:\n{}\nReturn the full recipe JSON with the \"{f}\" selector extended to a comma-separated list that covers both layouts (keep the other fields). Use null for it only if this page truly lacks it.",
            have[0][f.as_str()], serde_json::to_string(&recipe)?, page.as_str().unwrap_or("")
        );
        let (txt, u) = llm::complete(app, opt.model, PAGE_RECIPE_SYS, &user, true, 2500).await?;
        led.llm_calls.push((format!("layout_{f}"), u.prompt, u.completion));
        led.repairs += 1;
        tokens += u.prompt + u.completion;
        let Ok(mut r2) = serde_json::from_str::<Recipe>(&txt) else { continue };
        r2.item = "html".into();
        let got = apply(tab, &r2).await?.into_iter().next().unwrap_or_default();
        if got[f.as_str()].is_null() {
            continue; // the page really has no value: nothing to fix
        }
        items[probe][f.as_str()] = got[f.as_str()].clone();
        for &i in &gaps[1..] {
            tokio::time::sleep(Duration::from_millis(opt.page_delay_ms)).await;
            let url = items[i]["url"].as_str().unwrap_or("").to_string();
            if tab.goto(&url, Duration::from_secs(25)).await.is_err() {
                continue;
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
            led.pages += 1;
            if let Some(v) = apply(tab, &r2).await?.into_iter().next() {
                items[i][f.as_str()] = v[f.as_str()].clone();
            }
        }
        recipe = r2;
    }
    let ok = items.iter().filter(|i| i.get("_error").is_none()).count();
    if ok > 0 && !led.recipe_cached && weak_fields(&items, schema).is_empty() {
        if let Some(d) = cache.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&cache, serde_json::to_string_pretty(&recipe)?);
    }
    led.items = items.len();
    let mut cols = schema.to_vec();
    if !cols.iter().any(|c| c == "url") {
        cols.push("url".into());
    }
    let dir = artifacts::run_dir(&catalog.root, "scrape", task);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("items.jsonl"), items.iter().map(|i| format!("{i}\n")).collect::<String>())?;
    std::fs::write(dir.join("items.csv"), to_csv(&items, &cols))?;
    std::fs::write(dir.join("recipe.json"), serde_json::to_string_pretty(&recipe)?)?;
    let coverage = json!(schema.iter().map(|f| (f.clone(), json!(round2(items.iter().filter(|i| !i[f].is_null()).count() as f64 / items.len().max(1) as f64)))).collect::<serde_json::Map<_, _>>());
    let report = Report { task: task.into(), items: items.len(), fields: schema.to_vec(), coverage, tokens, ledger: led, wall_ms: t0.elapsed().as_millis(), dir: dir.display().to_string(), status: "done".into() };
    std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(&report)?)?;
    let _ = catalog.add("dataset", &format!("{task} ({} pages)", report.items), Some(&first_url), Some(task), &dir.join("items.csv"));
    Ok(report)
}

#[cfg(test)]
mod offer_tests {
    #[test]
    fn offer_fills_price_and_gaps() {
        let mut it = serde_json::json!({"price": null, "availability": null, "seller": "kept"});
        let offer = serde_json::json!({"price": "234.99", "currency": "EUR", "availability": "InStock", "seller": "x", "sold": null});
        super::merge_offer(&mut it, &offer, &["price".into(), "availability".into(), "seller".into()]);
        assert_eq!(it["price"], "234.99 EUR");
        assert_eq!(it["availability"], "InStock");
        assert_eq!(it["seller"], "kept");
    }
}
