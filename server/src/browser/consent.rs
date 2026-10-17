//! Cookie / consent walls without a vision model and without an LLM.
//!
//! Order (cheapest first):
//! 1. pre-seeded consent cookies for known gatekeepers (Google's EU consent redirect);
//! 2. known CMP buttons (OneTrust, Cookiebot, Didomi, Usercentrics, Quantcast, TrustArc,
//!    Sourcepoint-in-page, Borlabs, Complianz, Google) — "reject optional" preferred;
//! 3. visible buttons with consent wording inside a cookie/consent container; if more than
//!    one plausible choice, Laya picks (`relevant`, goal = close the dialog) — no screenshots;
//! 4. verify; if still covered, hide fixed overlays and unlock scrolling.
//! Reading text rarely needs any of this (overlays don't remove the DOM text), so the
//! research flow only calls it when a page is gated.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use super::cdp::Tab;
use crate::judge::{self, Judge};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Outcome {
    pub present: bool,
    /// "none" | "known:<selector>" | "text:<button text>" | "laya:<button text>" | "hidden"
    pub action: String,
    pub cleared: bool,
    pub laya_calls: u32,
}

/// Cookies that answer known consent gates before the page loads.
pub fn seed_cookies() -> Value {
    json!([
        // Google EU consent ("Reject all" equivalent); avoids the consent.google.com redirect.
        {"name": "SOCS", "value": "CAESHAgBEhJnd3NfMjAyMzA4MTAtMF9SQzIaAmRlIAEaBgiAo_CmBg", "domain": ".google.com", "path": "/", "secure": true, "sameSite": "Lax"},
        {"name": "CONSENT", "value": "PENDING+987", "domain": ".google.com", "path": "/", "secure": true},
        {"name": "SOCS", "value": "CAESHAgBEhJnd3NfMjAyMzA4MTAtMF9SQzIaAmRlIAEaBgiAo_CmBg", "domain": ".youtube.com", "path": "/", "secure": true, "sameSite": "Lax"}
    ])
}

/// Scans the page (incl. open shadow roots and same-origin iframes). Tags candidates with
/// data-sam-c so a click needs no selector round-trip.
const SCAN_JS: &str = r#"(() => {
  const KNOWN_REJECT = ['#onetrust-reject-all-handler','#CybotCookiebotDialogBodyButtonDecline','#didomi-notice-disagree-button','button[data-testid="uc-deny-all-button"]','.qc-cmp2-summary-buttons button[mode="secondary"]','#truste-consent-required','button[aria-label="Reject all"]','button[aria-label="Alle ablehnen"]','a[data-cookie-refuse]','.cmplz-deny','button.sp_choice_type_REJECT_ALL','#cookie-reject','[data-action="reject"]'];
  const KNOWN_ACCEPT = ['#onetrust-accept-btn-handler','#CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll','#CybotCookiebotDialogBodyButtonAccept','#didomi-notice-agree-button','button[data-testid="uc-accept-all-button"]','.qc-cmp2-summary-buttons button[mode="primary"]','#truste-consent-button','button[aria-label="Accept all"]','button[aria-label="Alle akzeptieren"]','a[data-cookie-accept]','.cmplz-accept','button.sp_choice_type_11','#cookie-accept','[data-action="accept"]'];
  const REJECT = /\b(reject( all)?|decline|deny|refuse|only (necessary|essential)|necessary only|essential only|ablehnen|alle ablehnen|nur (notwendige|erforderliche|essenzielle)|refuser|tout refuser|rechazar|rifiuta|weiger)\b/i;
  const ACCEPT = /\b(accept( all)?|agree|allow( all)?|i agree|got it|ok(ay)?|akzeptieren|alle akzeptieren|zustimmen|einverstanden|annehmen|accepter|tout accepter|aceptar|accetta|accepteren)\b/i;
  const CTX = /cookie|consent|privacy|datenschutz|tracking|gdpr|dsgvo|personali[sz]|zustimm|einwilligung/i;
  const roots = [document];
  const collect = (r) => { r.querySelectorAll('*').forEach(e => { if (e.shadowRoot) { roots.push(e.shadowRoot); collect(e.shadowRoot); } }); };
  collect(document);
  document.querySelectorAll('iframe').forEach(f => { try { if (f.contentDocument) roots.push(f.contentDocument); } catch (_) {} });
  const visible = (e) => { const r = e.getBoundingClientRect(); const s = getComputedStyle(e); return r.width > 2 && r.height > 2 && s.visibility !== 'hidden' && s.display !== 'none' && +s.opacity > 0.05; };
  const hit = (sels) => { for (const r of roots) for (const s of sels) { const e = r.querySelector(s); if (e && visible(e)) return [s, e]; } return null; };
  const known = hit(KNOWN_REJECT) || hit(KNOWN_ACCEPT);
  let n = 0; const cands = [];
  for (const r of roots) r.querySelectorAll('button,a,[role="button"],input[type="button"],input[type="submit"]').forEach(e => {
    if (!visible(e)) return;
    const t = (e.innerText || e.value || e.getAttribute('aria-label') || '').trim().replace(/\s+/g, ' ');
    if (!t || t.length > 60 || !(REJECT.test(t) || ACCEPT.test(t))) return;
    let p = e, ctx = '';
    for (let k = 0; k < 6 && p; k++) { p = p.parentElement || (p.getRootNode && p.getRootNode().host); if (p && p.innerText && CTX.test(p.innerText.slice(0, 3000))) { ctx = p.innerText.slice(0, 160).replace(/\s+/g, ' '); break; } }
    if (!ctx && !CTX.test(location.host)) return;
    e.setAttribute('data-sam-c', String(n));
    cands.push({i: n++, text: t, kind: REJECT.test(t) ? 'reject' : 'accept', ctx});
  });
  if (known) known[1].setAttribute('data-sam-c', 'known');
  const covered = [...document.querySelectorAll('body *')].some(e => { const s = getComputedStyle(e); if (s.position !== 'fixed' && s.position !== 'sticky') return false; const r = e.getBoundingClientRect(); return r.height > innerHeight * 0.2 && CTX.test((e.innerText || '').slice(0, 2000)); }) || location.host.startsWith('consent.');
  return {present: !!known || cands.length > 0 || covered, known: known ? known[0] : null, cands};
})()"#;

