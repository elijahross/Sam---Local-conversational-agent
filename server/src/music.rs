//! Music: "play some jazz" → YouTube in the user's browser (no account: the search page's
//! own data gives the first video), or Spotify (the desktop app over AppleScript, searching
//! through the Web API when the user pasted an app's client id and secret). Playback is
//! immediate, never a draft: nothing leaves the Mac except the search.

use std::sync::Mutex;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::AppState;

/// What Sam started last (service, title, url) so "stop the music" pauses the right thing and
/// says where it was playing. In memory: the user restarts music with a word anyway.
#[derive(Clone, Debug)]
pub struct NowPlaying {
    /// youtube | spotify | music (Apple Music)
    pub service: String,
    pub title: String,
    pub url: String,
    pub paused: bool,
    /// The app it plays in: the browser for YouTube ("Firefox"), else the player itself.
    pub app: String,
}

/// The now-playing card's data (island card, overview, the app's play/pause button).
pub fn view(n: &NowPlaying) -> Value {
    json!({"type": "music", "service": n.service, "title": n.title, "url": n.url, "app": n.app, "state": if n.paused { "paused" } else { "playing" }})
}

fn music_app() -> bool {
    std::path::Path::new("/System/Applications/Music.app").exists()
}

/// AppleScript string literal.
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Apple Music: a playlist whose name matches, else the first library track by name, artist,
/// album or genre. Only the user's own library (no catalogue search without an Apple account API).
async fn music_play(query: &str, dry: bool) -> anyhow::Result<String> {
    let q = applescript_str(query.trim());
    let script = format!(
        "tell application \"Music\"\n\
         set pl to (every user playlist whose name contains {q})\n\
         if pl is not {{}} then\n  play item 1 of pl\n  return \"playlist \" & (name of item 1 of pl)\nend if\n\
         set r to (every track of library playlist 1 whose name contains {q} or artist contains {q} or album contains {q} or genre contains {q})\n\
         if r is {{}} then error \"nothing in your Music library matches that\"\n\
         play item 1 of r\n\
         return (name of item 1 of r) & \" – \" & (artist of item 1 of r)\n\
         end tell");
    if dry {
        return Ok(format!("{} (dry run)", query.trim()));
    }
    osascript(&script).await
}

static NOW: Mutex<Option<NowPlaying>> = Mutex::new(None);

