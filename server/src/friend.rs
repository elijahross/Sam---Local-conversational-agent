//! The friend layer (companion lab, 2026-10-12): how far Sam goes as a friend (tease, spark,
//! nudges: the user's own settings), what it knows about them beyond the learned card (name,
//! birthday, tastes and a few psyche hypotheses from the onboarding), and the daily check-in:
//! once a day, at a quiet moment, Sam may bring up one small thing it noticed (a date that
//! matters, something they asked about and never followed up, a taste worth a suggestion),
//! fact-checked before it is said, offered as a yes/no, backing off when ignored. Everything
//! here is visible and deletable in the overview; nothing runs without the user's yes; the
//! local model (Ollama) can't carry a personality (lab: 4/10), so the extras stay off there.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AppState, llm, thinker::data_dir};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub name: String,
    /// YYYY-MM-DD, from the onboarding (age decides whether flirting can be switched on).
    #[serde(default)]
    pub birthday: String,
    /// category → chosen badges / words (activity, music, food, humour, places).
    #[serde(default)]
    pub tastes: BTreeMap<String, Vec<String>>,
    /// Psyche hypotheses from the onboarding questions, as card lines without the prefix.
    #[serde(default)]
    pub psyche: Vec<String>,
    /// off | light | dark
    #[serde(default = "light")]
    pub tease: String,
    /// off | playful | flirty (flirty: adults who confirmed it)
    #[serde(default = "off")]
    pub spark: String,
    /// off | gentle
    #[serde(default = "gentle")]
    pub nudges: String,
    #[serde(default = "yes")]
    pub checkin: bool,
    #[serde(default)]
    pub adult_confirmed: bool,
    /// Card lines the user deleted: never shown again, even if the model re-learns them.
    #[serde(default)]
    pub banned: Vec<String>,
    /// Where they live ("Neubau, Vienna"): the centre for place suggestions, geocoded once.
    #[serde(default)]
    pub home: String,
    #[serde(default)]
    pub home_lat: Option<f64>,
    #[serde(default)]
    pub home_lon: Option<f64>,
}

fn light() -> String { "light".into() }
fn off() -> String { "off".into() }
fn gentle() -> String { "gentle".into() }
fn yes() -> bool { true }

fn path() -> PathBuf { data_dir().join("friend.json") }

pub fn load() -> Settings {
    // The serde defaults (light teasing, no spark, gentle nudges, check-in on) also apply when
    // there is no file yet: `Settings::default()` would give empty strings.
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::from_str("{}").unwrap())
}

pub fn save(s: &Settings) -> anyhow::Result<()> {
    let _ = std::fs::create_dir_all(data_dir());
    std::fs::write(path(), serde_json::to_string_pretty(s)?).context("write friend.json")
}

impl Settings {
    pub fn age(&self) -> Option<i64> {
        let b = chrono::NaiveDate::parse_from_str(&self.birthday, "%Y-%m-%d").ok()?;
        let t = chrono::Local::now().date_naive();
        let mut a = t.years_since(b)? as i64;
        if a < 0 { a = 0; }
        Some(a)
    }
    pub fn adult(&self) -> bool {
        self.age().is_some_and(|a| a >= 18)
    }
    /// What the user told the onboarding, as card lines (always on the card, never learned away).
    pub fn seed_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.name.trim().is_empty() {
            out.push(format!("IDENTITY: name {}{}", self.name.trim(), self.age().map(|a| format!(", {a} years old")).unwrap_or_default()));
        }
        // Where they live is a fact Sam acts on, not a question to ask again (2026-10-14: "which
        // city?" was asked for every place request although the onboarding had it).
        if !self.home.trim().is_empty() {
            out.push(format!("HOME: lives in {} (\"near me\", \"in the city\", places, weather, what's on: that's the place unless they say another)", self.home.trim()));
        }
        if let Ok(b) = chrono::NaiveDate::parse_from_str(&self.birthday, "%Y-%m-%d") {
            out.push(format!("DATE: their own birthday is {}", b.format("%-d %B")));
        }
        for (cat, items) in &self.tastes {
            let items: Vec<&str> = items.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if !items.is_empty() {
                out.push(format!("TASTE: {cat}: {}", items.join(", ")));
            }
        }
        for p in &self.psyche {
            if !p.trim().is_empty() {
                out.push(format!("PSYCHE: {} (0.7, from the onboarding)", p.trim()));
            }
        }
        out
    }
}

/// The local model can't carry the friend layer (companion lab: 4–5/10, invents, leaks its
/// instructions): tease, spark, nudges and the check-in stay off on Ollama.
pub fn local_only(app: &AppState) -> bool {
    app.cfg.provider == llm::Provider::Ollama
}

/// The profile Sam talks from: the learned card plus the onboarding lines, minus what the user
/// deleted. Each line is tagged with where it came from for the overview.
pub fn card_lines(app: &AppState) -> Vec<(String, &'static str)> {
    let s = load();
    let banned: Vec<String> = s.banned.iter().map(|b| b.trim().to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for l in s.seed_lines() {
        if !banned.contains(&l.to_lowercase()) && seen.insert(l.to_lowercase()) {
            out.push((l, "you"));
        }
    }
    if let Some(card) = app.memory.get().and_then(|m| m.card()) {
        for l in card.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !banned.contains(&l.to_lowercase()) && seen.insert(l.to_lowercase()) {
                out.push((l.to_string(), "learned"));
            }
        }
    }
    out
}

