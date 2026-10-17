//! Local "toolkits" (ported from Composio's catalogue, but private): everything runs on this
//! Mac, with no hosted accounts or OAuth tokens. Calendar / Reminders / Contacts go through
//! macOS's own apps (JXA via /usr/bin/osascript), so any account the user already added to
//! the Mac (iCloud, Google, Exchange) works. First use triggers macOS's privacy prompt once.

use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

const OSA_TIMEOUT: Duration = Duration::from_secs(45);

/// Runs a JXA script with arguments (passed as argv, never interpolated into the source).
/// JXA for other modules (notes): same argv-only rule.
pub async fn jxa_pub(script: &str, args: &[String]) -> anyhow::Result<Value> {
    jxa(script, args).await
}

async fn jxa(script: &str, args: &[String]) -> anyhow::Result<Value> {
    let mut cmd = tokio::process::Command::new("/usr/bin/osascript");
    cmd.args(["-l", "JavaScript", "-e", script]).args(args).kill_on_drop(true);
    let out = tokio::time::timeout(OSA_TIMEOUT, cmd.output())
        .await
        .context("macOS app didn't answer in time (if a privacy prompt is open, allow access and try again)")??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-1743") || err.contains("Not authorized") {
            bail!("macOS denied access; allow it in System Settings → Privacy & Security → Automation / Calendars / Reminders / Contacts");
        }
        bail!("osascript: {}", err.trim().chars().take(300).collect::<String>());
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
}

/// Onboarding: a harmless read (a count) that makes macOS show its Automation / privacy prompt
/// for that app now, instead of in the middle of a task. Waits up to 2 minutes for the user.
pub async fn probe(app: &str) -> anyhow::Result<String> {
    let script = match app {
        "calendar" => "Application('Calendar').calendars.length",
        "reminders" => "Application('Reminders').lists.length",
        "notes" => "Application('Notes').folders.length",
        "contacts" => "Application('Contacts').people.length",
        "mail" => "Application('Mail').accounts.length",
        _ => bail!("unknown app {app}"),
    };
    let mut cmd = tokio::process::Command::new("/usr/bin/osascript");
    cmd.args(["-l", "JavaScript", "-e", script]).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(120), cmd.output()).await.context("no answer yet — allow the prompt, then try again")??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-1743") || err.contains("Not authorized") {
            bail!("denied — allow it in System Settings › Privacy & Security › Automation");
        }
        bail!("{}", err.trim().chars().take(200).collect::<String>());
    }
    Ok("allowed".into())
}

const CAL_LIST: &str = r#"function run(argv) {
  const [fromIso, toIso, q] = argv; const Cal = Application('Calendar');
  const from = new Date(fromIso), to = new Date(toIso); const out = [];
  for (const c of Cal.calendars()) {
    let evs; try { evs = c.events.whose({_and: [{startDate: {_greaterThan: from}}, {startDate: {_lessThan: to}}]})(); } catch (e) { continue; }
    for (const e of evs) {
      const t = e.summary() || '';
      if (q && !t.toLowerCase().includes(q.toLowerCase())) continue;
      out.push({title: t, start: e.startDate().toISOString(), end: e.endDate().toISOString(), all_day: e.alldayEvent(), calendar: c.name(), location: e.location() || ''});
      if (out.length >= 60) break;
    }
  }
  out.sort((a, b) => a.start < b.start ? -1 : 1);
  return JSON.stringify(out.slice(0, 40));
}"#;

const CAL_CREATE: &str = r#"function run(argv) {
  const [title, startIso, endIso, calName, location, notes] = argv; const Cal = Application('Calendar');
  let cal = null;
  if (calName) { try { cal = Cal.calendars.byName(calName); cal.name(); } catch (e) { cal = null; } }
  if (!cal) cal = Cal.calendars().find(c => { try { return c.writable(); } catch (e) { return false; } });
  if (!cal) throw new Error('no writable calendar');
  const ev = Cal.Event({summary: title, startDate: new Date(startIso), endDate: new Date(endIso), location: location || '', description: notes || ''});
  cal.events.push(ev);
  return JSON.stringify({created: title, calendar: cal.name(), start: startIso});
}"#;

