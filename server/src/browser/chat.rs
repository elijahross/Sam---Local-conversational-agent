//! Chat apps in Sam's own browser screen (WhatsApp Web, Telegram Web K): open a chat by name, read the
//! recent messages, send a reply. The user's own window is never touched: Sam has its own
//! linked session (one-time QR link, persisted in the screen's profile) off-screen.
//!
//! Selectors use the hooks WhatsApp Web has kept for years (`#pane-side`, `#main header`,
//! `[data-id]` true_/false_ rows, `[data-pre-plain-text]`, the footer textbox). Every send is
//! guarded: the chat header must match the approved chat, and the message must show up as
//! outgoing afterwards.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};

use super::cdp::Tab;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ChatMsg {
    pub from: String,
    pub time: String,
    pub outgoing: bool,
    pub text: String,
}

/// A supported chat site. URLs overridable for tests (`CHAT_URL_WHATSAPP`, `CHAT_URL_TELEGRAM`).
pub fn site_url(site: &str) -> Option<String> {
    match site {
        "whatsapp" => Some(crate::env_or("CHAT_URL_WHATSAPP", "https://web.whatsapp.com/")),
        "telegram" => Some(crate::env_or("CHAT_URL_TELEGRAM", "https://web.telegram.org/k/")),
        "slack" => Some(crate::env_or("CHAT_URL_SLACK", "https://app.slack.com/client")),
        _ => None,
    }
}

/// Which chat site a window is, from its title / app / URL ("WhatsApp", "Telegram Web", a URL).
pub fn detect_site(window: &str) -> Option<&'static str> {
    let w = window.to_lowercase();
    if w.contains("whatsapp") {
        Some("whatsapp")
    } else if w.contains("telegram") {
        Some("telegram")
    } else if w.contains("slack") {
        Some("slack")
    } else {
        None
    }
}

/// The page hooks of one site (JS expressions). `{t}` = a JSON string, `{n}` = a count.
struct Hooks {
    name: &'static str,
    /// → {list: bool, qr: bool}
    ready: &'static str,
    search: &'static str,
    titles: &'static str,
    row: &'static str,
    header: &'static str,
    /// → [{from, time, outgoing, text}] (last {n})
    messages: &'static str,
    composer: &'static str,
    link_help: &'static str,
}

fn hooks(site: &str) -> &'static Hooks {
    match site {
        "telegram" => &TELEGRAM,
        "slack" => &SLACK,
        _ => &WHATSAPP,
    }
}

static WHATSAPP: Hooks = Hooks {
    name: "WhatsApp",
    ready: "({list: !!document.querySelector('#pane-side'), qr: !!document.querySelector('canvas[aria-label], [data-ref] canvas')})",
    search: "document.querySelector('#side [contenteditable=true], #side input[type=text], #side [role=textbox]')",
    titles: "[...document.querySelectorAll('#pane-side span[title]')].map(e => e.getAttribute('title'))",
    row: "[...document.querySelectorAll('#pane-side span[title]')].find(e => e.getAttribute('title') === {t})",
    header: "(() => { const h = document.querySelector('#main header'); if (!h) return ''; const t = h.querySelector('span[title], span[dir=auto]'); return (t && (t.getAttribute('title') || t.textContent) || '').trim(); })()",
    messages: "[...document.querySelectorAll('#main [data-pre-plain-text]')].slice(-{n}).map(e => {
       const meta = e.getAttribute('data-pre-plain-text') || '';
       const m = meta.match(/^\\[(.*?)\\]\\s*(.*?):\\s*$/) || [];
       const row = e.closest('[data-id]');
       const out = !!(row && row.getAttribute('data-id').startsWith('true_')) || !!e.closest('.message-out');
       const t = e.querySelector('.selectable-text') || e;
       return {from: m[2] || '', time: m[1] || '', outgoing: out, text: (t.innerText || '').trim()};
     })",
    composer: "document.querySelector('#main footer [contenteditable=true], #main footer [role=textbox]')",
    link_help: "on your phone open WhatsApp › Settings › Linked devices › Link a device, and scan the code",
};

