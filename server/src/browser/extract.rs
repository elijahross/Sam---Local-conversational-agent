//! Page → clean text, never raw HTML to a model.
//!
//! Static first: a plain HTTP fetch + readability (dom_smoothie → markdown) handles most
//! article/docs pages without a browser. Pages that come back as JS shells, consent/bot
//! challenges or too little text are rendered in a tab and pruned *inside the page* by
//! `PRUNE_JS`, so only the main text crosses the wire.

use std::time::Duration;

use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub text: String,
    /// "static" | "rendered"
    pub mode: &'static str,
    pub html_bytes: usize,
}

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
pub const MIN_STATIC_CHARS: usize = 800;
const MAX_HTML: usize = 3_000_000;
pub const MAX_TEXT: usize = 40_000;

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(6))
        .gzip(true)
        .deflate(true)
        .build()
        .expect("http client")
}

/// Static attempt. `Err` = needs rendering (or unreachable): the caller decides.
pub async fn fetch_static(http: &reqwest::Client, url: &str) -> Result<Page, String> {
    let r = http
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| format!("fetch: {e}"))?;
    let status = r.status();
    let final_url = r.url().to_string();
    let ctype = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let is_pdf = ctype.contains("pdf") || final_url.to_lowercase().ends_with(".pdf");
    if !ctype.is_empty() && !ctype.contains("html") && !is_pdf {
        return Err(format!("not html: {ctype}"));
    }
    let bytes = r.bytes().await.map_err(|e| format!("body: {e}"))?;
    if is_pdf {
        // PDFs (timetables, manuals, papers): text layer only, capped; no rendering needed.
        if bytes.len() > 15_000_000 {
            return Err("pdf too large".into());
        }
        let b = bytes.to_vec();
        let text = tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&b)).await.map_err(|e| e.to_string())?.map_err(|e| format!("pdf: {e}"))?;
        let text = clean_markdown(&text);
        if text.len() < 300 {
            return Err("pdf has no text layer".into());
        }
        let title = text.lines().find(|l| l.trim().len() > 8).unwrap_or("PDF").trim().chars().take(120).collect();
        return Ok(Page { url: clean_url(&final_url), title, text: cap(&text, MAX_TEXT), mode: "pdf", html_bytes: bytes.len() });
    }
    if status.as_u16() >= 400 {
        return Err(format!("http {status}"));
    }
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_HTML)]).to_string();
    if is_challenge(&html) {
        return Err("challenge or js shell".into());
    }
    let page = readable(&html, &final_url).ok_or("readability failed")?;
    if page.text.len() < MIN_STATIC_CHARS {
        return Err(format!("too little text ({} chars)", page.text.len()));
    }
    Ok(Page { html_bytes: bytes.len(), ..page })
}

pub fn readable(html: &str, url: &str) -> Option<Page> {
    let cfg = dom_smoothie::Config { text_mode: dom_smoothie::TextMode::Markdown, ..Default::default() };
    let mut r = dom_smoothie::Readability::new(html, Some(url), Some(cfg)).ok()?;
    let a = r.parse().ok()?;
    let text = clean_markdown(&a.text_content);
    Some(Page { url: clean_url(url), title: a.title.trim().to_string(), text: cap(&text, MAX_TEXT), mode: "static", html_bytes: html.len() })
}

/// Bot walls, consent interstitials and empty SPA shells.
pub fn is_challenge(s: &str) -> bool {
    let head = &s[..s.len().min(20_000)].to_lowercase();
    ["just a moment...", "cf-chl", "challenge-platform", "prove your humanity", "enable javascript to", "please enable js",
     "you need to enable javascript", "are you a robot", "captcha-delivery", "/sorry/index", "access denied"]
        .iter()
        .any(|p| head.contains(p))
}