// Bulk reads: two Apple events per list (names, due dates) instead of two per reminder —
// 12 s instead of 2 min 16 s for ~15 open reminders (the per-item version timed out).
const REM_LIST: &str = r#"function run(argv) {
  const [listName] = argv; const R = Application('Reminders'); const out = [];
  const lists = listName ? [R.lists.byName(listName)] : R.lists();
  for (const l of lists) {
    let name, t, d; try { name = l.name(); const w = l.reminders.whose({completed: false}); t = w.name(); d = w.dueDate(); } catch (e) { continue; }
    for (let i = 0; i < t.length && out.length < 50; i++) out.push({title: t[i], due: d[i] ? d[i].toString().slice(0, 21) : null, list: name});
  }
  return JSON.stringify(out);
}"#;

const REM_CREATE: &str = r#"function run(argv) {
  const [title, dueIso, listName] = argv; const R = Application('Reminders');
  let l = null; if (listName) { try { l = R.lists.byName(listName); l.name(); } catch (e) { l = null; } }
  if (!l) l = R.defaultList();
  const props = {name: title}; if (dueIso) props.dueDate = new Date(dueIso);
  l.reminders.push(R.Reminder(props));
  return JSON.stringify({created: title, list: l.name(), due: dueIso || null});
}"#;

// Mark one open reminder done: exact title first, else the single one containing the words.
// Ambiguous → the candidates come back and nothing changes.
const REM_COMPLETE: &str = r#"function run(argv) {
  const [q, listName] = argv; const R = Application('Reminders');
  const lists = listName ? [R.lists.byName(listName)] : R.lists(); const all = [];
  for (const l of lists) { let rs; try { rs = l.reminders.whose({completed: false})(); } catch (e) { continue; } for (const r of rs) all.push([r, l.name()]); }
  const ql = q.toLowerCase();
  let hits = all.filter(([r]) => r.name().toLowerCase() === ql);
  if (!hits.length) hits = all.filter(([r]) => r.name().toLowerCase().includes(ql) || ql.includes(r.name().toLowerCase()));
  if (hits.length !== 1) return JSON.stringify({done: false, candidates: hits.slice(0, 8).map(([r, l]) => r.name() + ' (' + l + ')')});
  hits[0][0].completed = true;
  return JSON.stringify({done: true, title: hits[0][0].name(), list: hits[0][1]});
}"#;

const CONTACTS: &str = r#"function run(argv) {
  const [q] = argv; const C = Application('Contacts');
  const ps = C.people.whose({name: {_contains: q}})().slice(0, 8);
  return JSON.stringify(ps.map(p => ({name: p.name(), emails: p.emails().map(e => e.value()), phones: p.phones().map(x => x.value()), company: p.organization() || ''})));
}"#;

pub async fn calendar_events(from: chrono::DateTime<chrono::Local>, to: chrono::DateTime<chrono::Local>, query: &str) -> anyhow::Result<Value> {
    let mut v = jxa(CAL_LIST, &[from.to_rfc3339(), to.to_rfc3339(), query.to_string()]).await?;
    for e in v.as_array_mut().into_iter().flatten() {
        for k in ["start", "end"] {
            if let Some(t) = e[k].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
                e[k] = json!(t.with_timezone(&chrono::Local).format("%a %Y-%m-%d %H:%M").to_string());
            }
        }
    }
    Ok(v)
}

/// Raw events (RFC 3339 times) for conflict checks.
pub async fn calendar_events_raw(from: chrono::DateTime<chrono::Local>, to: chrono::DateTime<chrono::Local>) -> anyhow::Result<Value> {
    jxa(CAL_LIST, &[from.to_rfc3339(), to.to_rfc3339(), String::new()]).await
}