pub fn now_playing() -> Option<NowPlaying> {
    NOW.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_now(n: Option<NowPlaying>) {
    *NOW.lock().unwrap_or_else(|e| e.into_inner()) = n;
}

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// The first video on YouTube's results page for the query: (id, title).
pub async fn youtube_search(app: &AppState, query: &str) -> anyhow::Result<(String, String)> {
    let url = format!("https://www.youtube.com/results?search_query={}", urlencoding(query));
    let html = app.http.get(&url).header("User-Agent", UA).header("Accept-Language", "en-US,en;q=0.8")
        // The EU consent interstitial is skipped with these cookies.
        .header("Cookie", "CONSENT=YES+cb.20240101-00-p0.en+FX+000; SOCS=CAISEwgDEgk2NzU0NzM4ODIaAmVuIAEaBgiAo5m8Bg")
        .timeout(std::time::Duration::from_secs(20)).send().await?.text().await?;
    // ytInitialData holds the result list; the first videoRenderer is what a person would click.
    let re = regex::Regex::new(r#""videoRenderer":\{"videoId":"([A-Za-z0-9_-]{11})""#).unwrap();
    let title_re = regex::Regex::new(r#""title":\{"runs":\[\{"text":"((?:[^"\\]|\\.)*)""#).unwrap();
    let mut best: Option<(String, String)> = None;
    for m in re.captures_iter(&html) {
        let id = m[1].to_string();
        let after = &html[m.get(0).unwrap().end()..];
        let title = title_re.captures(&after[..after.len().min(4000)]).map(|t| t[1].replace("\\u0026", "&").replace("\\\"", "\"")).unwrap_or_default();
        if !title.to_lowercase().contains("#shorts") {
            best = Some((id, title));
            break;
        }
    }
    best.context("no video found on YouTube for that")
}

/// A playlist for an open-ended wish ("some jazz", "lofi for working"): YouTube's playlist-
/// filtered results, (first video id, playlist id, playlist title). None when the page has no
/// playlist (then a video with its auto-mix keeps the music going).
pub async fn youtube_playlist(app: &AppState, query: &str) -> Option<(String, String, String)> {
    let url = format!("https://www.youtube.com/results?search_query={}&sp=EgIQAw%253D%253D", urlencoding(query));
    let html = app.http.get(&url).header("User-Agent", UA).header("Accept-Language", "en-US,en;q=0.8")
        .header("Cookie", "CONSENT=YES+cb.20240101-00-p0.en+FX+000; SOCS=CAISEwgDEgk2NzU0NzM4ODIaAmVuIAEaBgiAo5m8Bg")
        .timeout(std::time::Duration::from_secs(20)).send().await.ok()?.text().await.ok()?;
    let re = regex::Regex::new(r#""watchEndpoint":\{"videoId":"([A-Za-z0-9_-]{11})","playlistId":"([A-Za-z0-9_-]{13,})""#).unwrap();
    let m = re.captures(&html)?;
    let (vid, pl) = (m[1].to_string(), m[2].to_string());
    // The playlist's own title sits in its lockup, shortly before the watch endpoint.
    let title_re = regex::Regex::new(r#""title":\{"content":"((?:[^"\\]|\\.)*)""#).unwrap();
    let before = &html[m.get(0).unwrap().start().saturating_sub(6000)..m.get(0).unwrap().start()];
    let title = title_re.captures_iter(before).last().map(|t| t[1].replace("\\u0026", "&").replace("\\\"", "\"").replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"")).unwrap_or_else(|| format!("{query} playlist"));
    Some((vid, pl, title))
}

/// Specific wishes ("Blue in Green by Miles Davis", a named track) get that video; anything
/// open-ended (a genre, a mood, an artist, "a playlist") gets a playlist that keeps playing.
fn wants_playlist(query: &str) -> bool {
    let q = format!(" {} ", query.to_lowercase());
    !(q.contains(" by ") || q.contains(" von ") || q.contains(" song ") || q.contains(" track ") || q.contains('"'))
}

/// The media key (play/pause, next, previous) through the computer-use helper, which posts it
/// system-wide: the browser tab Sam opened answers it like the keyboard's own key.
async fn media_key(app: &AppState, key: &str) -> anyhow::Result<()> {
    let r: Value = app.http.post("http://127.0.0.1:47803/media").header("X-Sam-Token", crate::localauth::token()).json(&json!({"key": key}))
        .timeout(std::time::Duration::from_secs(5)).send().await.context("the computer-use helper isn't running")?.json().await?;
    if let Some(e) = r["error"].as_str() {
        bail!("{e}");
    }
    Ok(())
}

/// YouTube's player in the user's browser through the helper (its /youtube route presses the
/// player's own Play / Pause / Next buttons via Accessibility). Err = helper not reachable.
async fn helper_youtube(app: &AppState, url: &str, action: &str) -> anyhow::Result<Value> {
    let r: Value = app.http.post("http://127.0.0.1:47803/youtube").header("X-Sam-Token", crate::localauth::token()).json(&json!({"url": url, "action": action}))
        .timeout(std::time::Duration::from_secs(40)).send().await.context("the computer-use helper isn't running")?.json().await?;
    Ok(r)
}

fn urlencoding(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_string() } else { format!("%{:02X}", c as u32) }).collect::<Vec<_>>().join("")
}

fn spotify_app() -> bool {
    std::path::Path::new("/Applications/Spotify.app").exists()
}

async fn osascript(script: &str) -> anyhow::Result<String> {
    let o = tokio::process::Command::new("osascript").arg("-e").arg(script).output().await?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Spotify search through the Web API with the user's own app credentials (Keychain
/// sam.spotify: "client_id", "client_secret"), → (uri, "Artist – Title").
async fn spotify_search(app: &AppState, query: &str) -> anyhow::Result<(String, String)> {
    let (id, secret) = (crate::secrets::kc_get("sam.spotify", "client_id"), crate::secrets::kc_get("sam.spotify", "client_secret"));
    let (Some(id), Some(secret)) = (id, secret) else { bail!("Spotify isn't set up (Connectors › Spotify: client id and secret)") };
    let tok: Value = app.http.post("https://accounts.spotify.com/api/token").basic_auth(id, Some(secret)).form(&[("grant_type", "client_credentials")]).timeout(std::time::Duration::from_secs(15)).send().await?.json().await?;
    let token = tok["access_token"].as_str().context("spotify token")?;
    let kind = if query.to_lowercase().contains("playlist") { "playlist" } else if query.split_whitespace().count() <= 2 && !query.contains(' ') { "track,artist,playlist" } else { "track,playlist" };
    let url = url::Url::parse_with_params("https://api.spotify.com/v1/search", &[("q", query), ("type", kind), ("limit", "3")])?;
    let v: Value = app.http.get(url).bearer_auth(token).timeout(std::time::Duration::from_secs(15)).send().await?.json().await?;
    for k in ["tracks", "playlists", "artists"] {
        if let Some(it) = v[k]["items"].as_array().and_then(|a| a.iter().find(|x| x["uri"].is_string())) {
            let name = format!("{}{}", it["artists"][0]["name"].as_str().map(|a| format!("{a} – ")).unwrap_or_default(), it["name"].as_str().unwrap_or(""));
            return Ok((it["uri"].as_str().unwrap_or("").to_string(), name));
        }
    }
    bail!("nothing found on Spotify for that")
}

/// Returns what is playing (for the notice). Test mode (SAM_NO_REAL_APPS) only reports.
pub async fn play(app: &AppState, query: &str, service: &str, action: &str) -> anyhow::Result<String> {
    let dry = std::env::var("SAM_NO_REAL_APPS").is_ok();
    let action = action.to_lowercase();
    if matches!(action.as_str(), "pause" | "stop" | "resume" | "next" | "previous") {
        return control(app, &action, service, dry).await;
    }
    let wanted = service.to_lowercase();
    let service = if wanted.contains("spotify") || (wanted.is_empty() && spotify_app() && crate::secrets::kc_get("sam.spotify", "client_id").is_some()) {
        "spotify"
    } else if wanted.contains("music") || wanted.contains("apple") || wanted.contains("itunes") {
        "music"
    } else {
        "youtube"
    };
    if query.trim().is_empty() {
        bail!("what should I play?");
    }
    if service == "music" {
        if !music_app() {
            bail!("the Music app isn't on this Mac");
        }
        let name = music_play(query, dry).await?;
        set_now(Some(NowPlaying { service: "music".into(), title: name.clone(), url: String::new(), paused: false, app: "Music".into() }));
        return Ok(format!("playing in Apple Music: {name}"));
    }
    if service == "spotify" {
        if !spotify_app() {
            bail!("Spotify isn't installed on this Mac");
        }
        let (uri, name) = spotify_search(app, query).await?;
        if !dry {
            osascript(&format!("tell application \"Spotify\"\nactivate\nplay track \"{uri}\"\nend tell")).await?;
        }
        set_now(Some(NowPlaying { service: "spotify".into(), title: name.clone(), url: uri, paused: false, app: "Spotify".into() }));
        return Ok(format!("playing on Spotify: {name}"));
    }
    // A genre or mood starts a playlist that keeps going; without one, the first video with
    // YouTube's own mix after it (list=RD…), so the music doesn't end after one song.
    let (url, title) = match if wants_playlist(query) { youtube_playlist(app, query).await } else { None } {
        Some((vid, pl, title)) => (format!("https://www.youtube.com/watch?v={vid}&list={pl}"), format!("playlist \"{title}\"")),
        None => {
            let (id, title) = youtube_search(app, query).await?;
            (format!("https://www.youtube.com/watch?v={id}&list=RD{id}"), title)
        }
    };
    // The user's default browser takes the focus on purpose (they asked for music). Browsers
    // hold back autoplay with sound, so the helper presses the player's Play button; without the
    // helper the page is only opened (2026-10-14: the tab opened and nothing played).
    let mut started = dry;
    let mut note = String::new();
    let mut browser = String::new();
    if !dry {
        match helper_youtube(app, &url, "play").await {
            Ok(v) if v["playing"].as_bool() == Some(true) => {
                started = true;
                browser = v["browser"].as_str().unwrap_or("").to_string();
            }
            Ok(v) => {
                tracing::warn!(answer = %v, "youtube: the helper couldn't start the player");
                if v["opened"].as_bool() != Some(true) {
                    tokio::process::Command::new("open").arg(&url).status().await?;
                }
                note = v["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
            }
            Err(e) => {
                tracing::warn!(error = %e, "youtube: opening without the helper");
                tokio::process::Command::new("open").arg(&url).status().await?;
            }
        }
    }
    set_now(Some(NowPlaying { service: "youtube".into(), title: title.clone(), url: url.clone(), paused: !started, app: browser }));
    if started {
        Ok(format!("playing on YouTube: {title} ({url})"))
    } else {
        Ok(format!("opened on YouTube: {title} ({url}); press play if it didn't start{note}"))
    }
}

/// Pause / resume / next / previous for what Sam started (it remembers the source). YouTube is
/// found in the browser even when Sam didn't start it or was restarted since (the helper
/// switches to the YouTube tab); Spotify when it is open and playing.
async fn control(app: &AppState, action: &str, service: &str, dry: bool) -> anyhow::Result<String> {
    let now = now_playing().filter(|n| service.is_empty() || n.service.eq_ignore_ascii_case(service));
    let cmd = match action { "pause" | "stop" => "pause", "resume" => "play", "next" => "next track", _ => "previous track" };
    let verb = match action { "pause" | "stop" => "paused", "resume" => "resumed", "next" => "skipped to the next track on", _ => "went back a track on" };
    if let Some(n) = now.as_ref().filter(|n| n.service == "spotify" || n.service == "music") {
        let player = if n.service == "spotify" { "Spotify" } else { "Music" };
        if !dry { osascript(&format!("tell application \"{player}\" to {cmd}")).await?; }
        set_now(Some(NowPlaying { paused: matches!(action, "pause" | "stop"), ..n.clone() }));
        return Ok(format!("{verb} {}: {}", if player == "Music" { "Apple Music" } else { player }, n.title));
    }
    if dry {
        let Some(n) = now else { bail!("nothing is playing that I started") };
        set_now(Some(NowPlaying { paused: matches!(action, "pause" | "stop"), ..n.clone() }));
        return Ok(format!("{verb} YouTube, in your browser: {}", n.title));
    }
    // YouTube: the player's own button through the helper (it brings the YouTube tab to the
    // front first when another tab is showing); the system media key when the helper can't.
    if !service.eq_ignore_ascii_case("spotify") {
        match helper_youtube(app, "", action).await {
            Ok(v) if v["ok"].as_bool() == Some(true) => {
                let title = now.as_ref().map(|n| n.title.clone()).filter(|t| !t.is_empty()).or_else(|| v["title"].as_str().map(str::to_string).filter(|t| !t.is_empty())).unwrap_or_else(|| "the video".into());
                if let Some(n) = now.as_ref().filter(|n| n.service == "youtube") {
                    set_now(Some(NowPlaying { paused: matches!(action, "pause" | "stop"), ..n.clone() }));
                }
                let playing = v["playing"].as_bool().unwrap_or(false);
                let state = match (action, playing) { ("pause" | "stop", false) => "paused", ("resume", true) => "resumed", ("pause" | "stop", true) => "couldn't pause", ("resume", false) => "couldn't resume", _ => verb };
                return Ok(format!("{state} YouTube, in your browser ({}): {title}", v["browser"].as_str().unwrap_or("browser")));
            }
            Ok(v) => tracing::warn!(answer = %v, "youtube control: the helper found no player"),
            Err(e) => tracing::warn!(error = %e, "youtube control: no helper"),
        }
        if let Some(n) = now.as_ref().filter(|n| n.service == "youtube") {
            let key = match action { "pause" | "stop" | "resume" => "playpause", "next" => "next", _ => "previous" };
            media_key(app, key).await?;
            set_now(Some(NowPlaying { paused: matches!(action, "pause" | "stop"), ..n.clone() }));
            return Ok(format!("{verb} YouTube, in your browser (media key): {}", n.title));
        }
    }
    // Music Sam didn't start: whichever player is playing right now.
    for (player, present) in [("Spotify", spotify_app()), ("Music", music_app())] {
        if !present || !(service.is_empty() || service.to_lowercase().contains(&player.to_lowercase())) {
            continue;
        }
        let state = osascript(&format!("tell application \"{player}\" to if it is running then player state as string")).await.ok();
        if state.as_deref() == Some("playing") || (action == "resume" && state.as_deref() == Some("paused")) {
            osascript(&format!("tell application \"{player}\" to {cmd}")).await?;
            let shown = if player == "Music" { "Apple Music" } else { player };
            return Ok(format!("{verb} {shown} (it was playing on its own)"));
        }
    }
    bail!("nothing is playing that I started, and no YouTube tab is open in the browser; if music is on somewhere else, pause it there")
}

/// Connector setup: {"client_id", "client_secret"} into the Keychain; a quick token check.
pub async fn spotify_setup(app: &AppState, b: &Value) -> anyhow::Result<Value> {
    let (id, secret) = (b["client_id"].as_str().unwrap_or("").trim(), b["client_secret"].as_str().unwrap_or("").trim());
    if id.is_empty() || secret.is_empty() {
        bail!("client id and secret needed (developer.spotify.com › Dashboard › Create app)");
    }
    let tok: Value = app.http.post("https://accounts.spotify.com/api/token").basic_auth(id, Some(secret)).form(&[("grant_type", "client_credentials")]).timeout(std::time::Duration::from_secs(15)).send().await?.json().await?;
    if tok["access_token"].as_str().is_none() {
        bail!("Spotify rejected the credentials: {}", tok["error_description"].as_str().unwrap_or("?"));
    }
    crate::secrets::kc_set("sam.spotify", "client_id", id)?;
    crate::secrets::kc_set("sam.spotify", "client_secret", secret)?;
    Ok(json!({"ok": true, "app_installed": spotify_app()}))
}