/// Runs inside a rendered page: drop chrome (nav/header/footer/aside/forms/hidden), pick the
/// densest content root, emit lightweight markdown. Returns {title, url, text}.
pub const PRUNE_JS: &str = r#"(() => {
  const root = document.body; if (!root) return {title: document.title, url: location.href, text: ''};
  const clone = root.cloneNode(true);
  clone.querySelectorAll('script,style,noscript,svg,canvas,iframe,form,nav,header,footer,aside,dialog,template,button,input,select,[aria-hidden="true"],[hidden],[role="navigation"],[role="banner"],[role="contentinfo"],[role="dialog"]').forEach(e => e.remove());
  clone.querySelectorAll('[class*="cookie" i],[id*="cookie" i],[class*="consent" i],[id*="consent" i],[class*="newsletter" i],[class*="advert" i],[class*="share" i],[class*="related" i]').forEach(e => e.remove());
  const cands = [...clone.querySelectorAll('main,article,[role="main"],#content,.content,.post,.article')];
  let best = clone, bestLen = 0;
  for (const c of cands) { const l = (c.innerText || c.textContent || '').length; if (l > bestLen) { best = c; bestLen = l; } }
  if (bestLen < 500) best = clone;
  const out = [];
  const walk = (n) => {
    for (const c of n.children) {
      const t = c.tagName;
      if (/^H[1-4]$/.test(t)) { const s = c.textContent.trim(); if (s) out.push('#'.repeat(+t[1]) + ' ' + s); }
      else if (t === 'P' || t === 'BLOCKQUOTE' || t === 'PRE' || t === 'TD') { const s = c.textContent.replace(/\s+/g, ' ').trim(); if (s.length > 1) out.push(s); }
      else if (t === 'LI') { const s = c.textContent.replace(/\s+/g, ' ').trim(); if (s) out.push('- ' + s); }
      else if (c.children.length) walk(c);
      else { const s = (c.textContent || '').replace(/\s+/g, ' ').trim(); if (s.length > 40) out.push(s); }
    }
  };
  walk(best);
  return {title: document.title, url: location.href, text: out.join('\n\n').slice(0, 60000)};
})()"#;

/// RawKit clean_markdown: entities, control chars, trailing spaces, blank-line runs.
pub fn clean_markdown(s: &str) -> String {
    static RE: std::sync::OnceLock<(Regex, Regex, Regex)> = std::sync::OnceLock::new();
    let (ctrl, trail, blank) = RE.get_or_init(|| {
        (Regex::new(r"[\u{0000}-\u{0008}\u{000B}\u{000C}\u{000E}-\u{001F}\u{200B}-\u{200D}\u{FEFF}]").unwrap(), Regex::new(r"[ \t]+\n").unwrap(), Regex::new(r"\n{3,}").unwrap())
    });
    let s = s.replace("&nbsp;", " ").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    let s = ctrl.replace_all(&s, "");
    let s = trail.replace_all(&s, "\n");
    blank.replace_all(&s, "\n\n").trim().to_string()
}

/// RawKit clean_url: drop tracking params (≈400 chars saved per URL in their runs).
pub fn clean_url(u: &str) -> String {
    let Ok(mut url) = url::Url::parse(u) else { return u.to_string() };
    let keep: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !(k.starts_with("utm_") || k.starts_with("_hs") || k.starts_with("mc_") || k.starts_with("pk_")
                || ["fbclid", "gclid", "dclid", "msclkid", "yclid", "ref", "ref_src", "igshid", "si", "spm", "_ga", "_gl", "sei", "ved", "usg", "sa", "oq", "gs_lcp", "sca_esv", "ei"]
                    .contains(&k.as_str()))
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    url.set_fragment(None);
    if keep.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(keep);
    }
    url.to_string()
}

/// Head/tail clamp with an explicit marker (RawKit `clamp`), so a cap never silently lies.
pub fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = floor_char(s, max * 8 / 10);
    let tail_start = ceil_char(s, s.len() - (max - head));
    format!("{}\n…[{} chars omitted]…\n{}", &s[..head], tail_start - head, &s[tail_start..])
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Paragraph-packed chunks of ~`size` chars for embedding-based relevance.
pub fn chunks(text: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for p in text.split("\n\n").map(str::trim).filter(|p| p.len() > 30) {
        if cur.len() + p.len() > size && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(&p[..floor_char(p, p.len().min(size * 2))]);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_text_cleaning() {
        assert_eq!(clean_url("https://x.com/a?utm_source=g&id=5&gclid=9#top"), "https://x.com/a?id=5");
        assert_eq!(clean_url("https://x.com/a?utm_source=g"), "https://x.com/a");
        assert_eq!(clean_markdown("a&amp;b  \n\n\n\nc"), "a&b\n\nc");
        assert!(cap(&"x".repeat(100), 50).contains("omitted"));
        assert!(is_challenge("<title>Just a moment...</title>"));
        let c = chunks(&format!("{}\n\n{}", "a".repeat(500), "b".repeat(500)), 600);
        assert_eq!(c.len(), 2);
    }
}