/// Mail.app is configured with accounts: whether the user has any (send fallback when no
/// IMAP/SMTP app password is set up).
pub fn mail_app_ready() -> bool {
    std::process::Command::new("defaults").args(["read", "com.apple.mail", "AccountOrdering"]).output().is_ok_and(|o| o.status.success() && o.stdout.len() > 4)
}

const MAIL_SEND: &str = r#"function run(argv) {
  const [toJ, ccJ, subject, body] = argv; const Mail = Application('Mail');
  const m = Mail.OutgoingMessage({subject: subject, content: body, visible: false});
  Mail.outgoingMessages.push(m);
  for (const a of JSON.parse(toJ)) m.toRecipients.push(Mail.ToRecipient({address: a}));
  for (const a of JSON.parse(ccJ)) m.ccRecipients.push(Mail.CcRecipient({address: a}));
  m.send();
  return JSON.stringify({sent: true});
}"#;

/// Lab / E2E runs set SAM_NO_REAL_APPS=1: anything that would change the user's Calendar,
/// Reminders, Notes or Mail refuses (tests never touch real data).
pub fn real_writes_ok() -> anyhow::Result<()> {
    if std::env::var("SAM_NO_REAL_APPS").is_ok_and(|v| v == "1") {
        bail!("test mode: not changing your real apps");
    }
    Ok(())
}

/// Sends through Mail.app (the user's account), without showing a compose window. Only ever
/// called for a confirmed pending action.
pub async fn mail_send(to: &[String], cc: &[String], subject: &str, body: &str) -> anyhow::Result<Value> {
    real_writes_ok()?;
    if to.is_empty() {
        bail!("no recipient");
    }
    jxa(MAIL_SEND, &[serde_json::to_string(to)?, serde_json::to_string(cc)?, subject.into(), body.into()]).await
}

// The message a pointed-at Mail window shows: the viewer's selection when its subject fits the
// window title (a message window is titled with the subject), else the newest inbox match.
const MAIL_FIND: &str = r#"function run(argv) {
  const [title] = argv; const Mail = Application('Mail');
  const norm = s => String(s || '').replace(/^\s*((re|aw|fwd?|wg)\s*:\s*)+/i, '').trim().toLowerCase();
  const want = norm(title); let msg = null;
  try { for (const v of Mail.messageViewers()) for (const m of (v.selectedMessages() || [])) {
    if (!msg && (!want || want.includes(norm(m.subject())) || norm(m.subject()).includes(want))) msg = m; } } catch (e) {}
  if (!msg && want.length >= 4) {
    const hits = Mail.inbox.messages.whose({subject: {_contains: want.slice(0, 80)}})();
    hits.sort((a, b) => b.dateReceived() - a.dateReceived()); msg = hits[0] || null;
  }
  if (!msg) return 'null';
  return JSON.stringify({message_id: msg.messageId(), from: msg.sender(), subject: msg.subject(),
    date: msg.dateReceived().toISOString(), text: String(msg.content() || '').slice(0, 3000)});
}"#;

/// Finds the message behind a pointed-at Mail window (by its title), with the real sender
/// address, which Mail's window only shows as a name.
pub async fn mail_find(window_title: &str) -> anyhow::Result<Option<crate::email::Message>> {
    let v = jxa(MAIL_FIND, &[window_title.to_string()]).await?;
    if v.is_null() {
        return Ok(None);
    }
    let f = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Ok(Some(crate::email::Message { uid: 0, date: f("date"), from: f("from"), to: String::new(), subject: f("subject"), message_id: v["message_id"].as_str().map(str::to_string), text: f("text"), attachments: vec![], truncated: false }))
}

