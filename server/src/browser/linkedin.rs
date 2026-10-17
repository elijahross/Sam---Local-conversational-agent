//! LinkedIn in Sam's own "accounts" browser profile (Connectors › LinkedIn: sign in once).
//! Used read-only by monitors on LinkedIn search pages (people / jobs / companies) for leads:
//! only pages the user could open by hand, a few times a day at most (LinkedIn restricts
//! accounts that look automated), never messages or connection requests.

use std::time::{Duration, Instant};

use anyhow::bail;

use super::cdp::Tab;

pub const HOME: &str = "https://www.linkedin.com/feed/";
/// Monitors on LinkedIn check at most this often (minutes).
pub const MIN_INTERVAL: i64 = 360;

/// Signed in = the session cookie is there and the feed doesn't bounce to a login page.
pub async fn signed_in(tab: &Tab) -> bool {
    if !tab.has_cookie("https://www.linkedin.com/", "li_at").await {
        return false;
    }
    let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    !(here.contains("/login") || here.contains("/authwall") || here.contains("/checkpoint"))
}

/// Waits until Sam's LinkedIn session is signed in; shows the window and asks once if not.
pub async fn ready(tab: &Tab, on_user: Option<&tokio::sync::mpsc::UnboundedSender<String>>) -> anyhow::Result<()> {
    tab.goto(HOME, Duration::from_secs(30)).await?;
    if signed_in(tab).await {
        return Ok(());
    }
    tab.goto("https://www.linkedin.com/login", Duration::from_secs(30)).await?;
    tab.show_window(true).await;
    let ask = "Sam needs its own LinkedIn sign-in once: sign in in the browser window I just opened. I'll continue as soon as you're in.";
    match on_user {
        Some(tx) => {
            let _ = tx.send(ask.into());
        }
        None => eprintln!("\n>>> {ask}"),
    }
    let end = Instant::now() + Duration::from_secs(600);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        if tab.has_cookie("https://www.linkedin.com/", "li_at").await && !here.contains("/login") && !here.contains("/checkpoint") {
            tab.show_window(false).await;
            crate::connectors::mark_linked("linkedin");
            return Ok(());
        }
    }
    bail!("LinkedIn wasn't signed in within 10 minutes")
}