/// Telegram Web K (web.telegram.org/k): `.chatlist-chat` rows with `.peer-title`, the
/// `.chat-info` header, `.bubble` messages (`is-in` / `is-out`), the `.input-message-input` box.
static TELEGRAM: Hooks = Hooks {
    name: "Telegram",
    ready: "({list: !!document.querySelector('.chatlist .chatlist-chat, #chatlist-container .chatlist'), qr: !!document.querySelector('canvas.qr-canvas, .auth-pages canvas, #auth-pages canvas')})",
    search: "document.querySelector('#column-left input.input-search-input, .input-search input')",
    titles: "[...document.querySelectorAll('.chatlist-chat .peer-title')].map(e => e.innerText.trim())",
    row: "[...document.querySelectorAll('.chatlist-chat')].find(r => ((r.querySelector('.peer-title') || {}).innerText || '').trim() === {t})",
    header: "((document.querySelector('.chat-info .peer-title, .topbar .peer-title') || {}).innerText || '').trim()",
    messages: "(() => { const chat = ((document.querySelector('.chat-info .peer-title') || {}).innerText || '').trim();
       return [...document.querySelectorAll('.bubbles-inner .bubble')].filter(b => b.querySelector('.message')).slice(-{n}).map(b => {
         const m = b.querySelector('.message').cloneNode(true); m.querySelectorAll('.time, .reactions, .bubble-name-first, .name').forEach(x => x.remove());
         const name = b.querySelector('.name .peer-title, .bubble-name .peer-title'); const t = b.querySelector('.time .time-inner, .time');
         const out = b.classList.contains('is-out');
         return {from: name ? name.innerText.trim() : (out ? 'me' : chat), time: t ? (t.getAttribute('title') || t.innerText).trim() : '', outgoing: out, text: m.innerText.trim()};
       }); })()",
    composer: "document.querySelector('.input-message-input[contenteditable=true]:not(.input-field-input-fake)')",
    link_help: "on your phone open Telegram › Settings › Devices › Link Desktop Device, and scan the code",
};

/// Slack in the browser (app.slack.com/client, signed in once in Sam's browser; the desktop
/// app exposes no Accessibility tree): sidebar names, the view header, `message_container`s
/// (sender id → own messages via the signed-in user ids), the `message_input` box (Enter sends).
static SLACK: Hooks = Hooks {
    name: "Slack",
    ready: "({list: !!document.querySelector('[data-qa=channel_sidebar], .p-channel_sidebar'), qr: location.hostname.endsWith('slack.com') && location.hostname !== 'app.slack.com' || !!document.querySelector('input[type=email], [data-qa=signin_domain_input]')})",
    search: "document.querySelector('[data-qa=channel-sidebar-search] input, [data-qa=channel_sidebar] input[type=text], .p-channel_sidebar input, input[placeholder*=\"nterhaltung\"], input[placeholder*=\"onversation\"]')",
    titles: "[...document.querySelectorAll('[data-qa=channel_sidebar_name_button], .p-channel_sidebar__name')].map(e => e.innerText.trim().replace(/^#\\s*/, ''))",
    row: "[...document.querySelectorAll('[data-qa=channel_sidebar_name_button], .p-channel_sidebar__name')].find(e => e.innerText.trim().replace(/^#\\s*/, '') === {t})",
    header: "((document.querySelector('[data-qa=channel_name_button], [data-qa=channel_header_name], .p-view_header__channel_title') || {}).innerText || '').trim().replace(/^#\\s*/, '')",
    messages: "(() => { let me = []; try { me = Object.values((JSON.parse(localStorage.localConfig_v2 || '{}').teams) || {}).map(t => t.user_id); } catch (e) {}
       if (window.__samSlackMe) me.push(window.__samSlackMe);
       let from = '', fromId = '';
       return [...document.querySelectorAll('[data-qa=message_container]')].map(c => {
         const s = c.querySelector('[data-qa=message_sender_name]');
         if (s) { from = s.innerText.trim(); fromId = s.getAttribute('data-message-sender') || ''; }
         const t = c.querySelector('[data-qa=message-text], .p-rich_text_section, .c-message__body');
         const ts = c.querySelector('[data-qa=timestamp_label], .c-timestamp');
         return {from, time: ts ? (ts.getAttribute('aria-label') || ts.innerText || '').trim() : '', outgoing: me.includes(fromId), text: t ? t.innerText.trim() : ''};
       }).filter(m => m.text).slice(-{n}); })()",
    composer: "document.querySelector('[data-qa=message_input] [contenteditable=true], .ql-editor[contenteditable=true]')",
    link_help: "sign in to your Slack workspace",
};