// Mail's own reply on the original (threading headers, quoted original kept below the body).
// Mail stores Message-IDs without angle brackets. `dry` builds the reply and discards it.
const MAIL_REPLY: &str = r#"function run(argv) {
  const [id, subject, toJ, ccJ, body, dry] = argv; const Mail = Application('Mail');
  // Subject search is indexed (~0.1 s over 20k mails); messageId alone would scan every message.
  const want = id.replace(/^<|>$/g, ''), subj = subject.replace(/^\s*((re|aw|fwd?|wg)\s*:\s*)+/i, '').trim();
  const hits = Mail.inbox.messages.whose({subject: {_contains: subj.slice(0, 80)}})().filter(m => m.messageId() === want);
  if (!hits.length) return JSON.stringify({found: false});
  const o = hits[0];
  const r = Mail.reply(o, {openingWindow: false, replyToAll: false});
  const quoted = String(o.content() || '').split('\n').map(l => '> ' + l).join('\n');
  r.content = body + '\n\nOn ' + o.dateReceived().toLocaleString() + ', ' + o.sender() + ' wrote:\n' + quoted;
  for (const a of JSON.parse(ccJ)) r.ccRecipients.push(Mail.CcRecipient({address: a}));
  const info = {found: true, subject: r.subject(), to: r.toRecipients().map(t => t.address())};
  // Mail addresses the reply to the original sender; anyone else gets a new message instead.
  const same = JSON.stringify(info.to.map(a => a.toLowerCase()).sort()) === JSON.stringify(JSON.parse(toJ).map(a => a.toLowerCase()).sort());
  if (dry === '1' || !same) { r.delete(); info.mismatch = !same; return JSON.stringify(info); }
  r.send(); info.sent = true; return JSON.stringify(info);
}"#;

/// Replies through Mail.app on the original message. Returns Ok(false) when the original
/// isn't in the inbox, or `to` isn't exactly the original's sender: `sent` is then absent and
/// the caller sends a new message instead. Only ever called for a confirmed pending action.
pub async fn mail_reply(message_id: &str, subject: &str, to: &[String], cc: &[String], body: &str, dry: bool) -> anyhow::Result<Value> {
    if !dry {
        real_writes_ok()?;
    }
    jxa(MAIL_REPLY, &[message_id.into(), subject.into(), serde_json::to_string(to)?, serde_json::to_string(cc)?, body.into(), if dry { "1" } else { "0" }.into()]).await
}

pub async fn calendar_create(title: &str, start: &str, end: &str, calendar: &str, location: &str, notes: &str) -> anyhow::Result<Value> {
    real_writes_ok()?;
    for t in [start, end] {
        chrono::DateTime::parse_from_rfc3339(t).with_context(|| format!("bad time {t} (need RFC 3339, e.g. 2026-10-09T19:00:00+02:00)"))?;
    }
    jxa(CAL_CREATE, &[title.into(), start.into(), end.into(), calendar.into(), location.into(), notes.into()]).await
}

pub async fn reminders(list: &str) -> anyhow::Result<Value> {
    jxa(REM_LIST, &[list.to_string()]).await
}

pub async fn reminder_create(title: &str, due: &str, list: &str) -> anyhow::Result<Value> {
    real_writes_ok()?;
    if !due.is_empty() {
        chrono::DateTime::parse_from_rfc3339(due).with_context(|| format!("bad due time {due}"))?;
    }
    jxa(REM_CREATE, &[title.into(), due.into(), list.into()]).await
}