pub fn is_linkedin(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.ends_with("linkedin.com"))).unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// Reading: people search cards, a compact profile, recent posts. LinkedIn's class names are
// generated, so the parsers use structure (role=list/listitem, sections, headings) and text.
// Every page view goes through `view`: a human-like pause and a daily cap.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Page views per day across all LinkedIn work (search, profiles, activity).
pub const DAILY_VIEWS: u32 = 120;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lead {
    pub slug: String,
    pub name: String,
    pub degree: String,
    pub headline: String,
    pub location: String,
    /// "Aktuell: … / Früher: …" line from the search card (current / past role match).
    pub summary: String,
    /// Connect | Message | Follow — what the card offers.
    pub action: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Post {
    pub url: String,
    pub date: String,
    pub text: String,
    #[serde(default)]
    pub author: String,
    /// The lead's own post (not a reshare of someone else's).
    pub own: bool,
}

const SEARCH_JS: &str = r#"(() => {
  const actions = /^(Vernetzen|Connect|Nachricht|Message|Folgen|Follow|Ausstehend|Pending)$/;
  const items = [...document.querySelectorAll('[role=listitem], main li')].filter(e => e.querySelector('a[href*="/in/"]'));
  const seen = new Set(), out = [];
  for (const it of items) {
    const a = it.querySelector('a[href*="/in/"]');
    const m = a.href.match(/\/in\/([^/?#]+)/); if (!m || seen.has(m[1])) continue;
    const lines = it.innerText.split('\n').map(x => x.trim()).filter(Boolean);
    if (lines.length < 2) continue;
    seen.add(m[1]);
    const di = lines.findIndex(l => /^[•·]\s*\d/.test(l) || /^\d\.(\+)?$/.test(l));
    const rest = lines.slice(di >= 0 ? di + 1 : 1).filter(l => !/^(Status ist|Status is)/.test(l));
    const act = rest.find(l => actions.test(l)) || '';
    const info = rest.filter(l => !actions.test(l));
    out.push({slug: decodeURIComponent(m[1]), name: lines[0].replace(/\s*[•·].*$/, ''), degree: di >= 0 ? lines[di].replace(/[•·\s]/g, '') : '',
      headline: info[0] || '', location: info[1] || '', summary: info.find(l => /^(Aktuell|Früher|Current|Past|Kenntnisse|Skills)\s*:/.test(l)) || '',
      action: ({Vernetzen: 'connect', Connect: 'connect', Nachricht: 'message', Message: 'message', Folgen: 'follow', Follow: 'follow'})[act] || act.toLowerCase(),
      url: 'https://www.linkedin.com/in/' + m[1] + '/'});
  }
  return out;
})()"#;

/// Profile as compact text: top card, About, the first roles of Experience, the activity
/// snippet. Section boundaries by heading text (DE / EN UI).
const PROFILE_JS: &str = r#"(() => {
  const clean = (t) => (t || '').split('\n').map(x => x.trim()).filter(Boolean).filter((x, i, a) => a.indexOf(x) === i);
  const secs = [...document.querySelectorAll('main section')].map(s => clean(s.innerText)).filter(l => l.length);
  const top = secs[0] || [];
  const find = (re) => secs.find(l => re.test(l[0])) || [];
  const about = find(/^(Info|About|Über)$/).slice(1).join(' ');
  const exp = find(/^(Erfahrung|Experience|Berufserfahrung)$/).slice(1, 16);
  const act = find(/^(Aktivitäten|Activity)$/).slice(1, 10).filter(l => !/^(Folgen|Follow|\d[\d.]*\s*Follower)/.test(l));
  return {top: top.slice(0, 8).filter(l => !/^(Kontaktinformationen|Contact info|Nachricht|Message|Folgen|Follow|Vernetzen|Connect|Mehr|More|·)$/.test(l)).join(' | '),
          about: about.slice(0, 600), experience: exp.join(' | ').slice(0, 700), activity: act.join(' | ').slice(0, 400)};
})()"#;

const POSTS_JS: &str = r#"(() => {
  const out = [], seen = new Set();
  for (const a of document.querySelectorAll('a[href*="/feed/update/"]')) {
    const url = a.href.split('?')[0]; if (seen.has(url)) continue;
    let c = a; for (let i = 0; i < 12 && c.parentElement; i++) { c = c.parentElement; if ((c.innerText || '').length > 200) break; }
    const lines = (c.innerText || '').split('\n').map(x => x.trim()).filter(Boolean).filter(l => !/^(Feed-Beitrag|Feed post|Folgen|Follow|•.*)$/.test(l));
    seen.add(url);
    const di = lines.findIndex(l => /^\d{1,2}\.\s?[A-Za-zä]{3}|^\d+\s?(Std|Tag|Wo|Mon|h|d|w|mo)\b|^vor /.test(l));
    const reshare = /hat das geteilt|reposted|shared this|kommentiert|commented|gefällt|likes|reagiert|reacted/i.test(lines.slice(0, 2).join(' '));
    out.push({url, date: di >= 0 ? lines[di] : '', text: lines.slice(di >= 0 ? di + 1 : 1).join(' ').slice(0, 500), own: !reshare, author: (lines[0] || '').replace(/\s*(hat das geteilt|reposted).*$/i, '')});
  }
  return out.slice(0, 8);
})()"#;

fn usage_file() -> std::path::PathBuf {
    crate::thinker::data_dir().join("linkedin_usage.json")
}

