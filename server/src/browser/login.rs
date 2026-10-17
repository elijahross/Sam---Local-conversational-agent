//! Signing in to a website with the login saved under Connectors › Website logins (macOS
//! Keychain). Deterministic: no model sees the page or the values. The password goes from the
//! Keychain straight into the field of Sam's own browser through CDP `Input.insertText`; it is
//! never put into a script, a prompt, a log line or a task record.
//!
//! Handles the common shapes: one form with username + password, two-step forms (username,
//! Continue, then password) and a one-time code afterwards when a TOTP seed is saved. Sites
//! that insist on a second device (Google, LinkedIn, Facebook…) still end at the user's own
//! sign-in in Sam's browser window.
use super::cdp::Tab;
use crate::secrets;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    /// Nothing saved for this site: the user signs in themselves.
    NoLogin,
    /// A login is saved but the page shows no sign-in form.
    NoForm,
    /// The form was filled and submitted (the caller checks the site's own signed-in state).
    Submitted,
}

/// Marks the first visible username, password, one-time-code field and submit control
/// (shadow roots included) with `data-sam-login`; returns which of them exist.
const SCAN_JS: &str = r#"(() => {
  const vis = e => { try { const r = e.getBoundingClientRect(); const s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' && !e.disabled; } catch (_) { return false; } };
  const all = []; const walk = r => { for (const e of r.querySelectorAll('input,button,[role=button]')) all.push(e); for (const x of r.querySelectorAll('*')) if (x.shadowRoot) walk(x.shadowRoot); };
  walk(document);
  for (const e of all) e.removeAttribute('data-sam-login');
  const inputs = all.filter(e => e.tagName === 'INPUT' && vis(e));
  const sig = e => ((e.type || '') + ' ' + (e.name || '') + ' ' + (e.id || '') + ' ' + (e.autocomplete || '') + ' ' + (e.placeholder || '') + ' ' + (e.getAttribute('aria-label') || '')).toLowerCase();
  const pw = inputs.find(e => e.type === 'password');
  const otp = inputs.find(e => e.type !== 'password' && /otp|one.?time|2fa|verification|totp|authenticator|\bcode\b/.test(sig(e)));
  const user = inputs.find(e => e !== otp && !['password', 'hidden', 'checkbox', 'radio', 'submit', 'button', 'search', 'file'].includes(e.type) && /user|email|e-mail|login|ident|account|benutzer|mail|phone|tel|name/.test(sig(e)));
  const label = e => (e.innerText || e.value || e.getAttribute('aria-label') || '').trim();
  const submit = all.find(e => e !== user && e !== pw && vis(e) && (e.type === 'submit' || e.tagName === 'BUTTON' || e.getAttribute('role') === 'button') && /log ?in|sign ?in|anmelden|einloggen|continue|weiter|next|submit|fortfahren/i.test(label(e)));
  const mark = (e, k) => { if (e) e.setAttribute('data-sam-login', k); };
  mark(user, 'user'); mark(pw, 'pw'); mark(otp, 'otp'); mark(submit, 'submit');
  return {user: !!user, pw: !!pw, otp: !!otp, submit: !!submit};
})()"#;

/// Deep lookup of a marked element.
const FIND_JS: &str = r#"(k => { const find = r => { const e = r.querySelector('[data-sam-login="' + k + '"]'); if (e) return e; for (const x of r.querySelectorAll('*')) if (x.shadowRoot) { const f = find(x.shadowRoot); if (f) return f; } return null; }; return find(document); })"#;

#[derive(Debug, Default, Clone, Copy)]
struct Form {
    user: bool,
    pw: bool,
    otp: bool,
    submit: bool,
}

async fn scan(tab: &Tab) -> anyhow::Result<Form> {
    let v = tab.eval(SCAN_JS).await?;
    let b = |k: &str| v[k].as_bool().unwrap_or(false);
    Ok(Form { user: b("user"), pw: b("pw"), otp: b("otp"), submit: b("submit") })
}

/// Focus a marked field and type the value with CDP (the value is never part of a script).
async fn fill(tab: &Tab, mark: &str, value: &str) -> anyhow::Result<()> {
    let ok = tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return false; e.scrollIntoView({{block: 'center'}}); e.focus(); if (e.select) e.select(); return true; }})()", json!(mark))).await?;
    anyhow::ensure!(ok.as_bool().unwrap_or(false), "no {mark} field");
    tab.send("Input.insertText", json!({"text": value})).await?;
    tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (e) {{ e.dispatchEvent(new Event('input', {{bubbles: true}})); e.dispatchEvent(new Event('change', {{bubbles: true}})); }} return true; }})()", json!(mark))).await?;
    Ok(())
}

/// The form's submit control, or Enter in the field that was just filled.
async fn submit(tab: &Tab, f: Form, last: &str) -> anyhow::Result<()> {
    if f.submit {
        let clicked = tab.eval(&format!("(() => {{ const e = {FIND_JS}('submit'); if (!e) return false; e.click(); return true; }})()")).await?;
        if clicked.as_bool().unwrap_or(false) {
            return Ok(());
        }
    }
    tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (e) e.focus(); return true; }})()", json!(last))).await?;
    for t in ["keyDown", "keyUp"] {
        let mut p = json!({"type": t, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13});
        if t == "keyDown" {
            p["text"] = Value::String("\r".into());
        }
        tab.send("Input.dispatchKeyEvent", p).await?;
    }
    Ok(())
}

async fn wait_for(tab: &Tab, want: impl Fn(Form) -> bool, secs: u64) -> Form {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let f = scan(tab).await.unwrap_or_default();
        if want(f) || std::time::Instant::now() > end {
            return f;
        }
    }
}

/// Sign in on the page the tab shows, with the login saved for `host`'s site.
pub async fn sign_in(tab: &Tab, host: &str) -> anyhow::Result<SignIn> {
    let (Some(user), Some(pass)) = (secrets::get(host, "username"), secrets::get(host, "password")) else {
        return Ok(SignIn::NoLogin);
    };
    let f = scan(tab).await?;
    if !f.user && !f.pw {
        return Ok(SignIn::NoForm);
    }
    tracing::info!(site = %secrets::site(host), two_step = f.user && !f.pw, "signing in with the saved login");
    if f.user {
        fill(tab, "user", &user).await?;
    }
    let f = if f.pw {
        f
    } else {
        // Two-step: the password field appears after the username is accepted.
        submit(tab, f, "user").await?;
        let f2 = wait_for(tab, |x| x.pw, 10).await;
        if !f2.pw {
            return Ok(SignIn::NoForm);
        }
        f2
    };
    fill(tab, "pw", &pass).await?;
    submit(tab, f, "pw").await?;
    // A one-time code afterwards, when a seed is saved.
    let f3 = wait_for(tab, |x| x.otp || (!x.pw && !x.user), 8).await;
    if f3.otp {
        if let Some(code) = secrets::get(host, "totp") {
            fill(tab, "otp", &code).await?;
            submit(tab, f3, "otp").await?;
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    Ok(SignIn::Submitted)
}
