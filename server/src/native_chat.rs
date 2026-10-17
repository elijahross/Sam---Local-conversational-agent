//! Chat replies through the user's installed chat app instead of Sam's own web session
//! (WhatsApp for Mac today; Telegram for Mac exposes no Accessibility tree, Slack isn't covered
//! yet). Everything happens with the app in the background:
//! - open / draft: the app's own link (`whatsapp://send?phone=…&text=…`, `open -g`) opens the
//!   chat with the draft in its message box — the app's model registers it (AX value sets don't).
//! - read: the helper's read-only `/chat_read` snapshot of the open chat (header verified).
//! - send: the helper's `/chat_send`, which can only press the app's own Send button and only
//!   while the box holds exactly the approved text.
//! - cancel: an empty link clears the box, only if it still holds Sam's draft.
//!
//! Chats are addressed by phone number (Contacts lookup); groups and unknown names fall back to
//! the web session. Off with the flag file `native-chats-disabled` (Connectors).

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::browser::chat::ChatMsg;

const HELPER: &str = "http://127.0.0.1:47803";
pub const WHATSAPP: &str = "net.whatsapp.WhatsApp";

fn flag() -> std::path::PathBuf {
    crate::localauth::token_path().with_file_name("native-chats-disabled")
}
pub fn enabled() -> bool {
    !flag().exists()
}
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    if on {
        let _ = std::fs::remove_file(flag());
    } else {
        std::fs::write(flag(), b"native chat apps off\n")?;
    }
    Ok(())
}

/// The installed app Sam can drive for a chat site, if any.
pub fn app_for(site: &str) -> Option<&'static str> {
    static WA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    match site {
        "whatsapp" if *WA.get_or_init(|| installed(WHATSAPP)) => Some(WHATSAPP),
        _ => None,
    }
}

pub fn installed(bundle: &str) -> bool {
    std::process::Command::new("/usr/bin/mdfind")
        .arg(format!("kMDItemCFBundleIdentifier == '{bundle}'"))
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.ends_with(".app")))
}

/// How Sam reaches a chat service — the user's choice in Connectors: "app" (their installed
/// app) or "web" (Sam's own browser session). Default: the app when Sam can drive it.
pub fn mode(site: &str) -> &'static str {
    match crate::connectors::state()["chat_via"][site].as_str() {
        Some("web") => "web",
        Some("app") if app_for(site).is_some() => "app",
        _ if app_for(site).is_some() && enabled() => "app",
        _ => "web",
    }
}

pub fn set_mode(site: &str, via: &str) -> anyhow::Result<()> {
    if via == "app" && app_for(site).is_none() {
        bail!("Sam can't operate that app in the background; use Sam's browser");
    }
    crate::connectors::set_chat_via(site, via);
    // The helper's switch follows: on while any service uses its app.
    set_enabled(["whatsapp", "telegram", "slack"].iter().any(|s| mode(s) == "app"))
}

/// The app to use for this site, if the user chose the app (None = the web session).
pub fn usable(site: &str) -> Option<&'static str> {
    // Tests (SAM_NO_REAL_APPS=1) never touch the user's chat apps: they use Sam's own web
    // session against the fixtures (CHAT_URL_*).
    if test_mode() || mode(site) != "app" {
        return None;
    }
    app_for(site)
}

/// Test runs never touch the user's real chat app.
pub fn test_mode() -> bool {
    std::env::var("SAM_NO_REAL_APPS").is_ok_and(|v| v == "1")
}

/// "+43 660 1234567" / "0043…" → "436601234567". National numbers without a country code
/// can't be addressed safely → None.
pub fn digits(phone: &str) -> Option<String> {
    let t = phone.trim();
    let d: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    let d = if t.starts_with('+') { d } else if let Some(x) = d.strip_prefix("00") { x.to_string() } else { return None };
    (8..=15).contains(&d.len()).then_some(d)
}

fn digits_plain(d: &str) -> Option<String> {
    (d.len() >= 8 && d.chars().all(|c| c.is_ascii_digit())).then(|| d.to_string())
}

/// The self-chat's number from a pointed-at WhatsApp window ("+49 176 84561924" next to "(Du)" /
/// "(You)"): remembered for "message myself".
pub fn learn_self_number(context: &str) -> Option<String> {
    let re = regex::Regex::new(r"(\+[\d \u{00a0}\-]{8,20}\d)[\s\u{200e}]*\((?:\u{200e})?(?:Du|You|Sie)\)").ok()?;
    let d = digits(re.captures(context)?.get(1)?.as_str())?;
    crate::connectors::set_my_phone(&d);
    Some(d)
}

/// The remembered own number, if any (no Contacts lookup).
pub fn phone_for_cached_self() -> Option<String> {
    crate::connectors::state()["my_phone"].as_str().and_then(digits_plain)
}