/// Counts a page view against today's cap; Err when the day's budget is used up.
fn count_view() -> anyhow::Result<()> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut u: Value = std::fs::read_to_string(usage_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let n = u[&today].as_u64().unwrap_or(0) as u32;
    if n >= DAILY_VIEWS {
        bail!("LinkedIn page budget for today is used up ({DAILY_VIEWS} views); continuing tomorrow keeps the account safe");
    }
    u = serde_json::json!({ today: n + 1 });
    let _ = std::fs::write(usage_file(), u.to_string());
    Ok(())
}

pub fn views_today() -> u32 {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    std::fs::read_to_string(usage_file()).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|u| u[&today].as_u64()).unwrap_or(0) as u32
}

/// One paced page view: 4–9 s pause (people don't open pages back to back), cap, load, settle.
pub async fn view(tab: &Tab, url: &str) -> anyhow::Result<()> {
    count_view()?;
    let pause = 4000 + (chrono::Local::now().timestamp_subsec_millis() as u64 * 5);
    tokio::time::sleep(Duration::from_millis(pause)).await;
    tab.goto(url, Duration::from_secs(30)).await?;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let here = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if here.contains("/login") || here.contains("/authwall") || here.contains("/checkpoint") {
        bail!("LinkedIn asks to sign in again (Connectors › LinkedIn › Link)");
    }
    Ok(())
}

pub fn search_url(keywords: &str, page: u32) -> String {
    let q: String = url::form_urlencoded::byte_serialize(keywords.as_bytes()).collect();
    format!("https://www.linkedin.com/search/results/people/?keywords={q}&origin=GLOBAL_SEARCH_HEADER{}", if page > 1 { format!("&page={page}") } else { String::new() })
}

/// People on an already loaded search page (monitors).
pub async fn search_page(tab: &Tab, url: &str) -> anyhow::Result<Vec<Lead>> {
    view(tab, url).await?;
    Ok(serde_json::from_value(tab.eval(SEARCH_JS).await?).unwrap_or_default())
}

/// LinkedIn's monthly limit on people searches for free accounts ("commercial use limit").
pub const SEARCH_LIMIT: &str = "linkedin search limit";

pub async fn search(tab: &Tab, keywords: &str, page: u32) -> anyhow::Result<Vec<Lead>> {
    view(tab, &search_url(keywords, page)).await?;
    let text = tab.eval("document.body.innerText.slice(0, 4000)").await.ok().and_then(|v| v.as_str().map(str::to_lowercase)).unwrap_or_default();
    if ["commercial use limit", "kommerzielle nutzung", "monatliche limit", "monthly limit for searches", "suchlimit"].iter().any(|p| text.contains(p)) {
        bail!("{SEARCH_LIMIT}: LinkedIn paused people searches for this account until next month");
    }
    Ok(serde_json::from_value(tab.eval(SEARCH_JS).await?).unwrap_or_default())
}

/// Compact profile text (≈ 1–2k chars) for fit scoring.
pub async fn profile(tab: &Tab, slug: &str) -> anyhow::Result<Value> {
    view(tab, &format!("https://www.linkedin.com/in/{slug}/")).await?;
    // Experience / Education render only once scrolled into view.
    let _ = tab.eval("(async () => { for (let r = 0; r < 3; r++) for (const s of document.querySelectorAll('main section')) if (!(s.innerText || '').trim()) { s.scrollIntoView({block: 'center'}); await new Promise(x => setTimeout(x, 700)); } })()").await;
    tab.eval(PROFILE_JS).await
}

/// Recent posts; `own` = written by `name` (not a reshare / someone else's post they engaged with).
pub async fn posts(tab: &Tab, slug: &str, name: &str) -> anyhow::Result<Vec<Post>> {
    view(tab, &format!("https://www.linkedin.com/in/{slug}/recent-activity/all/")).await?;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let mut ps: Vec<Post> = serde_json::from_value(tab.eval(POSTS_JS).await?).unwrap_or_default();
    let first = name.split_whitespace().next().unwrap_or("").to_lowercase();
    for p in &mut ps {
        p.own = p.own && !first.is_empty() && p.author.to_lowercase().contains(&first);
    }
    Ok(ps)
}