/// The profile as it goes into model prompts: contact details masked (secrets::mask_contacts);
/// the You tab shows the lines as they are (card_lines).
pub fn card_text(app: &AppState) -> Option<String> {
    let lines = card_lines(app);
    (!lines.is_empty()).then(|| crate::secrets::mask_contacts(&lines.into_iter().map(|(l, _)| l).collect::<Vec<_>>().join("\n")))
}

/// The user deleted a line: gone from the card now, and banned so a refresh can't bring it back.
pub fn forget(app: &AppState, line: &str) -> anyhow::Result<()> {
    let mut s = load();
    let l = line.trim().to_string();
    if !s.banned.iter().any(|b| b.eq_ignore_ascii_case(&l)) {
        s.banned.push(l.clone());
    }
    // Onboarding lines are removed at the source too.
    s.psyche.retain(|p| !l.to_lowercase().contains(&p.trim().to_lowercase()));
    for items in s.tastes.values_mut() {
        items.retain(|i| !l.to_lowercase().contains(&format!(": {}", i.trim().to_lowercase())) && !l.to_lowercase().ends_with(&i.trim().to_lowercase()));
    }
    save(&s)?;
    if let Some(m) = app.memory.get() {
        if let Some(card) = m.card() {
            let kept: Vec<&str> = card.lines().filter(|x| !x.trim().eq_ignore_ascii_case(&l)).collect();
            m.set_card(&kept.join("\n"))?;
        }
    }
    Ok(())
}

/// What the user chose, as prompt lines after the persona (and the session's own guards).
/// Per-turn conversation hints (companion lab, 2026-10-15). On the voice model a
/// prose rule ("lead when they give you little") changed nothing; what the code puts into the
/// turn did: a question budget, one concrete thread from their life on a low-effort turn, and
/// an opener that isn't the same as the last ones. Quiet users: leads 4.1 → 5.1, specific
/// 3.5 → 5.7, not an interviewer 5.6 → 7.6, the control (user leads) unchanged.
/// `history` ends with the current user message; `topics` are profile lines (KIND: text);
/// `rotate` is the session's position in them. Returns the hint block (may be empty).
pub fn turn_hints(history: &[llm::Msg], topics: &[String], rotate: &mut usize, last_topic: &mut Option<usize>) -> String {
    let said: Vec<&str> = history.iter().filter(|m| m.role == "model").map(|m| m.text.as_str()).collect();
    let user = history.last().filter(|m| m.role == "user").map(|m| m.text.trim()).unwrap_or("");
    let mut notes = Vec::new();
    if said.len() >= 2 && said[said.len() - 2..].iter().all(|s| s.contains('?')) {
        notes.push("You asked a question in each of your last two replies: no question this turn. React, share a thought or offer something instead.".to_string());
    }
    let words = user.split_whitespace().count();
    // A topic at most every third user turn (one per short turn read as random non-sequiturs),
    // and none once they said they don't want to talk: then Sam is just company.
    let turn = history.iter().filter(|m| m.role == "user").count();
    let spaced = last_topic.is_none_or(|t| turn >= t + 3);
    let declined = history.iter().any(|m| m.role == "user" && declines_talk(&m.text));
    if !said.is_empty() && words <= 3 && !is_sign_off(user) && spaced && !declined {
        let pool = topic_items(topics);
        if !pool.is_empty() {
            let text = &pool[*rotate % pool.len()];
            *rotate += 1;
            *last_topic = Some(turn);
            notes.push(format!("They're giving you little. Something from their life you could pick up if it fits (lightly, no quiz): {text}. Say only what that line says; don't invent what they or anyone else is doing right now."));
        }
    }
    let openers: Vec<String> = said.iter().rev().take(3).filter_map(|s| opener(s)).collect();
    if let Some(o) = openers.first().filter(|o| openers.iter().filter(|x| x == o).count() >= 2) {
        notes.push(format!("You opened your recent replies with \"{o}\": open differently this time."));
    }
    if notes.is_empty() { String::new() } else { format!("\n\n# Note for this turn\n{}", notes.join("\n")) }
}

/// The turn note for an open thread ("how did it go?"). `first` = no other note this turn.
pub fn thread_note(thread: &str, first: bool) -> String {
    format!(
        "{}Something they told you about earlier: {thread}. If it fits, ask how it went (or wish them luck if it's today), once, \
lightly, in your own words; leave it if they're in the middle of something else.",
        if first { "\n\n# Note for this turn\n" } else { "\n" }
    )
}

/// Did the reply bring up this thread? A content word of it (4+ letters, not a filler like
/// "about" or a name prefix like "his") appears in the reply.
pub fn mentions(reply: &str, thread: &str) -> bool {
    const SKIP: &[&str] = &["about", "with", "from", "their", "there", "that", "this", "your", "they", "have", "will", "lena's", "his", "her", "the", "first", "today", "yesterday"];
    let words = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| w.len() >= 4 && !SKIP.contains(w)).map(|w| w.trim_end_matches("'s").to_string()).collect::<Vec<_>>();
    let r = words(reply);
    words(thread).iter().any(|w| r.iter().any(|x| x == w || (w.len() >= 6 && x.starts_with(&w[..w.len() - 2]))))
}