pub fn is_self(name: &str) -> bool {
    let n = name.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    ["me", "myself", "my self", "du", "you", "yourself", "mir", "mich", "mich selbst", "mir selbst", "selbst", "self", "note to self", "message yourself", "notiz an mich"].contains(&n.as_str())
}

/// The chat's phone number: the name itself if it is one, else the Contacts entry.
pub async fn phone_for(name: &str) -> Option<String> {
    if name.chars().filter(|c| c.is_ascii_digit()).count() >= 8 {
        return digits(name);
    }
    // "myself" / "me" / WhatsApp's own label "Du" / "You": the self-chat (My Card's number).
    if is_self(name) {
        if let Some(d) = crate::connectors::state()["my_phone"].as_str().and_then(digits_plain) {
            return Some(d);
        }
        return crate::apps::my_phones().await.ok()?.iter().find_map(|p| digits(p));
    }
    let people = crate::apps::contacts(name).await.ok()?;
    people.as_array()?.iter()
        .filter(|p| crate::browser::chat::name_matches(p["name"].as_str().unwrap_or(""), name) || crate::browser::chat::name_matches_fuzzy(p["name"].as_str().unwrap_or(""), name))
        .flat_map(|p| p["phones"].as_array().cloned().unwrap_or_default())
        .find_map(|ph| ph.as_str().and_then(digits))
}

async fn helper(http: &reqwest::Client, path: &str, body: Value) -> anyhow::Result<Value> {
    let r = http.post(format!("{HELPER}{path}")).header("X-Sam-Token", crate::localauth::token()).json(&body).timeout(Duration::from_secs(20)).send().await.context("the Sam Computer Use helper isn't running")?;
    let v: Value = r.json().await?;
    if let Some(e) = v["error"].as_str() {
        bail!("{e}");
    }
    Ok(v)
}

/// Opens the chat in the background; with `text`, that text becomes the message box content
/// (`Some("")` clears it).
async fn open(phone: &str, text: Option<&str>) -> anyhow::Result<()> {
    let mut url = format!("whatsapp://send?phone={phone}");
    if let Some(t) = text {
        url.push_str("&text=");
        url.extend(url::form_urlencoded::byte_serialize(t.as_bytes()).map(|x| if x == "+" { "%20" } else { x }));
    }
    let st = tokio::process::Command::new("/usr/bin/open").args(["-g", &url]).status().await?;
    if !st.success() {
        bail!("WhatsApp didn't open the chat");
    }
    tokio::time::sleep(Duration::from_millis(1800)).await;
    Ok(())
}

