//! Gmail on the web, in Sam's own browser screen (the fallback when the thread isn't in
//! Mail.app and no IMAP account is set up). Hooks Gmail has kept for years: search via the
//! `#search/` URL fragment, `tr.zA` rows with `.bog` subjects, `h2.hP` thread subject,
//! `[data-message-id]` messages with `.gD[email]` senders and `.a3s` bodies, the `.ams.bkH`
//! Reply button, the `[g_editable=true]` body, Cmd+Enter to send.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::cdp::Tab;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailMsg {
    pub from: String,
    pub email: String,
    pub date: String,
    pub text: String,
}

fn base() -> String {
    crate::env_or("GMAIL_URL", "https://mail.google.com/mail/u/0/")
}

/// The thread subject from a Gmail window title ("Re: Dinner - me@gmail.com - Gmail — Mozilla Firefox").
pub fn subject_from_title(window: &str) -> Option<String> {
    let w = window.split(" — ").next().unwrap_or(window);
    let re = regex::Regex::new(r"^(.+?) - [^\s]+@[^\s]+ - Gmail").ok()?;
    let s = re.captures(w)?[1].trim().to_string();
    (!s.is_empty() && !["inbox", "posteingang"].contains(&s.to_lowercase().split(" (").next().unwrap_or(""))).then_some(s)
}