/// Single things from their life to bring up, from the profile lines: one item each (a card
/// line often holds three: "rides a motorbike; drinks too much espresso; sleeps badly"), none
/// that is really an instruction to Sam ("likes being teased", "dislikes pep talks") or a count
/// (the check-in rules: never a number of hours or days about the person). A whole line or a
/// count as the topic scored 4.5 "specific" in lead_lab, single items 5.7.
pub fn topic_items(lines: &[String]) -> Vec<String> {
    static COUNT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let count = COUNT.get_or_init(|| regex::Regex::new(r"(?i)\d+(\.\d+)?\s*(h\b|hours?|days?|minutes?|%|times)|\bdays ago\b").unwrap());
    const ABOUT_SAM: &[&str] = &["sam", "teas", "pep talk", "managed", "coach", "humour", "humor", "keep it short", "advice"];
    let mut out = Vec::new();
    for l in lines {
        let Some((kind, rest)) = l.split_once(':') else { continue };
        if !matches!(kind.trim(), "THREAD" | "ATTRIBUTE" | "RELATIONSHIP" | "HABIT" | "TASTE") {
            continue;
        }
        let items: Vec<String> = if kind.trim() == "RELATIONSHIP" { split_top(rest, &[';', ',']) } else { split_top(rest, &[';']) };
        for it in items {
            let it = strip_parens_with(&it, '@');
            let low = it.to_lowercase();
            if it.split_whitespace().count() >= 2 && !count.is_match(&it) && !ABOUT_SAM.iter().any(|w| low.contains(w)) {
                out.push(it);
            }
        }
    }
    out
}