/// The open chat as the helper sees it: (header title, messages, message box value).
pub fn parse(elements: &str) -> (String, Vec<ChatMsg>, String) {
    let clean = |s: &str| s.replace(['\u{200e}', '\u{200f}', '\u{202a}', '\u{202c}', '\u{2068}', '\u{2069}'], "").trim().to_string();
    // "e12 Button \"Label\" = \"value\"" → (role, label, value)
    let rows: Vec<(String, String, String)> = elements
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let (_, role, rest) = (it.next()?, it.next()?, it.next().unwrap_or(""));
            let (label, value) = match rest.split_once("\" = \"") {
                Some((a, b)) => (a.trim_start_matches('"'), b.trim_end_matches('"')),
                None => (rest.trim_matches('"'), ""),
            };
            Some((role.to_string(), clean(label), clean(value)))
        })
        .collect();
    let composer = rows.iter().rposition(|r| r.0 == "TextArea");
    let box_value = composer.map(|i| rows[i].2.clone()).unwrap_or_default();
    let end = composer.unwrap_or(rows.len());
    // The conversation starts after the chat list (buttons with a preview value) and the search.
    let start = rows[..end].iter().rposition(|r| r.0 == "Button" && !r.2.is_empty()).map(|i| i + 1).unwrap_or(0);
    let start = rows[start..end].iter().position(|r| r.1 == "Text löschen" || r.1 == "Clear text").map(|i| start + i + 1).unwrap_or(start);
    let conv = &rows[start..end];
    let controls = ["Mehr", "More", "Medien teilen", "Share media", "Attach", "Emoji-Auswahl", "Emoji", "Sprachnachricht", "Voice message", "Senden", "Send", "Neuer Chat", "New chat", "Bearbeiten", "Edit", "Text löschen", "Clear text"];
    let sections = ["Chats", "Communitys", "Communities", "Kontakte", "Contacts", "Medien", "Media", "Nachrichten", "Messages", "Mehr", "More", "Aktuelles", "Updates", "Anrufe", "Calls"];
    // The chat header is the last plain button before the conversation's first date heading
    // ("Heute", "Gestern", a date); without one (an empty chat), the last before the composer.
    let is_day = |r: &(String, String, String)| {
        let l = r.1.to_lowercase();
        (r.0 == "Heading" && !sections.contains(&r.1.as_str()))
            || (r.0 == "StaticText" && r.2.is_empty() && (["heute", "gestern", "today", "yesterday", "montag", "dienstag", "mittwoch", "donnerstag", "freitag", "samstag", "sonntag", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"].contains(&l.as_str())
                || (l.len() <= 12 && l.chars().any(|c| c.is_ascii_digit()) && l.contains('.'))))
    };
    let plain_button = |r: &(String, String, String)| r.0 == "Button" && r.2.is_empty() && !controls.contains(&r.1.as_str()) && !r.1.to_lowercase().contains("ende-zu-ende") && !r.1.to_lowercase().contains("end-to-end");
    // Header: the first plain button after the search field; else the last before the first day marker.
    let search = conv.iter().position(|r| r.1 == "Suchen" || r.1 == "Search");
    let header = match search {
        Some(si) => conv[si + 1..].iter().position(|r| plain_button(r)).map(|i| si + 1 + i),
        None => {
            let first_day = conv.iter().position(|r| is_day(r)).unwrap_or(conv.len());
            conv[..first_day].iter().rposition(|r| plain_button(r))
        }
    };
    let title = header.map(|i| conv[i].1.clone()).unwrap_or_default();
    let body = header.map(|i| &conv[i + 1..]).unwrap_or(&conv[conv.len()..]);
    let mut day = String::new();
    let mut msgs = Vec::new();
    for r in body {
        if is_day(r) {
            day = r.1.clone();
            continue;
        }
        let text = if r.2.is_empty() { r.1.clone() } else { format!("{} {}", r.1, r.2) };
        let low = text.to_lowercase();
        if text.is_empty() || controls.contains(&r.1.as_str()) || r.0 == "StaticText" && text.starts_with('(') || low.contains("ende-zu-ende") || low.contains("end-to-end") {
            continue;
        }
        let outgoing = ["deine ", "du:", "your ", "you:"].iter().any(|p| low.starts_with(p));
        msgs.push(ChatMsg { from: if outgoing { "me".into() } else { title.clone() }, time: day.clone(), outgoing, text });
    }
    (title, msgs, box_value)
}

/// Opens and reads a chat by name (header must match the person or number).
pub async fn read(http: &reqwest::Client, bundle: &str, name: &str, n: usize) -> anyhow::Result<(String, Vec<ChatMsg>, String)> {
    let phone = phone_for(name).await.with_context(|| format!("no phone number for \"{name}\" in Contacts"))?;
    open(&phone, None).await?;
    let v = helper(http, "/chat_read", json!({"app": bundle})).await?;
    let (title, mut msgs, _) = parse(v["elements"].as_str().unwrap_or(""));
    let title_digits: String = title.chars().filter(|c| c.is_ascii_digit()).collect();
    let ok = is_self(name) && title_digits.ends_with(&phone[phone.len().saturating_sub(8)..])
        || crate::browser::chat::name_matches(&title, name) || crate::browser::chat::name_matches_fuzzy(&title, name) || (!title_digits.is_empty() && title_digits.ends_with(&phone[phone.len().saturating_sub(8)..]));
    if !ok {
        bail!("WhatsApp opened \"{title}\", not \"{name}\"");
    }
    let skip = msgs.len().saturating_sub(n);
    msgs.drain(..skip);
    Ok((title, msgs, phone))
}

/// Puts the draft into the chat's message box (visible in the app until sent or cancelled).
pub async fn draft(http: &reqwest::Client, bundle: &str, phone: &str, text: &str) -> anyhow::Result<()> {
    open(phone, Some(text)).await?;
    let v = helper(http, "/chat_read", json!({"app": bundle})).await?;
    let (_, _, boxv) = parse(v["elements"].as_str().unwrap_or(""));
    if boxv.trim() != text.trim() {
        bail!("the draft didn't land in WhatsApp's message box");
    }
    Ok(())
}

/// Sends the approved text: re-drafts if the box was emptied meanwhile, refuses if it holds
/// anything else (the user typed there), then presses the app's Send button.
pub async fn send(http: &reqwest::Client, bundle: &str, phone: &str, text: &str) -> anyhow::Result<()> {
    open(phone, None).await?;
    let v = helper(http, "/chat_read", json!({"app": bundle})).await?;
    let (_, _, boxv) = parse(v["elements"].as_str().unwrap_or(""));
    if boxv.trim().is_empty() {
        draft(http, bundle, phone, text).await?;
    } else if boxv.trim() != text.trim() {
        bail!("WhatsApp's message box holds other text now; not sending");
    }
    helper(http, "/chat_send", json!({"app": bundle, "expect": text})).await?;
    Ok(())
}