fn norm(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = regex::Regex::new(r"^((re|aw|fwd?|wg)\s*:\s*)+").unwrap().replace(&s, "").to_string();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn eval_str(tab: &Tab, js: &str) -> String {
    tab.eval(js).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

async fn click(tab: &Tab, js: &str) -> anyhow::Result<()> {
    let r = tab.eval(&format!("(() => {{ const e = {js}; if (!e) return null; e.scrollIntoView({{block: 'center'}}); const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()")).await?;
    let (x, y) = (r[0].as_f64().context("element not found")?, r[1].as_f64().unwrap_or(0.0));
    for t in ["mouseMoved", "mousePressed", "mouseReleased"] {
        tab.send("Input.dispatchMouseEvent", json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": 1})).await?;
    }
    Ok(())
}

/// Opens Gmail; a Google sign-in page is handed to the user once (window shown, then hidden).
pub async fn ready(tab: &Tab, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<()> {
    let here = eval_str(tab, "location.href").await;
    if !here.starts_with(&base()) {
        tab.goto(&base(), Duration::from_secs(30)).await?;
    }
    let mut asked = false;
    let end = Instant::now() + Duration::from_secs(600);
    while Instant::now() < end {
        let s = tab.eval("({main: !!document.querySelector('div[role=main]'), login: location.host.includes('accounts.google.') || !!document.querySelector('input[type=email], input[type=password]')})").await.unwrap_or_default();
        if s["main"] == true {
            crate::connectors::mark_linked("gmail");
            if asked {
                tab.show_window(false).await;
            }
            return Ok(());
        }
        if s["login"] == true && !asked {
            asked = true;
            tab.show_window(true).await;
            let ask = "Please sign in to Gmail once in the browser window I just opened (Sam's own browser); I'll continue as soon as your inbox shows.".to_string();
            match on_user {
                Some(tx) => {
                    let _ = tx.send(ask);
                }
                None => eprintln!("\n>>> {ask}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(if asked { 2000 } else { 500 })).await;
    }
    bail!("Gmail didn't become ready (not signed in within 10 minutes?)")
}

/// Opens the newest thread whose subject matches; returns the thread's exact subject.
pub async fn open_thread(tab: &Tab, subject: &str) -> anyhow::Result<String> {
    let want = norm(subject);
    if norm(&eval_str(tab, "((document.querySelector('h2.hP') || {}).innerText || '').trim()").await) == want {
        return Ok(eval_str(tab, "document.querySelector('h2.hP').innerText.trim()").await);
    }
    let q = format!("subject:\"{}\"", subject.replace('"', ""));
    tab.eval(&format!("(location.hash = '#search/' + encodeURIComponent({}), true)", json!(q))).await?;
    let end = Instant::now() + Duration::from_secs(12);
    let row = loop {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let subjects: Vec<String> = serde_json::from_value(tab.eval("[...document.querySelectorAll('tr.zA')].map(r => ((r.querySelector('.bog') || {}).innerText || '').trim())").await.unwrap_or_default()).unwrap_or_default();
        if let Some(i) = subjects.iter().position(|s| norm(s) == want) {
            break i;
        }
        if Instant::now() > end {
            bail!("no Gmail thread with subject \"{subject}\" found");
        }
    };
    click(tab, &format!("document.querySelectorAll('tr.zA')[{row}]")).await?;
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let h = eval_str(tab, "((document.querySelector('h2.hP') || {}).innerText || '').trim()").await;
        if norm(&h) == want {
            return Ok(h);
        }
    }
    bail!("the thread \"{subject}\" didn't open")
}

/// Messages of the open thread, oldest first (untrusted data).
pub async fn read(tab: &Tab) -> anyhow::Result<Vec<MailMsg>> {
    let v = tab
        .eval("[...document.querySelectorAll('div[role=main] [data-message-id]')].map(m => { const g = m.querySelector('.gD'); const d = m.querySelector('.g3');
               return {from: g ? (g.getAttribute('name') || g.innerText).trim() : '', email: g ? (g.getAttribute('email') || '') : '', date: d ? (d.getAttribute('title') || d.innerText).trim() : '',
                       text: ((m.querySelector('.a3s') || {}).innerText || '').trim().slice(0, 4000)}; })")
        .await?;
    Ok(serde_json::from_value(v)?)
}

/// Replies in the open thread, only if its subject is the approved one; verifies Gmail's
/// "Message sent" confirmation.
pub async fn reply(tab: &Tab, subject: &str, body: &str) -> anyhow::Result<()> {
    let open = open_thread(tab, subject).await?;
    if norm(&open) != norm(subject) {
        bail!("refusing to send: open thread is \"{open}\", approved was \"{subject}\"");
    }
    let before: usize = tab.eval("document.querySelectorAll('div[role=main] [data-message-id]').length").await?.as_u64().unwrap_or(0) as usize;
    click(tab, "document.querySelector('.ams.bkH, [role=button][data-tooltip^=Reply], [role=button][aria-label^=Reply]')").await.context("Reply button not found")?;
    let end = Instant::now() + Duration::from_secs(8);
    while eval_str(tab, "document.querySelector('[g_editable=true]') ? 'y' : ''").await.is_empty() {
        if Instant::now() > end {
            bail!("reply box didn't open");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    click(tab, "document.querySelector('[g_editable=true]')").await?;
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            // A full keyDown (with text) inserts the line break; rawKeyDown would not.
            tab.send("Input.dispatchKeyEvent", json!({"type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r"})).await?;
            tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13})).await?;
        }
        if !line.is_empty() {
            tab.send("Input.insertText", json!({"text": line})).await?;
        }
    }
    let typed = eval_str(tab, "(document.querySelector('[g_editable=true]') || {}).innerText || ''").await;
    // Compare without whitespace: editors render line breaks as divs / <br>.
    let n = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if !n(&typed).contains(&n(body)) {
        bail!("the reply box holds something else than the approved text; not sent");
    }
    // Cmd+Enter sends in Gmail regardless of the keyboard-shortcut setting.
    tab.send("Input.dispatchKeyEvent", json!({"type": "rawKeyDown", "key": "Enter", "code": "Enter", "modifiers": 4, "windowsVirtualKeyCode": 13})).await?;
    tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "Enter", "code": "Enter", "modifiers": 4, "windowsVirtualKeyCode": 13})).await?;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let sent = eval_str(tab, "((document.querySelector('.bAq') || {}).innerText || '')").await.to_lowercase();
        let count: usize = tab.eval("document.querySelectorAll('div[role=main] [data-message-id]').length").await.ok().and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        if sent.contains("sent") || sent.contains("gesendet") || count > before {
            return Ok(());
        }
    }
    bail!("no \"Message sent\" confirmation from Gmail (it MAY have been sent — check before retrying)")
}

#[cfg(test)]
mod tests {
    #[test]
    fn gmail_titles() {
        assert_eq!(super::subject_from_title("Re: Dinner on Friday - me@gmail.com - Gmail — Mozilla Firefox").as_deref(), Some("Re: Dinner on Friday"));
        assert_eq!(super::subject_from_title("Inbox (3) - me@gmail.com - Gmail"), None);
        assert_eq!(super::subject_from_title("WhatsApp"), None);
    }
}