/// Marks an open reminder done (reversible: it's a checkbox). Ambiguous titles change nothing.
pub async fn reminder_complete(title: &str, list: &str) -> anyhow::Result<String> {
    real_writes_ok()?;
    if title.trim().len() < 2 {
        bail!("which reminder?");
    }
    let v = jxa(REM_COMPLETE, &[title.trim().to_string(), list.to_string()]).await?;
    if v["done"] == true {
        return Ok(format!("marked done: \"{}\" ({})", v["title"].as_str().unwrap_or(title), v["list"].as_str().unwrap_or("")));
    }
    let c: Vec<String> = v["candidates"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
    if c.is_empty() { bail!("no open reminder matches \"{title}\"") } else { bail!("several reminders match \"{title}\": {} — which one?", c.join("; ")) }
}

/// The user's own phone numbers (Contacts › My Card): "message myself".
pub async fn my_phones() -> anyhow::Result<Vec<String>> {
    let v = jxa("function run(){const c=Application('Contacts').myCard();return JSON.stringify(c?c.phones().map(p=>p.value()):[])}", &[]).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

pub async fn contacts(query: &str) -> anyhow::Result<Value> {
    if query.trim().len() < 2 {
        bail!("name too short");
    }
    jxa(CONTACTS, &[query.trim().to_string()]).await
}

/// Open-Meteo (no API key): current conditions + daily forecast for a place name.
pub async fn weather(http: &reqwest::Client, place: &str, days: u32) -> anyhow::Result<Value> {
    let g: Value = http
        .get(url::Url::parse_with_params("https://geocoding-api.open-meteo.com/v1/search", &[("name", place), ("count", "1"), ("language", "en")])?)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .json()
        .await?;
    let loc = g["results"].get(0).with_context(|| format!("place not found: {place}"))?;
    let (lat, lon) = (loc["latitude"].as_f64().unwrap_or(0.0), loc["longitude"].as_f64().unwrap_or(0.0));
    let f: Value = http
        .get(url::Url::parse_with_params("https://api.open-meteo.com/v1/forecast", &[
            ("latitude", lat.to_string()),
            ("longitude", lon.to_string()),
            ("current", "temperature_2m,apparent_temperature,weather_code,wind_speed_10m,precipitation".into()),
            ("daily", "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max".into()),
            ("timezone", "auto".into()),
            ("forecast_days", days.clamp(1, 7).to_string()),
        ])?)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .json()
        .await?;
    let c = &f["current"];
    let d = &f["daily"];
    let daily: Vec<Value> = (0..d["time"].as_array().map(|a| a.len()).unwrap_or(0))
        .map(|i| json!({"date": d["time"][i], "sky": wmo(d["weather_code"][i].as_i64().unwrap_or(-1)), "max_c": d["temperature_2m_max"][i], "min_c": d["temperature_2m_min"][i], "rain_chance_pct": d["precipitation_probability_max"][i]}))
        .collect();
    Ok(json!({
        "place": format!("{}, {}", loc["name"].as_str().unwrap_or(place), loc["country"].as_str().unwrap_or("")),
        "now": {"temp_c": c["temperature_2m"], "feels_c": c["apparent_temperature"], "sky": wmo(c["weather_code"].as_i64().unwrap_or(-1)), "wind_kmh": c["wind_speed_10m"], "precip_mm": c["precipitation"]},
        "daily": daily
    }))
}

fn wmo(code: i64) -> &'static str {
    match code {
        0 => "clear",
        1 | 2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "fog",
        51..=57 => "drizzle",
        61..=67 => "rain",
        71..=77 => "snow",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95..=99 => "thunderstorm",
        _ => "unknown",
    }
}

/// A note in Sam's own notes folder (catalogued, searchable by name).
pub fn note_save(data: &std::path::Path, title: &str, text: &str) -> anyhow::Result<std::path::PathBuf> {
    let dir = data.join("artifacts/notes");
    std::fs::create_dir_all(&dir)?;
    let mut p = dir.join(format!("{}_{}.md", chrono::Local::now().format("%Y-%m-%d"), crate::artifacts::slug(title, 60)));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{}_{}-{n}.md", chrono::Local::now().format("%Y-%m-%d"), crate::artifacts::slug(title, 60)));
        n += 1;
    }
    std::fs::write(&p, format!("# {title}\n\n{}\n", text.trim()))?;
    if let Ok(c) = crate::artifacts::Catalog::open(data) {
        let _ = c.add("note", title, None, None, &p);
    }
    Ok(p)
}