/// Removes a cancelled draft from the message box (only if it is still exactly Sam's draft).
pub async fn clear(http: &reqwest::Client, bundle: &str, phone: &str, text: &str) {
    if open(phone, None).await.is_err() {
        return;
    }
    // Empty the visible box (helper: only if it is exactly the draft), then an empty link so
    // the app's own draft state follows (the link alone doesn't clear existing text).
    if helper(http, "/chat_clear", json!({"app": bundle, "expect": text})).await.is_ok() {
        let _ = open(phone, Some("")).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_open_chat() {
        let snap = "e24 Button \"Wadim Mitlewski\" = \"\u{200e}Deine Nachricht\"\ne38 StaticText \"\u{200e}Suchen\" = \"du\"\ne39 Button \"\u{200e}Text löschen\"\ne40 Button \"+49 176 84561924\"\ne41 StaticText \"(\u{200e}Du)\"\ne42 Button \"\u{200e}Mehr\"\ne43 Heading \"\u{200e}Heute\"\ne44 Button \"\u{200e}\u{200e}Nachrichten an dich selbst sind Ende-zu-Ende-verschlüsselt\"\ne45 Button \"Markus, Monday works?, 10:06\"\ne46 Button \"Deine Nachricht, Yes!, 10:07, Gelesen\"\ne47 Button \"\u{200e}Medien teilen\"\ne48 TextArea \"\u{200e}Nachricht erstellen\" = \"draft\"\ne49 Button \"\u{200e}Senden\"";
        let (title, msgs, boxv) = super::parse(snap);
        assert_eq!(title, "+49 176 84561924");
        assert_eq!(boxv, "draft");
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(!msgs[0].outgoing && msgs[1].outgoing && msgs[0].time == "Heute");
        assert_eq!(super::digits("+43 660 355-2510").as_deref(), Some("436603552510"));
        assert_eq!(super::digits("0660 3552510"), None);
        // Real layout without a search term: no "Text löschen" anchor.
        let real = "e19 Button \"Iman Gadzhi\" = \"\u{200e}Nachricht, hi\"\ne20 StaticText \"\u{200e}Deine persönlichen Nachrichten sind Ende-zu-Ende-verschlüsselt\"\ne22 Heading \"\u{200e}Chats\"\ne23 Button \"\u{200e}Mehr\"\ne24 Button \"\u{200e}Neuer Chat\"\ne25 StaticText \"\u{200e}Suchen\"\ne26 Button \"+49 176 84561924\"\ne27 StaticText \"(\u{200e}Du)\"\ne28 Button \"\u{200e}Mehr\"\ne29 StaticText \"\u{200e}Heute\"\ne30 Button \"\u{200e}Nachrichten an dich selbst sind Ende-zu-Ende-verschlüsselt\"\ne31 StaticText \"\u{200e}Deine Nachricht, hello, 20:19, \u{200e}Gesendet\"\ne32 Button \"\u{200e}Medien teilen\"\ne33 TextArea \"\u{200e}Nachricht erstellen\"";
        let (t2, m2, _) = super::parse(real);
        assert_eq!(t2, "+49 176 84561924");
        assert_eq!(m2.len(), 1, "{m2:?}");
        assert!(m2[0].outgoing && m2[0].text.contains("hello"));
    }
}

#[cfg(test)]
mod live {
    /// `NATIVE_TEST_PHONE=4917… NATIVE_TEST_TEXT=… cargo test --release --lib native_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn native_live_clear() {
        let http = reqwest::Client::new();
        let phone = std::env::var("NATIVE_TEST_PHONE").unwrap();
        let text = std::env::var("NATIVE_TEST_TEXT").unwrap();
        let v = super::helper(&http, "/chat_read", serde_json::json!({"app": super::WHATSAPP})).await.unwrap();
        let (title, msgs, boxv) = super::parse(v["elements"].as_str().unwrap_or(""));
        println!("title {title:?} msgs {} box {boxv:?} equal {}", msgs.len(), boxv.trim() == text.trim());
        super::clear(&http, super::WHATSAPP, &phone, &text).await;
        let v = super::helper(&http, "/chat_read", serde_json::json!({"app": super::WHATSAPP})).await.unwrap();
        println!("after: box {:?}", super::parse(v["elements"].as_str().unwrap_or("")).2);
    }
}

#[cfg(test)]
mod self_tests {
    #[test]
    fn self_names_and_number() {
        assert!(super::is_self("myself") && super::is_self("Du") && super::is_self(" me ") && !super::is_self("Markus"));
        let re = regex::Regex::new(r"(\+[\d \u{00a0}\-]{8,20}\d)[\s\u{200e}]*\((?:\u{200e})?(?:Du|You|Sie)\)").unwrap();
        assert!(re.is_match("+49 176 84561924\n(\u{200e}Du)"));
    }
}