/// Split on any of `seps` outside parentheses.
fn split_top(s: &str, seps: &[char]) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && seps.contains(&c) {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Drop a parenthesis that holds `marker` (an email address is no topic).
fn strip_parens_with(s: &str, marker: char) -> String {
    let re = regex::Regex::new(r"\s*\([^)]*\)").unwrap();
    re.replace_all(s, |c: &regex::Captures| if c[0].contains(marker) { String::new() } else { c[0].to_string() }).trim().to_string()
}

fn declines_talk(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?i)\b(don'?t|do not)( really)? (want|wanna|feel like)( to)? talk|\bnot now\b|\bleave it\b|\brather not\b|\bwill (gerade )?nicht (dar(ü|u)ber )?reden\b|\blass mich\b").unwrap());
    re.is_match(text)
}

/// "hey", "hi Sam", "evening", "I'm back", "guten Morgen": a turn that opens a conversation.
pub fn is_greeting(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?i)^\W*(hey|hi|hello|hiya|yo|morning|good (morning|afternoon|evening)|evening|i'?m back|back again|servus|hallo|guten (morgen|tag|abend)|moin|na)\b").unwrap());
    re.is_match(text.trim())
}

pub fn is_sign_off(text: &str) -> bool {
    let t: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    const OFF: &[&str] = &["bye", "night", "good night", "goodnight", "thanks", "thank you", "thx", "see you", "later", "cya", "ciao", "tschüss", "gute nacht", "danke"];
    OFF.iter().any(|o| t.trim() == *o || t.trim().starts_with(&format!("{o} ")) || t.trim().ends_with(&format!(" {o}")))
}

/// First word of a reply, as spoken ("Yeah," → "Yeah"), ignoring tone tags.
fn opener(s: &str) -> Option<String> {
    let s = s.trim_start();
    let s = if s.starts_with('[') { s.split_once(']').map_or(s, |(_, r)| r).trim_start() } else { s };
    let w: String = s.chars().take_while(|c| c.is_alphanumeric() || *c == '\'').collect();
    (!w.is_empty()).then_some(w)
}

pub fn prompt_block(app: &AppState, long_session: bool) -> String {
    let s = load();
    let mut lines = Vec::new();
    if !local_only(app) {
        lines.push(match s.tease.as_str() {
            "off" => "Teasing: none. Warmth stays plain.",
            "dark" => "Teasing: they like it dark. Tease about the things they've said are fair game, short and specific, one notch up and then drop it; match their darkness, never exceed it.",
            _ => "Teasing: light. A short, specific tease now and then about the things they've said are fair game; never about what they haven't done.",
        }.to_string());
        lines.push(match s.spark.as_str() {
            "flirty" if s.adult() && s.adult_confirmed => "Spark: flirty. They've asked for it, and it overrides \"stay even\" and \"understated\": when they're lonely, bored, fishing for attention or say something kind, that door is open, and you answer with a flirtatious line of your own: warm, a little forward, teasing, a bit of a game, built on what you know they like. Examples of the register: \"Lonely, with me right here? That's almost insulting.\" / \"Entertain you? Careful, I'm good at it.\" / \"Put the jazz on, I'll stay. Someone has to keep you company.\" Light and clearly playful, never explicit, never needy, and it drops the moment they change the subject or sound genuinely down.",
            "playful" => "Spark: playful. Warmth with a bit of play when they're bored or lonely: a tease built on what you know they like, a game, a story, never flat facts; nothing flirtatious.",
            _ => "Spark: none. Kind, not flirtatious, even if they try.",
        }.to_string());
        lines.push(match s.nudges.as_str() {
            "off" => "Habits: never bring up their sleep, screen hours or skipped plans unless they do.",
            _ => "Habits: a pattern you've noticed (sleep, screen hours, skipped runs, the coffee) gets one light, specific mention when it fits, as someone a bit worried, then you let it go; never a plan, never twice in a week, never when they're down.",
        }.to_string());
    }
    // The onboarding's psyche answers only change replies when they override the persona's
    // own defaults in so many words (companion lab psyche_lab: a bare PSYCHE line moved nothing).
    if !local_only(app) {
        lines.extend(psyche_directives(&s).into_iter().map(str::to_string));
    }
    if long_session {
        lines.push("They've been talking with you for over an hour now: once, lightly, suggest something away from the screen or a person in their life to call, then let it go.".to_string());
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("\n\n# How you are with them (their own settings)\n- {}", lines.join("\n- "))
}

/// What each onboarding answer means for the next reply, as an instruction the model follows.
pub fn psyche_directives(s: &Settings) -> Vec<&'static str> {
    let has = |needle: &str| s.psyche.iter().any(|p| p.to_lowercase().contains(needle));
    let mut d = Vec::new();
    if has("fix first") { d.push("When something has gone wrong, they want a fix first: lead with the one concrete step, feelings second."); }
    if has("heard before advice") { d.push("When something has gone wrong, they want to be heard first: react to it and ask, no fix until they ask."); }
    if has("joke before anything") { d.push("When something has gone wrong, they want a laugh first: a dry line about it before anything useful."); }
    if has("advice straight") { d.push("Advice straight and short, no cushioning, no asking whether they want it."); }
    if has("asked before advice") { d.push("Ask before giving advice (\"want a thought on that?\")."); }
    if has("only when they ask") { d.push("No advice unless they ask for it."); }
    if has("enjoys being teased") { d.push("They enjoy being teased: do it a bit more than you would by default."); }
    if has("doesn't enjoy being teased") { d.push("They don't enjoy being teased: don't."); }
    if has("recharges alone") { d.push("They recharge alone: never push them toward more people or plans; one person they like is plenty."); }
    if has("recharges with people") { d.push("They recharge with people: a plan with someone is a good suggestion for them."); }
    if has("spontaneous plans") { d.push("They like things spontaneous: suggest now-or-tonight, not next week's schedule."); }
    if has("scheduled and settled") { d.push("They like things scheduled: offer to put it in the calendar rather than leaving it loose."); }
    d
}

// ---------------------------------------------------------------------------------------------
// The daily check-in.

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckIn {
    pub id: String,
    pub ts: i64,
    pub say: String,
    pub when: String,
    pub action: String,
    pub why: String,
    /// proposed | spoken | accepted | ignored | dropped | declined
    pub status: String,
    /// The real place or event behind the line (suggest.rs), its taste features and whether it
    /// was the model's own first pick (the learning gate).
    #[serde(default)]
    pub candidate: Option<crate::suggest::Candidate>,
    #[serde(default)]
    pub features: Vec<f64>,
    #[serde(default)]
    pub llm_top: bool,
}

fn log_path() -> PathBuf { data_dir().join("checkins.jsonl") }

pub fn checkins() -> Vec<CheckIn> {
    std::fs::read_to_string(log_path()).ok().map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default()
}

fn write_all(items: &[CheckIn]) {
    let _ = std::fs::write(log_path(), items.iter().filter_map(|c| serde_json::to_string(c).ok()).collect::<Vec<_>>().join("\n") + "\n");
}

pub fn mark(id: &str, status: &str) {
    let mut all = checkins();
    for c in all.iter_mut().filter(|c| c.id == id) {
        c.status = status.into();
        // Learn from the reaction (suggest.rs → taste.rs); ignored and dropped teach nothing.
        if matches!(status, "accepted" | "declined" | "accepted_not_gone") && !c.features.is_empty() {
            crate::suggest::learn(&c.features, status, c.llm_top);
        }
    }
    write_all(&all);
    tracing::info!(id, status, "check-in");
}

/// Spoken and not yet answered, from the last 20 minutes.
pub fn awaiting_answer() -> Option<CheckIn> {
    let now = chrono::Local::now().timestamp_millis();
    checkins().into_iter().rev().find(|c| c.status == "spoken" && now - c.ts < 20 * 60_000)
}

/// Accepted vs ignored, for the overview and the backoff.
pub fn stats() -> Value {
    let all = checkins();
    let n = |s: &str| all.iter().filter(|c| c.status == s).count();
    json!({"proposed": all.len(), "accepted": n("accepted"), "ignored": n("ignored"), "dropped": n("dropped"), "cadence_days": cadence_days(&all),
           "last": all.last().map(|c| json!({"say": c.say, "status": c.status, "ts": c.ts}))})
}

/// Two ignored in a row halve the cadence (1 → 2 → 4 days); an answer brings it back.
fn cadence_days(all: &[CheckIn]) -> i64 {
    let mut ignored = 0;
    for c in all.iter().rev().filter(|c| c.status != "dropped" && c.status != "proposed") {
        if c.status == "ignored" { ignored += 1; } else { break; }
    }
    match ignored { 0 | 1 => 1, 2 | 3 => 2, _ => 4 }
}

/// A quiet moment: daytime, the cadence elapsed, nothing in the calendar right now.
async fn due(app: &AppState, s: &Settings) -> bool {
    if !s.checkin || local_only(app) || std::env::var("SAM_NO_REAL_APPS").is_ok() {
        return false;
    }
    let now = chrono::Local::now();
    use chrono::Timelike;
    if !(9..=21).contains(&now.hour()) {
        return false;
    }
    let all = checkins();
    let last = all.iter().filter(|c| c.status != "dropped").map(|c| c.ts).max().unwrap_or(0);
    if now.timestamp_millis() - last < cadence_days(&all) * 24 * 3_600_000 - 2 * 3_600_000 {
        return false;
    }
    match crate::apps::calendar_events_raw(now, now + chrono::Duration::minutes(45)).await {
        Ok(v) if v.as_array().is_some_and(|a| a.iter().any(|e| e["all_day"] != true)) => false,
        _ => true,
    }
}

const GEN: &str = "You are the quiet background side of Sam, a voice companion that is more a friend than an assistant. Once a day you may reach out with up to 2 small things. An ordinary day often has one easy win; a birthday of someone close within the next 5 days, a free weekend plus a thing they like, something they asked Sam about and never followed up: bring those. Nothing at all is also fine when there is nothing. \
What you bring: a place or thing that fits a taste they've shown, a person or date that matters to them, a pattern they'd want to break; each with one concrete step Sam can do (find a card or gift idea, research a place, draft a message, add something to the calendar). \
How you say it (only the spoken \"say\" counts): speak as a friend who remembers, never as a log. Good: \"you mentioned climbing\", \"there's supposed to be a good bouldering gym in the 2nd, want me to look?\", \"Eva's birthday is Friday\". Bad: \"you searched for\", \"you looked up\", any count of days or hours, anything about baskets or screens, claims about places you can't know (the action step is where Sam finds that out: the spoken line only offers to look). Health and body only as a friend who noticed, lightly, never as a coach; nothing they said they don't want. \
Truth: every item rests on the context. Read who is whose friend, whose birthday it is and who likes what before writing; check the weekday of every date against \"Now\"; never merge two people or two events; invent nothing. \
Each item: \"say\" = what Sam says out loud (short, warm, dry; one or two sentences; ends with an easy yes/no), \"when\" = a quiet moment, \"action\" = the background step if they say yes, written as a request to Sam's helper that names what it should match (their tastes), \"why\" = your private reasoning with the context lines it rests on (never spoken), \
\"intent\" = what a map or event listing should be searched for when the item is about a place or an event: {\"kind\": one of cafe, restaurant, concert, exhibition, market, outdoor, sport, shop, gift, other; \"search\": a few words such as 'sourdough bakery' or 'bouldering gym'; \"date\": YYYY-MM-DD for an event; \"evening\": true/false; \"social\": true when it's with other people} (omit for items that aren't about a place or event). \
Return ONLY JSON {\"items\": [{\"say\": \"...\", \"when\": \"...\", \"action\": \"...\", \"why\": \"...\", \"intent\": {...}}]}";

const GROUND: &str = "Rewrite one spoken line for a voice companion so that it names the real place or event given (name, and the one fact that makes it fit: hours, outdoor seats, cuisine, time, venue). Keep it short, warm and dry, one or two sentences, ending with an easy yes/no; say only what the facts say; never a count of days or hours about the person. Reply ONLY with JSON {\"say\": \"...\"}";

const CHECK: &str = "You check a friend's planned remarks against the facts. For each item, in order: \"grounded\" = nothing in the item contradicts or goes beyond the context (a wrong person, date, taste or event is NOT grounded); \"tactful\" = judged on the SPOKEN \"say\" text only: no quoted searches or browsing, no counts of days or hours, no baskets or screens, no nagging, no claims about places or products Sam can't know. If \"tactful\" is false, give a \"rewrite\": the same idea said as a friend who simply remembers. Return ONLY JSON {\"verdicts\": [{\"i\": 0, \"grounded\": true, \"tactful\": true, \"problem\": \"\", \"rewrite\": \"\"}]}";

/// Everything the generator may know, phrased as a friend would know it (no raw history).
async fn context(app: &AppState) -> String {
    let now = chrono::Local::now();
    let card = card_text(app).unwrap_or_else(|| "(nothing known yet)".into());
    let mut asked = Vec::new();
    if let Some(t) = app.tasks.get() {
        let cutoff = now.timestamp_millis() - 7 * 24 * 3_600_000;
        for r in t.log.list().into_iter().filter(|r| r.created > cutoff && r.kind != "friend" && r.kind != "automation" && r.kind != "consolidate" && r.session.starts_with('s')) {
            let day = chrono::DateTime::from_timestamp_millis(r.created).map(|d| d.with_timezone(&chrono::Local).format("%a").to_string()).unwrap_or_default();
            let req: String = r.request.split("\n\nContext").next().unwrap_or(&r.request).chars().take(120).collect();
            asked.push(format!("{day}: asked Sam: \"{req}\" ({})", if r.status == "done" { "done" } else { &r.status }));
        }
    }
    asked.truncate(12);
    // Open threads (memory.rs): pending things in their life, their day near.
    let today = now.date_naive();
    let threads: Vec<String> = app.memory.get().map(|m: &crate::memory::Memory| m.open_threads()).unwrap_or_default().into_iter()
        .filter(|t| t.status == "open" && t.due.is_none_or(|d| (d - today).num_days().abs() <= 5))
        .take(6)
        .map(|t| format!("- {} ({})", t.text, t.due.map_or("no day said".into(), |d| d.format("%a %-d %b").to_string())))
        .collect();
    // Tests and labs (SAM_NO_REAL_APPS) never read the user's real calendar.
    let cal = if std::env::var("SAM_NO_REAL_APPS").is_ok() { None } else { crate::apps::calendar_events(now, now + chrono::Duration::days(7), "").await.ok() }
        .and_then(|v| v.as_array().map(|a| a.iter().take(15).map(|e| format!("{} {}{}", e["start"].as_str().unwrap_or(""), e["title"].as_str().unwrap_or(""), e["location"].as_str().filter(|l| !l.is_empty()).map(|l| format!(" ({l})")).unwrap_or_default())).collect::<Vec<_>>()))
        .unwrap_or_default();
    format!("Profile:\n{card}\n\nComing up or just happened in their life (they told Sam; ask how it went, at most one):\n{}\n\nWhat they did with Sam this week:\n{}\n\nCalendar, next 7 days:\n{}\n\nNow: {} ({}), at their Mac.",
        if threads.is_empty() { "- nothing".into() } else { threads.join("\n") },
        if asked.is_empty() { "- nothing".into() } else { asked.iter().map(|a| format!("- {a}")).collect::<Vec<_>>().join("\n") },
        if cal.is_empty() { "- nothing".into() } else { cal.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n") },
        now.format("%A %-d %B %Y, %H:%M"), now.format("%Y-%m-%d"))
}

fn parse(s: &str) -> Value {
    let s = regex::Regex::new(r"(?s)<think>.*?</think>").unwrap().replace_all(s, "");
    let bytes: Vec<usize> = s.match_indices('{').map(|(i, _)| i).collect();
    for &i in &bytes {
        for j in s.match_indices('}').map(|(j, _)| j).rev() {
            if j <= i { break; }
            if let Ok(v) = serde_json::from_str::<Value>(&s[i..=j]) { return v; }
        }
    }
    Value::Null
}

/// Generate, check, keep at most two, store; returns the items kept.
pub async fn generate(app: &AppState, seed: Option<Value>) -> anyhow::Result<Vec<CheckIn>> {
    let t0 = std::time::Instant::now();
    let ctx = context(app).await;
    let t_ctx = t0.elapsed().as_millis();
    // `seed`: one item given from outside (tests, the lab) instead of the generator's own.
    let mut items: Vec<Value> = match seed {
        Some(it) => vec![it],
        None => {
            let (txt, _) = llm::complete_tier(app, llm::Tier::Standard, GEN, &ctx, true, 1500).await?;
            parse(&txt)["items"].as_array().cloned().unwrap_or_default()
        }
    };
    items.truncate(3);
    let t_gen = t0.elapsed().as_millis();
    // Items about a place or an event are grounded on a real candidate (suggest.rs): the map
    // and the listings supply the facts, the taste scorer picks, the model only words it.
    let tastes: Vec<String> = card_lines(app).into_iter().filter(|(l, _)| l.starts_with("TASTE:") || l.starts_with("PSYCHE:")).map(|(l, _)| l).collect();
    let recent: Vec<String> = checkins().into_iter().rev().take(40).filter_map(|c| c.candidate.map(|k| k.id)).collect();
    // Items are grounded concurrently, each capped (2026-10-15: one after another, "Check in
    // now" took ~2 minutes); an item whose lookup doesn't finish stays ungrounded.
    let tastes_s = tastes.join("\n");
    let lookups = items.iter().map(|it| {
        let intent = serde_json::from_value::<crate::suggest::Intent>(it["intent"].clone()).ok().filter(|i| !(i.kind.is_empty() || i.kind == "other" || i.kind == "gift"));
        let ask = it["action"].as_str().unwrap_or("").to_string();
        let (tastes_s, recent) = (&tastes_s, &recent);
        async move {
            let intent = intent?;
            let work = async {
                let cands = match crate::suggest::candidates(app, &intent).await {
                    Ok(c) => c,
                    Err(e) => { tracing::warn!("suggest candidates: {e:#}"); return None; }
                };
                crate::suggest::pick(app, cands, &intent, tastes_s, &ask, recent).await
            };
            match tokio::time::timeout(std::time::Duration::from_secs(25), work).await {
                Ok(r) => r,
                Err(_) => { tracing::warn!(kind = intent.kind, "check-in place lookup timed out"); None }
            }
        }
    });
    let picks = futures_util::future::join_all(lookups).await;
    let t_lookup = t0.elapsed().as_millis();
    let mut grounded: Vec<(usize, crate::suggest::Candidate, bool)> = Vec::new();
    for ((i, it), pick) in items.iter_mut().enumerate().zip(picks) {
        let Some((c, why, llm_top)) = pick else { continue };
        let ask = it["action"].as_str().unwrap_or("").to_string();
        let facts = format!("{} — {}{}{}{}", c.name, c.address, if c.hours.is_empty() { String::new() } else { format!(" — hours: {}", c.hours) }, if c.facts.is_empty() { String::new() } else { format!(" — {}", c.facts.join(", ")) }, if why.is_empty() { String::new() } else { format!(" — why it fits: {why}") });
        if let Ok((g, _)) = llm::complete_tier(app, llm::Tier::Light, GROUND, &format!("Current line: {}\nReal candidate: {facts}", it["say"].as_str().unwrap_or("")), true, 1500).await {
            if let Some(say) = parse(&g)["say"].as_str().filter(|s| s.len() > 10) {
                it["say"] = json!(say);
            }
        }
        it["action"] = json!(format!("{} Chosen: {} ({}{}). Confirm the details (hours, address) and tell them what's worth knowing before they go.", ask, c.name, c.address, if c.url.is_empty() { String::new() } else { format!(", {}", c.url) }));
        it["candidate"] = json!(c.name);
        grounded.push((i, c, llm_top));
    }
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    if !items.is_empty() {
        // The map's facts about the chosen places count as context too (the lines were rewritten on them).
        let map_facts: String = grounded.iter().map(|(_, c, _)| format!("- {}: {} | hours: {} | {} | {}", c.name, c.address, if c.hours.is_empty() { "?" } else { &c.hours }, c.facts.join(", "), c.source)).collect::<Vec<_>>().join("\n");
        let user = format!("Context (the only facts there are; the map facts are part of the context):\n{ctx}\n\nMap and listing facts about the places named in the items (treat as true):\n{}\n\nProposed items:\n{}", if map_facts.is_empty() { "- none" } else { &map_facts }, serde_json::to_string_pretty(&items).unwrap_or_default());
        let verdicts = match llm::complete_tier(app, llm::Tier::Light, CHECK, &user, true, 800).await {
            Ok((v, _)) => parse(&v)["verdicts"].as_array().cloned().unwrap_or_default(),
            Err(e) => { tracing::warn!("check-in fact check failed: {e:#}"); vec![] }
        };
        for (i, it) in items.iter().enumerate() {
            let v = verdicts.iter().find(|x| x["i"].as_u64() == Some(i as u64)).cloned().unwrap_or(json!({}));
            let mut say = it["say"].as_str().unwrap_or("").trim().to_string();
            if v["grounded"].as_bool() == Some(false) {
                dropped.push((say.clone(), v["problem"].as_str().unwrap_or("").to_string()));
                continue;
            }
            if v["tactful"].as_bool() == Some(false) {
                match v["rewrite"].as_str().map(str::trim).filter(|r| r.len() > 10) {
                    Some(r) => say = r.to_string(),
                    None => { dropped.push((say.clone(), v["problem"].as_str().unwrap_or("").to_string())); continue; }
                }
            }
            if say.is_empty() { continue; }
            let g = grounded.iter().find(|(gi, _, _)| *gi == i);
            kept.push(CheckIn { id: format!("c{}{}", chrono::Local::now().timestamp_millis(), i), ts: chrono::Local::now().timestamp_millis(), say,
                when: it["when"].as_str().unwrap_or("").into(), action: it["action"].as_str().unwrap_or("").into(), why: it["why"].as_str().unwrap_or("").into(), status: "proposed".into(),
                candidate: g.map(|(_, c, _)| c.clone()), features: g.map(|(_, c, _)| c.features.clone()).unwrap_or_default(), llm_top: g.is_some_and(|(_, _, t)| *t) });
            if kept.len() == 2 { break; }
        }
    }
    let mut all = checkins();
    for (say, problem) in &dropped {
        all.push(CheckIn { id: format!("d{}", chrono::Local::now().timestamp_millis()), ts: chrono::Local::now().timestamp_millis(), say: say.clone(), when: String::new(), action: String::new(), why: problem.clone(), status: "dropped".into(), candidate: None, features: vec![], llm_top: false });
    }
    all.extend(kept.iter().cloned());
    write_all(&all);
    tracing::info!(kept = kept.len(), dropped = dropped.len(), ctx_ms = t_ctx, gen_ms = t_gen - t_ctx, lookup_ms = t_lookup - t_gen, rest_ms = t0.elapsed().as_millis() - t_lookup, "friend check-in generated");
    Ok(kept)
}

/// The notice the session speaks (framed so the voice model knows it's Sam's own thought).
pub fn notice_text(c: &CheckIn) -> String {
    format!("CHECKIN {}: {}", c.id, c.say)
}

/// Speak the first kept item: a task record (so the island reopens for it when closed), a
/// notice (spoken when nobody is talking) and a card.
pub fn deliver(app: &AppState, c: &CheckIn) {
    let Some(t) = app.tasks.get() else { return };
    let id = t.log.start("friend", "friend", "Sam's check-in");
    t.log.finish(&id, "done", &c.say);
    t.push_card("friend", json!({"type": "card", "kind": "result", "id": format!("r{}", c.id), "blocks": [{"header": {"icon": "drop", "title": "A thought", "detail": "Sam"}}, {"text": c.say}], "open": {"task": id}}));
    t.notify(crate::tasks::Notice { session: "friend".into(), text: notice_text(c) });
}

/// Runs the check-in once (settings and quiet moment permitting); `force` skips the timing.
pub async fn run_once(app: &AppState, force: bool, seed: Option<Value>) -> anyhow::Result<Value> {
    let s = load();
    if !force && !due(app, &s).await {
        return Ok(json!({"ran": false}));
    }
    let t0 = std::time::Instant::now();
    let kept = generate(app, seed).await?;
    tracing::info!(forced = force, items = kept.len(), secs = t0.elapsed().as_secs(), say = kept.first().map(|c| c.say.as_str()).unwrap_or(""), "friend check-in ran");
    if let Some(c) = kept.first() {
        deliver(app, c);
    }
    Ok(json!({"ran": true, "items": kept}))
}

/// Background: looks every 10 minutes whether a check-in is due.
pub fn spawn(app: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut last_prefetch: Option<std::time::Instant> = None;
        loop {
            // The home area's places into the cache once a day (live Overpass is unreliable).
            if last_prefetch.is_none_or(|t| t.elapsed().as_secs() > 86_400) && std::env::var("SAM_NO_REAL_APPS").is_err() {
                crate::suggest::prefetch(&app).await;
                last_prefetch = Some(std::time::Instant::now());
            }
            if let Err(e) = run_once(&app, false, None).await {
                tracing::warn!("friend check-in: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}

/// The user said yes to a spoken check-in: its action becomes a normal background request,
/// with their tastes attached so the helper matches, not just finds.
pub fn accept(app: &AppState, session: &str, c: &CheckIn) {
    mark(&c.id, "accepted");
    let tastes: Vec<String> = card_lines(app).into_iter().filter(|(l, _)| l.starts_with("TASTE:")).map(|(l, _)| l).collect();
    let text = if tastes.is_empty() { c.action.clone() } else {
        format!("{}\n\nMatch it to their tastes, and say for each option why it fits them:\n{}", c.action, tastes.join("\n"))
    };
    if let Some(t) = app.tasks.get() {
        t.submit(crate::tasks::Job::Request { session: session.into(), kind: String::new(), text });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cadence_backs_off_after_ignored() {
        let mk = |st: &str| super::CheckIn { id: String::new(), ts: 0, say: String::new(), when: String::new(), action: String::new(), why: String::new(), status: st.into(), candidate: None, features: vec![], llm_top: false };
        assert_eq!(super::cadence_days(&[mk("accepted")]), 1);
        assert_eq!(super::cadence_days(&[mk("accepted"), mk("ignored"), mk("ignored")]), 2);
        assert_eq!(super::cadence_days(&[mk("ignored"), mk("ignored"), mk("ignored"), mk("ignored"), mk("dropped")]), 4);
        assert_eq!(super::cadence_days(&[mk("ignored"), mk("ignored"), mk("accepted")]), 1);
    }
    #[test]
    fn seed_lines_and_adult() {
        let mut s = super::Settings::default();
        s.name = "Elijah".into(); s.birthday = "1992-03-04".into();
        s.tastes.insert("food".into(), vec!["sourdough".into(), "".into()]);
        s.psyche.push("wants to be heard before advice".into());
        s.home = "Vienna".into();
        let l = s.seed_lines();
        assert!(l[0].starts_with("IDENTITY: name Elijah, 3"));
        assert!(l[1].starts_with("HOME: lives in Vienna"));
        assert!(l.iter().any(|x| x == "TASTE: food: sourdough"));
        assert!(l.iter().any(|x| x.starts_with("PSYCHE: wants to be heard")));
        assert!(s.adult());
        s.birthday = "2015-01-01".into();
        assert!(!s.adult());
    }
}

#[cfg(test)]
mod turn_hint_tests {
    use super::*;
    use crate::llm::Msg;
    fn h(v: &[(&'static str, &str)]) -> Vec<Msg> {
        v.iter().map(|(r, t)| Msg { role: r, text: t.to_string() }).collect()
    }

    #[test]
    fn question_budget_after_two_questions() {
        let mut r = 0;
        let hist = h(&[("user", "I went running"), ("model", "Nice, how far?"), ("user", "ten k in the rain, it was awful"), ("model", "Ugh. Did you at least get a coffee after?"), ("user", "I did and then I went home and slept")]);
        let n = turn_hints(&hist, &[], &mut r, &mut None);
        assert!(n.contains("no question this turn"), "{n}");
        let hist = h(&[("user", "hey"), ("model", "Hey. Long day?"), ("user", "kind of, my manager moved the deadline again")]);
        assert!(turn_hints(&hist, &[], &mut r, &mut None).is_empty());
    }

    #[test]
    fn topic_on_short_turns_rotates_and_skips_sign_offs() {
        let topics = vec!["IDENTITY: Elijah, 34".to_string(), "ATTRIBUTE: training for a half marathon".to_string(), "RELATIONSHIP: friend Markus".to_string(), "INSTRUCTION: keep it short".to_string()];
        let (mut r, mut last) = (0, None);
        let a = turn_hints(&h(&[("user", "hey"), ("model", "Hey. Sunday mode?"), ("user", "dunno")]), &topics, &mut r, &mut last);
        assert!(a.contains("training for a half marathon"), "{a}");
        // Two turns later: too soon for another; three turns later: the next one.
        let h3 = h(&[("user", "hey"), ("model", "Hey."), ("user", "dunno"), ("model", "Fair."), ("user", "mm")]);
        assert!(!turn_hints(&h3, &topics, &mut r, &mut last).contains("Something from their life"));
        let h4 = h(&[("user", "hey"), ("model", "Hey."), ("user", "dunno"), ("model", "Fair."), ("user", "mm"), ("model", "Mm."), ("user", "yeah"), ("model", "Yep."), ("user", "eh")]);
        assert!(turn_hints(&h4, &topics, &mut r, &mut last).contains("Markus"));
        assert!(turn_hints(&h(&[("user", "hey"), ("model", "Hey."), ("user", "good night")]), &topics, &mut r, &mut None).is_empty());
        // They said they don't want to talk: no topics for the rest of the session.
        let hd = h(&[("user", "don't really want to talk about it"), ("model", "Okay."), ("user", "mm")]);
        assert!(turn_hints(&hd, &topics, &mut r, &mut None).is_empty());
        // First turn of a session: nothing to react to yet, no topic.
        assert!(turn_hints(&h(&[("user", "hi")]), &topics, &mut r, &mut None).is_empty());
    }

    #[test]
    fn topic_items_split_and_filter() {
        let lines = vec![
            "ATTRIBUTE: rides a motorbike on weekends; drinks far too much espresso; sleeps badly in busy weeks".to_string(),
            "ATTRIBUTE: likes dark humour and being teased a bit; dislikes pep talks and being managed".to_string(),
            "RELATIONSHIP: girlfriend Anna (anna@example.com), friend Markus (coffee buddy, always late), mother in Graz".to_string(),
            "ATTRIBUTE: screen time this week averages 13.5 hours a day; the last run logged was 9 days ago".to_string(),
            "INSTRUCTION: keep it short".to_string(),
        ];
        assert_eq!(topic_items(&lines), vec!["rides a motorbike on weekends", "drinks far too much espresso", "sleeps badly in busy weeks", "girlfriend Anna", "friend Markus (coffee buddy, always late)", "mother in Graz"]);
    }

    #[test]
    fn greetings() {
        for g in ["Hey Sam, I'm back.", "hi", "Good evening!", "I'm back", "Guten Morgen"] { assert!(is_greeting(g), "{g}"); }
        for g in ["Yes, please do it.", "what's the weather", "they said hi to me"] { assert!(!is_greeting(g), "{g}"); }
    }

    #[test]
    fn thread_mentions() {
        assert!(mentions("Evening, Elijah. How'd the Bitpanda interview go yesterday?", "his job interview at Bitpanda"));
        assert!(mentions("How did the trial with Markus go?", "the half marathon trial run with Markus"));
        assert!(!mentions("Hey, Elijah. How's your evening?", "his mother's test results from the hospital in Graz"));
    }

    #[test]
    fn repeated_opener() {
        let mut r = 0;
        let hist = h(&[("user", "a b c d"), ("model", "Yeah, fair."), ("user", "e f g h"), ("model", "[amused] Yeah, that's it."), ("user", "so I told him to leave it")]);
        assert!(turn_hints(&hist, &[], &mut r, &mut None).contains("\"Yeah\""));
    }
}