// ---------------------------------------------------------------------------------------------
// Outreach: connection request (with note), message, comment. Each runs only after the user
// approved that exact text (pending action, ask_always) and within the daily caps (leads.rs).
// `send = false` is a dry run: everything up to a found, enabled Send button, then it cancels
// and clears what it typed (tests, previews). Buttons are found by visible text / aria-label
// (German and English UI), never by LinkedIn's generated class names.

use super::chat::{center, clear_focused, click_at};

/// Shadow-aware lookups: LinkedIn renders dialogs inside a shadow root ("interop outlet").
const DEEP: &str = "(() => { const roots = [document, ...[...document.querySelectorAll('*')].filter(e => e.shadowRoot).map(e => e.shadowRoot)]; \
  const all = (s) => roots.flatMap(r => [...r.querySelectorAll(s)]); \
  const dialog = () => all('[role=dialog], [role=alertdialog], dialog').filter(d => d.getBoundingClientRect().height > 0).pop() || null; \
  return {all, dialog}; })()";

/// A visible button / link whose text or aria-label matches `re`, inside `scope` ("" = the
/// whole page incl. shadow roots, "dialog" = the open dialog, else a CSS selector);
/// JS expression yielding the element or null.
fn btn(re: &str, scope: &str) -> String {
    let root = match scope {
        "" => "null".to_string(),
        "dialog" => format!("{DEEP}.dialog()"),
        css => format!("document.querySelector({})", serde_json::to_string(css).unwrap()),
    };
    format!(
        "(() => {{ const D = {DEEP}; const root = {root}; if ({scope_needed} && !root) return null; const re = new RegExp({re}, 'i'); \
         const els = root ? [...root.querySelectorAll('button, [role=button], a, [role=menuitem], div[aria-label]')] : D.all('button, [role=button], a, [role=menuitem], div[aria-label]'); \
         return els.find(e => {{ const r = e.getBoundingClientRect(); if (!r.width || !r.height) return false; \
           const t = (e.innerText || '').trim(), l = e.getAttribute('aria-label') || ''; return re.test(t) || re.test(l); }}) || null; }})()",
        re = serde_json::to_string(re).unwrap(),
        scope_needed = !scope.is_empty(),
    )
}

async fn click(tab: &Tab, js: &str) -> anyhow::Result<bool> {
    match center(tab, js).await {
        Some((x, y)) => {
            click_at(tab, x, y).await?;
            tokio::time::sleep(Duration::from_millis(900)).await;
            Ok(true)
        }
        None => Ok(false),
    }
}

async fn exists(tab: &Tab, js: &str) -> bool {
    tab.eval(&format!("!!({js})")).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// The profile's top card (its first section) — buttons elsewhere belong to other people.
const TOP: &str = "main section";

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub sent: bool,
    pub detail: String,
}