const CLICK_JS: &str = r#"(id => {
  const find = (r) => { const e = r.querySelector(`[data-sam-c="${id}"]`); if (e) return e; for (const x of r.querySelectorAll('*')) if (x.shadowRoot) { const f = find(x.shadowRoot); if (f) return f; } return null; };
  let e = find(document); if (!e) document.querySelectorAll('iframe').forEach(f => { try { e = e || find(f.contentDocument); } catch (_) {} });
  if (!e) return false; e.click(); return true; })"#;

const HIDE_JS: &str = r#"(() => { const CTX = /cookie|consent|privacy|datenschutz|tracking|gdpr|einwilligung/i; let n = 0;
  document.querySelectorAll('body *').forEach(e => { const s = getComputedStyle(e); if ((s.position === 'fixed' || s.position === 'sticky') && +s.zIndex > 10 && CTX.test((e.innerText || '').slice(0, 2000))) { e.remove(); n++; } });
  for (const el of [document.documentElement, document.body]) { el.style.setProperty('overflow', 'auto', 'important'); el.style.setProperty('position', 'static', 'important'); }
  return n; })()"#;

async fn scan(tab: &Tab) -> Value {
    tab.eval(SCAN_JS).await.unwrap_or(json!({"present": false}))
}

async fn click(tab: &Tab, id: &str) -> bool {
    tab.eval(&format!("{CLICK_JS}({})", json!(id))).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Clear a consent wall on the current page. Never asks an LLM; at most one Laya batch.
pub async fn dismiss(tab: &Tab, judge: Option<&Judge>) -> Outcome {
    let mut out = Outcome { action: "none".into(), ..Default::default() };
    for _round in 0..3 {
        let s = scan(tab).await;
        if !s["present"].as_bool().unwrap_or(false) {
            out.cleared = true;
            return out;
        }
        out.present = true;
        let cands = s["cands"].as_array().cloned().unwrap_or_default();
        let choice: Option<(String, String)> = if let Some(sel) = s["known"].as_str() {
            Some(("known".into(), format!("known:{sel}")))
        } else if let Some(c) = cands.iter().find(|c| c["kind"] == "reject").or_else(|| (cands.len() == 1).then(|| &cands[0])) {
            Some((c["i"].to_string(), format!("text:{}", c["text"].as_str().unwrap_or(""))))
        } else if let (Some(j), false) = (judge, cands.is_empty()) {
            // Several accept-style buttons: Laya scores which one closes the dialog.
            out.laya_calls += 1;
            let goal = "Close the cookie consent dialog so the page can be used";
            let items = cands.iter().map(|c| (json!({"goal": goal, "element": format!("button {}", c["text"].as_str().unwrap_or(""))}), judge::questions(&["relevant"]))).collect();
            let scores = j.batch(items, 10_000).await.unwrap_or_default();
            scores
                .iter()
                .zip(&cands)
                .filter_map(|(a, c)| judge::noul(a, "relevant").map(|p| (p, c)))
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, c)| (c["i"].to_string(), format!("laya:{}", c["text"].as_str().unwrap_or(""))))
        } else {
            cands.first().map(|c| (c["i"].to_string(), format!("text:{}", c["text"].as_str().unwrap_or(""))))
        };
        match choice {
            Some((id, action)) if click(tab, &id).await => {
                out.action = action;
                tokio::time::sleep(Duration::from_millis(900)).await;
            }
            _ => break,
        }
    }
    if !scan(tab).await["present"].as_bool().unwrap_or(false) {
        out.cleared = true;
        return out;
    }
    let n = tab.eval(HIDE_JS).await.ok().and_then(|v| v.as_u64()).unwrap_or(0);
    if n > 0 {
        out.action = "hidden".into();
        out.cleared = !scan(tab).await["present"].as_bool().unwrap_or(false) || n > 0;
    }
    out
}