/// Opens the site in `tab` and waits until the chat list is there. A QR screen means Sam's
/// browser isn't linked yet: the window is shown and the user asked (via `on_user`) to link it.
pub async fn ready(tab: &Tab, site: &str, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<()> {
    let url = site_url(site).with_context(|| format!("unsupported chat site {site}"))?;
    let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if !here.starts_with(url.split('?').next().unwrap_or(&url)) {
        tab.goto(&url, Duration::from_secs(30)).await?;
    }
    let mut asked = false;
    let end = Instant::now() + Duration::from_secs(600);
    while Instant::now() < end {
        let s = tab.eval(hooks(site).ready).await.unwrap_or_default();
        if s["list"] == true {
            crate::connectors::mark_linked(site);
            if asked {
                tab.show_window(false).await;
            }
            return Ok(());
        }
        if s["qr"] == true && !asked {
            asked = true;
            tab.show_window(true).await;
            let h = hooks(site);
            let ask = format!("Sam needs its own {} link once: {} in the browser window I just opened. I'll continue as soon as it's linked.", h.name, h.link_help);
            match on_user {
                Some(tx) => {
                    let _ = tx.send(ask);
                }
                None => eprintln!("\n>>> {ask}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(if asked { 2000 } else { 500 })).await;
    }
    bail!("{} didn't become ready (not linked within 10 minutes?)", hooks(site).name)
}

pub(crate) async fn click_at(tab: &Tab, x: f64, y: f64) -> anyhow::Result<()> {
    for t in ["mouseMoved", "mousePressed", "mouseReleased"] {
        tab.send("Input.dispatchMouseEvent", json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": 1})).await?;
    }
    Ok(())
}

/// Centre of the first element matching `js` (an expression yielding an Element or null).
pub(crate) async fn center(tab: &Tab, js: &str) -> Option<(f64, f64)> {
    let r = tab.eval(&format!("(() => {{ const e = {js}; if (!e) return null; e.scrollIntoView({{block: 'center'}}); const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()")).await.ok()?;
    Some((r[0].as_f64()?, r[1].as_f64()?))
}

/// Empties the focused text box like a user would: select all (Chrome's editing command; a
/// synthetic Cmd+A doesn't select on macOS), then Backspace.
pub(crate) async fn clear_focused(tab: &Tab) -> anyhow::Result<()> {
    tab.send("Input.dispatchKeyEvent", json!({"type": "rawKeyDown", "key": "a", "code": "KeyA", "modifiers": 4, "windowsVirtualKeyCode": 65, "commands": ["selectAll"]})).await?;
    tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "a", "code": "KeyA", "modifiers": 4, "windowsVirtualKeyCode": 65})).await?;
    tab.send("Input.dispatchKeyEvent", json!({"type": "rawKeyDown", "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8})).await?;
    tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8})).await?;
    Ok(())
}

async fn header(tab: &Tab, site: &str) -> String {
    tab.eval(hooks(site).header)
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn norm(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does chat title `title` match the requested `name` ("Markus" ↔ "Markus Weber")?
pub fn name_matches(title: &str, name: &str) -> bool {
    let (t, n) = (norm(title), norm(name));
    !n.is_empty() && (t == n || t.split(' ').any(|w| w == n) || t.starts_with(&format!("{n} ")) || n.split(' ').all(|w| t.split(' ').any(|x| x == w)))
}

/// Spoken names arrive as speech recognition spells them ("Marcus" for Markus, "Katrin" for
/// Kathrin): every word of `name` must be within one edit of a word of `title` that starts with
/// the same letter (names of 4+ letters only, so short words never match loosely).
pub fn name_matches_fuzzy(title: &str, name: &str) -> bool {
    fn dist(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut d: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut prev = d[0];
            d[0] = i;
            for j in 1..=b.len() {
                let cur = d[j];
                d[j] = (d[j] + 1).min(d[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
                prev = cur;
            }
        }
        d[b.len()]
    }
    let (t, n) = (norm(title), norm(name));
    // Same first letter too: recognition misspells inside a name, not its start (Hanna ≠ Anna).
    !n.is_empty() && n.split(' ').all(|w| w.chars().count() >= 4 && t.split(' ').any(|x| x.chars().next() == w.chars().next() && dist(w, x) <= 1))
}

/// Opens the chat whose title matches `name` (via the search box) and returns its exact title.
/// Ambiguous matches ("Markus" with two Markuses) are refused rather than guessed.
pub async fn open_chat(tab: &Tab, site: &str, name: &str) -> anyhow::Result<String> {
    let h = hooks(site);
    if name_matches(&header(tab, site).await, name) {
        return Ok(header(tab, site).await);
    }
    // Already in the visible list (Slack's sidebar): open it directly, no search needed.
    let visible: Vec<String> = serde_json::from_value::<Vec<String>>(tab.eval(h.titles).await.unwrap_or_default()).unwrap_or_default().into_iter().filter(|x| name_matches(x, name)).collect();
    let mut vis = visible.clone();
    vis.dedup();
    if vis.len() == 1 {
        let want = serde_json::to_string(&vis[0])?;
        if let Some((x, y)) = center(tab, &h.row.replace("{t}", &want)).await {
            click_at(tab, x, y).await?;
            let end = Instant::now() + Duration::from_secs(8);
            while Instant::now() < end {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if header(tab, site).await == vis[0] {
                    return Ok(vis[0].clone());
                }
            }
        }
    }
    let search = center(tab, h.search).await.context("search box not found")?;
    click_at(tab, search.0, search.1).await?;
    // Clear, then type the name like a user would (the list filters on input events).
    clear_focused(tab).await?;
    tab.send("Input.insertText", json!({"text": name})).await?;
    let list = || async { serde_json::from_value::<Vec<String>>(tab.eval(h.titles).await.unwrap_or_default()).unwrap_or_default() };
    let end = Instant::now() + Duration::from_secs(8);
    let mut titles = loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let hits: Vec<String> = list().await.into_iter().filter(|x| name_matches(x, name)).collect();
        if !hits.is_empty() || Instant::now() > end {
            break hits;
        }
    };
    if titles.is_empty() {
        // No exact hit: search the first three letters and allow one spelling edit per word.
        let stem: String = name.chars().take(3).collect();
        if stem.chars().count() == 3 {
            clear_focused(tab).await?;
            tab.send("Input.insertText", json!({"text": stem})).await?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            titles = list().await.into_iter().filter(|x| name_matches_fuzzy(x, name)).collect();
        }
    }
    let mut uniq = titles.clone();
    uniq.dedup();
    match uniq.len() {
        0 => bail!("no chat named \"{name}\" found"),
        1 => {}
        _ => bail!("several chats match \"{name}\": {} — say which one", uniq.join(", ")),
    }
    let want = serde_json::to_string(&uniq[0])?;
    let (x, y) = center(tab, &h.row.replace("{t}", &want)).await.context("chat row vanished")?;
    click_at(tab, x, y).await?;
    let end = Instant::now() + Duration::from_secs(8);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let t = header(tab, site).await;
        if t == uniq[0] {
            return Ok(t);
        }
    }
    bail!("chat \"{}\" didn't open", uniq[0])
}

/// The last `n` messages of the open chat (oldest first). Message text is untrusted data.
pub async fn read(tab: &Tab, site: &str, n: usize) -> anyhow::Result<Vec<ChatMsg>> {
    let v: Value = tab.eval(&hooks(site).messages.replace("{n}", &n.to_string())).await?;
    Ok(serde_json::from_value(v)?)
}

/// Sends `text` into the open chat, only if its header is exactly `chat` (the approved one).
/// Verifies the message appears as the newest outgoing message.
pub async fn send(tab: &Tab, site: &str, chat: &str, text: &str) -> anyhow::Result<()> {
    let h = hooks(site);
    let title = open_chat(tab, site, chat).await?;
    if title != chat {
        bail!("refusing to send: open chat is \"{title}\", approved was \"{chat}\"");
    }
    let (x, y) = center(tab, h.composer).await.context("message box not found")?;
    click_at(tab, x, y).await?;
    // Anything already in the box (an old draft) would be sent too: clear it first.
    let leftover = tab.eval(&format!("({} || {{}}).innerText || ''", h.composer)).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if !leftover.trim().is_empty() {
        clear_focused(tab).await?;
    }
    // Line breaks: Shift+Enter inside the box (a plain Enter would send early).
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            tab.send("Input.dispatchKeyEvent", json!({"type": "keyDown", "key": "Enter", "code": "Enter", "modifiers": 8, "windowsVirtualKeyCode": 13})).await?;
            tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "Enter", "code": "Enter", "modifiers": 8, "windowsVirtualKeyCode": 13})).await?;
        }
        if !line.is_empty() {
            tab.send("Input.insertText", json!({"text": line})).await?;
        }
    }
    let typed = tab.eval(&format!("({} || {{}}).innerText || ''", h.composer)).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if norm(&typed) != norm(text) {
        bail!("message box holds something else than the approved text; not sent");
    }
    tab.send("Input.dispatchKeyEvent", json!({"type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r"})).await?;
    tab.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13})).await?;
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(last) = read(tab, site, 3).await?.into_iter().rev().find(|m| m.outgoing) {
            if norm(&last.text) == norm(text) {
                return Ok(());
            }
        }
    }
    bail!("timed out: no confirmation the message appeared in the chat (it MAY have been sent — check before retrying)")
}

#[cfg(test)]
mod tests {
    use super::name_matches;

    #[test]
    fn chat_names() {
        assert!(name_matches("Markus Weber", "markus"));
        assert!(name_matches("Markus Weber", "Markus Weber"));
        assert!(!name_matches("Markusplatz Café", "markus"));
        assert!(!name_matches("Anna Berger", "markus"));
        assert!(super::name_matches_fuzzy("Markus Weber", "Marcus"));
        assert!(super::name_matches_fuzzy("Kathrin Huber", "katrin"));
        assert!(!super::name_matches_fuzzy("Anna Berger", "Hanna"));
        assert!(!super::name_matches_fuzzy("Tom Weber", "Tim"));
    }
}