/// Connection request with an optional note (LinkedIn allows 300 characters). Safety: only
/// a Connect action whose label names THIS person is clicked (sidebars list other people's
/// Connect buttons), and the dialog must name them before anything is sent.
pub async fn connect(tab: &Tab, slug: &str, name: &str, note: &str, send: bool) -> anyhow::Result<Outcome> {
    view(tab, &format!("https://www.linkedin.com/in/{slug}/")).await?;
    let first = name.split_whitespace().next().unwrap_or("").to_string();
    anyhow::ensure!(first.chars().count() >= 2, "need the person's name to find their Connect button safely");
    if exists(tab, &btn(r"^(Ausstehend|Pending)$|Einladung zurückziehen|Withdraw", TOP)).await {
        bail!("an invitation to {name} is already pending");
    }
    let mine = format!(r"(?:einladen|Invite|vernetzen|connect).*{n}|{n}.*(?:einladen|vernetzen|connect)", n = regex::escape(name));
    let mut found = click(tab, &btn(&mine, "")).await?;
    if !found {
        // Often behind "Mehr" / "More" in the top card; the menu item carries the name too.
        if click(tab, &btn(r"^(Mehr|More)$|Weitere Aktionen|More actions", TOP)).await? {
            tokio::time::sleep(Duration::from_millis(600)).await;
            found = click(tab, &btn(&mine, "")).await?;
        }
    }
    if !found {
        bail!("no Connect action for {name} on the profile (already connected, or LinkedIn only offers Follow)");
    }
    let dialog = "dialog";
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let dtext = tab.eval(&format!("(({DEEP}.dialog()) || {{}}).innerText || ''")).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    if !dtext.to_lowercase().contains(&first.to_lowercase()) {
        let _ = click(tab, &btn(r"^(Verwerfen|Dismiss|Schließen|Close|Abbrechen|Cancel)$", dialog)).await;
        bail!("the invitation dialog doesn't name {name}; stopped (nothing sent)");
    }
    if !note.trim().is_empty() {
        if !click(tab, &btn(r"^(Nachricht hinzufügen|Notiz hinzufügen|Add a note|Add a message|Personalisieren|Personalize)$", dialog)).await? {
            let _ = click(tab, &btn(r"^(Verwerfen|Dismiss|Schließen|Close|Abbrechen|Cancel)$", dialog)).await;
            bail!("LinkedIn offers no personal note right now (free accounts get a few per month); send without a note or wait");
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        if !click(tab, &format!("(({DEEP}.dialog()) || document).querySelector('textarea')")).await? {
            bail!("no note field in the invitation dialog");
        }
        let text: String = note.chars().take(300).collect();
        tab.send("Input.insertText", serde_json::json!({"text": text})).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let send_btn = btn(r"^(Senden|Send|Ohne Notiz senden|Send without a note|Einladung senden|Send invitation)$", dialog);
    let enabled = tab.eval(&format!("(() => {{ const b = {send_btn}; return !!b && !b.disabled && b.getAttribute('aria-disabled') !== 'true'; }})()")).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
    if !send {
        if !note.trim().is_empty() {
            clear_focused(tab).await?;
        }
        let _ = click(tab, &btn(r"^(Verwerfen|Dismiss|Schließen|Close|Abbrechen|Cancel)$", dialog)).await;
        return Ok(Outcome { sent: false, detail: format!("dry run: invitation to {name} ready (Send button {}), cancelled", if enabled { "found" } else { "NOT found" }) });
    }
    if !enabled {
        bail!("the Send button isn't available in the invitation dialog");
    }
    click(tab, &send_btn).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pending = exists(tab, &btn(r"^(Ausstehend|Pending)$", TOP)).await;
    Ok(Outcome { sent: true, detail: if pending { "invitation sent (pending)".into() } else { "invitation sent".into() } })
}

/// A message to a first-degree contact (the profile's Message button opens the chat overlay,
/// possibly inside LinkedIn's shadow root).
pub async fn message(tab: &Tab, slug: &str, text: &str, send: bool) -> anyhow::Result<Outcome> {
    view(tab, &format!("https://www.linkedin.com/in/{slug}/")).await?;
    if !click(tab, &btn(r"^(Nachricht|Message)$", TOP)).await? {
        bail!("no Message button on the profile (not connected?)");
    }
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let boxjs = format!("({DEEP}.all('[contenteditable=true]').filter(e => e.getBoundingClientRect().height > 0 && !e.closest('main')).pop() || null)");
    if !exists(tab, &boxjs).await {
        let premium = exists(tab, &btn(r"Premium|InMail", "")).await;
        let _ = click(tab, &btn(r"Unterhaltung schließen|Close your conversation|Close conversation|^(Verwerfen|Dismiss|Schließen|Close)$", "")).await;
        bail!("{}", if premium { "not a connection: LinkedIn only offers InMail (Premium) for this person" } else { "no message box opened" });
    }
    click(tab, &boxjs).await?;
    tab.send("Input.insertText", serde_json::json!({"text": text})).await?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let send_btn = format!(
        "(() => {{ let c = {boxjs}; const ok = (b) => b.getBoundingClientRect().width && (/^(Senden|Send)$/i.test((b.innerText || '').trim()) || /^(Senden|Send)$/i.test(b.getAttribute('aria-label') || '')); \
           for (let i = 0; i < 12 && c; i++, c = c.parentElement || (c.getRootNode && c.getRootNode().host)) {{ if (!c.querySelectorAll) continue; const b = [...c.querySelectorAll('button')].find(ok); if (b) return b; }} return null; }})()"
    );
    let enabled = tab.eval(&format!("(() => {{ const b = {send_btn}; return !!b && !b.disabled; }})()")).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
    if !send {
        click(tab, &boxjs).await?;
        clear_focused(tab).await?;
        let _ = click(tab, &btn(r"Unterhaltung schließen|Close your conversation|Close conversation", "")).await;
        return Ok(Outcome { sent: false, detail: format!("dry run: message box filled, Send button {}, cleared", if enabled { "ready" } else { "NOT found" }) });
    }
    if !enabled {
        bail!("the Send button isn't available");
    }
    click(tab, &send_btn).await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    Ok(Outcome { sent: true, detail: "message sent".into() })
}

/// A comment on a post (URL /feed/update/urn:li:…).
pub async fn comment(tab: &Tab, post_url: &str, text: &str, send: bool) -> anyhow::Result<Outcome> {
    if !post_url.starts_with("https://www.linkedin.com/feed/update/") && !post_url.starts_with("https://www.linkedin.com/posts/") {
        bail!("not a LinkedIn post URL: {post_url}");
    }
    view(tab, post_url).await?;
    let boxjs = "[...document.querySelectorAll('[contenteditable=true][role=textbox], div.ql-editor[contenteditable=true]')].filter(e => e.getBoundingClientRect().height > 0)[0] || null";
    if !exists(tab, boxjs).await {
        let _ = click(tab, &btn(r"^(Kommentieren|Comment)$", "main")).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    if !click(tab, boxjs).await? {
        bail!("no comment box on this post (comments may be off)");
    }
    tab.send("Input.insertText", serde_json::json!({"text": text})).await?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    // The composer: the nearest ancestor of the editor holding its emoji button and a submit
    // button (never the post's own "Comment" action, which sits outside it).
    let submit = format!(
        "(() => {{ let c = {boxjs}; const ok = (b) => b.getBoundingClientRect().width && /^(Kommentieren|Comment|Posten|Post|Veröffentlichen|Antworten|Reply)$/i.test((b.innerText || '').trim()); \
           for (let i = 0; i < 12 && c; i++, c = c.parentElement) {{ const bs = [...c.querySelectorAll('button')]; \
             if (bs.some(b => /emoji/i.test(b.getAttribute('aria-label') || '')) && bs.some(ok)) return bs.find(ok); }} return null; }})()"
    );
    let enabled = tab.eval(&format!("(() => {{ const b = {submit}; return !!b && !b.disabled; }})()")).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
    if !send {
        click(tab, boxjs).await?;
        clear_focused(tab).await?;
        return Ok(Outcome { sent: false, detail: format!("dry run: comment box filled, submit button {}, cleared", if enabled { "ready" } else { "NOT found" }) });
    }
    if !enabled {
        bail!("the comment button isn't available");
    }
    click(tab, &submit).await?;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let probe: String = text.chars().take(40).collect();
    let shown = tab.eval(&format!("document.body.innerText.includes({})", serde_json::to_string(&probe)?)).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(Outcome { sent: true, detail: if shown { "comment posted".into() } else { "comment sent (not yet visible)".into() } })
}
