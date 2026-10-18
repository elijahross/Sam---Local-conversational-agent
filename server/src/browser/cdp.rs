//! Minimal Chrome DevTools Protocol client over tokio-tungstenite (no heavy browser crate).
//!
//! Runtime choice (browser lab, exp-078..083): host Chrome with an isolated, persistent profile
//! per "screen" (Bops pattern). Linux Chromium in Docker / Apple `container` micro-VMs was ~5x
//! slower per page and hit bot walls (Reddit, Cloudflare, Booking) that host Chrome passes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

pub const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
/// Headless Chrome advertises "HeadlessChrome"; use the real desktop UA (Bops local.ts).
pub const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// How a screen's window is shown. Off-screen headful is the default: it passes bot checks that
/// block headless, without a visible window. Visible is for logging in once by hand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Visibility {
    Headless,
    Offscreen,
    Visible,
}

/// One browser "screen": a Chrome process with its own persistent profile and DevTools port.
pub struct Chrome {
    pub port: u16,
    pub profile: PathBuf,
    child: Option<tokio::process::Child>,
    http: reqwest::Client,
}

/// One Chrome per profile (port) is shared by every handle (parallel tasks, parked tabs,
/// connector links). The process closes when the LAST handle goes, whoever launched it, so a
/// finishing task never kills a browser another task is still using.
struct Shared {
    refs: usize,
    pid: Option<u32>,
}
/// Seconds a browser stays open after its last handle. 0 in the CLIs (their process, and with
/// it the timer, ends right after the run); the server sets 180 at start (`set_linger`).
pub static LINGER_DEFAULT_S: AtomicU64 = AtomicU64::new(0);

pub fn set_linger(secs: u64) {
    LINGER_DEFAULT_S.store(secs, Ordering::Relaxed);
}

static SHARED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u16, Shared>>> = std::sync::LazyLock::new(Default::default);

fn share(port: u16, pid: Option<u32>) {
    let mut m = SHARED.lock().unwrap();
    let e = m.entry(port).or_insert(Shared { refs: 0, pid: None });
    e.refs += 1;
    if pid.is_some() {
        e.pid = pid;
    }
}

/// Handles to Sam's Chrome processes right now: (port, handles) — for the resource monitor.
pub fn shared_counts() -> Vec<(u16, usize)> {
    SHARED.lock().unwrap().iter().map(|(p, s)| (*p, s.refs)).collect()
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let mut m = SHARED.lock().unwrap();
        let last = match m.get_mut(&self.port) {
            Some(s) => {
                s.refs = s.refs.saturating_sub(1);
                s.refs == 0
            }
            None => true,
        };
        if last {
            // The browser lingers a few minutes after its last user: the next task attaches
            // instead of launching a new process (a launch costs ~2 s, a focus switch for the
            // user and a cold renderer). An idle Chrome costs ~300 MB meanwhile; CHROME_LINGER_S=0
            // closes it at once as before.
            let linger = crate::config::env_num("CHROME_LINGER_S", LINGER_DEFAULT_S.load(Ordering::Relaxed));
            if let Some(s) = m.get_mut(&self.port) {
                s.pid = s.pid.or_else(|| self.child.as_ref().and_then(|c| c.id()));
            }
            let port = self.port;
            let fallback_pid = self.child.as_ref().and_then(|c| c.id());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(linger));
                let mut m = SHARED.lock().unwrap();
                // Someone attached meanwhile: it's theirs now.
                if m.get(&port).is_some_and(|s| s.refs > 0) {
                    return;
                }
                let pid = m.remove(&port).and_then(|s| s.pid).or(fallback_pid);
                drop(m);
                kill_port(port, pid);
            });
        }
    }
}

/// SIGTERM the Chrome on `port` (it flushes cookies and exits with its helpers); by pattern
/// when nobody here launched it (e.g. after a server restart).
fn kill_port(port: u16, pid: Option<u32>) {
    match pid {
        Some(pid) => {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
        None => {
            let _ = std::process::Command::new("pkill").arg("-f").arg(format!("remote-debugging-port={port}")).status();
        }
    }
}

/// Server shutdown: close every Sam Chrome this process started or used.
pub fn kill_all() {
    let pids: Vec<u32> = SHARED.lock().unwrap().drain().filter_map(|(_, s)| s.pid).collect();
    for pid in pids {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
}

/// Kills Sam Chrome processes left over from a previous server run (crash, kill -9): they
/// belong to nobody now and would hold RAM forever.
pub fn reap_orphans(data_dir: &Path) {
    let dir = data_dir.join("browser/profiles");
    let _ = std::process::Command::new("pkill").arg("-f").arg(format!("--user-data-dir={}", dir.display())).status();
}

impl Chrome {
    /// Launch (or attach to an already running) Chrome for `screen` on port 9300 + screen.
    /// The profile persists logins/cookies: log in once with `headless = false`, reuse headless.
    pub async fn launch(data_dir: &Path, screen: u16, vis: Visibility) -> anyhow::Result<Self> {
        let headless = vis == Visibility::Headless;
        let port = 9300 + screen;
        let profile = data_dir.join("browser/profiles").join(format!("s{screen}"));
        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
        let mode_file = profile.join(".sam-mode");
        let want = match vis { Visibility::Headless => "headless", Visibility::Offscreen => "offscreen", Visibility::Visible => "visible" };
        if let Ok(c) = Self::attach_with(http.clone(), port, profile.clone()).await {
            if std::fs::read_to_string(&mode_file).is_ok_and(|m| m == want) {
                return Ok(c);
            }
            // Running in the other mode: close it so the requested mode takes effect.
            std::mem::forget(c);   // not a real user of it: don't count the release
            if let Some(s) = SHARED.lock().unwrap().get_mut(&port) {
                s.refs = s.refs.saturating_sub(1);
            }
            let _ = http.put(format!("http://127.0.0.1:{port}/json/close")).send().await;
            let _ = std::process::Command::new("pkill").arg("-f").arg(format!("remote-debugging-port={port}")).status();
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        std::fs::create_dir_all(&profile)?;
        // Bops bops-chrome: clear stale locks and the crash flag so no restore bubble appears.
        for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
            let _ = std::fs::remove_file(profile.join(f));
        }
        persist_sessions(&profile);
        let mut args = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-blink-features=AutomationControlled".into(),
            "--hide-crash-restore-bubble".into(),
            "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
            "--lang=en-US".into(),
            "--window-size=1280,960".into(),
            // RAM/speed: no images, one renderer per few tabs, no extensions/audio.
            "--blink-settings=imagesEnabled=false".into(),
            "--renderer-process-limit=4".into(),
            "--disable-extensions".into(),
            "--mute-audio".into(),
            "--disable-background-networking".into(),
        ];
        if headless {
            // Headless advertises "HeadlessChrome": present the real desktop UA instead.
            args.push("--headless=new".into());
            args.push(format!("--user-agent={UA}"));
        } else if vis == Visibility::Offscreen {
            // Headful but off-screen: passes Cloudflare/DataDome checks that block headless
            // (lab exp br-006 follow-up: 3 s vs blocked), without a visible window.
            // On Sam's invisible workspace display (no flash on the user's screens); macOS
            // would clamp a plain off-screen position into view at launch.
            let (x, y) = tokio::task::spawn_blocking(super::workspace::park_origin).await.unwrap_or((-2400, 0));
            args.push(format!("--window-position={x},{y}"));
            args.push("--window-size=1280,960".into());
            // No window at launch: Chrome's first window (and every `/json/new` tab) takes the
            // focus from the user, even through `open -g`; a target created over DevTools with
            // `background: true` doesn't (measured 2026-10-12, see new_tab).
            args.push("--no-startup-window".into());
            for f in ["--disable-backgrounding-occluded-windows", "--disable-renderer-backgrounding", "--disable-background-timer-throttling"] {
                args.push(f.into());
            }
        } else {
            args.push("--window-position=80,60".into());
            // Images on for a human (the agent's own tabs block heavy resources per tab).
            args.retain(|a| a != "--blink-settings=imagesEnabled=false");
        }
        std::fs::create_dir_all(&profile)?;
        let _ = std::fs::write(&mode_file, want);
        // Off-screen and headless: launched through LaunchServices in the background (`open -g`)
        // and without a startup window, so the new Chrome never takes the focus from what the
        // user is typing in; a direct spawn of the binary (or a first window) activates the app. The visible window (a login by hand) does
        // activate. `open` exits at once: the Chrome pid is looked up by its DevTools port.
        let (child, pid) = if headless || vis == Visibility::Offscreen {
            let app = std::path::Path::new(CHROME).ancestors().find(|a| a.extension().is_some_and(|e| e == "app")).map(|a| a.to_path_buf()).unwrap_or_else(|| PathBuf::from("/Applications/Google Chrome.app"));
            tokio::process::Command::new("open").arg("-g").arg("-n").arg("-a").arg(&app).arg("--args").args(&args)
                .stdout(Stdio::null()).stderr(Stdio::null()).status().await.context("open Chrome")?;
            (None, None)
        } else {
            let child = tokio::process::Command::new(CHROME)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(false)   // closed by the last handle (Drop), not by whoever launched it
                .spawn()
                .context("launch Chrome")?;
            let pid = child.id();
            (Some(child), pid)
        };
        share(port, pid);
        for _ in 0..200 {
            if http.get(format!("http://127.0.0.1:{port}/json/version")).send().await.is_ok() {
                if pid.is_none() {
                    // The pid of the Chrome `open` started, for the close later.
                    if let Ok(o) = tokio::process::Command::new("pgrep").arg("-f").arg(format!("remote-debugging-port={port}")).output().await {
                        if let Some(p) = String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse::<u32>().ok()).min() {
                            if let Some(s) = SHARED.lock().unwrap().get_mut(&port) {
                                s.pid = Some(p);
                            }
                        }
                    }
                }
                let c = Self { port, profile, child, http };
                c.close_restored_tabs().await;
                c.restore_cookies(screen).await;
                return Ok(c);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("Chrome did not expose DevTools on {port}")
    }

    /// "Continue where you left off" restores the last tabs; the agent starts clean.
    async fn close_restored_tabs(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(r) = self.http.get(format!("http://127.0.0.1:{}/json/list", self.port)).send().await {
            if let Ok(list) = r.json::<Vec<Value>>().await {
                for t in list.iter().filter(|t| t["type"] == "page") {
                    if let Some(id) = t["id"].as_str() {
                        let _ = self.http.get(format!("http://127.0.0.1:{}/json/close/{id}", self.port)).send().await;
                    }
                }
            }
        }
    }

    async fn attach_with(http: reqwest::Client, port: u16, profile: PathBuf) -> anyhow::Result<Self> {
        http.get(format!("http://127.0.0.1:{port}/json/version")).send().await?.error_for_status()?;
        share(port, None);
        Ok(Self { port, profile, child: None, http })
    }

    /// One command on the browser-level DevTools socket (targets, windows).
    async fn browser_send(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let v: Value = self.http.get(format!("http://127.0.0.1:{}/json/version", self.port)).send().await?.json().await?;
        let url = v["webSocketDebuggerUrl"].as_str().context("browser ws url")?.to_string();
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.context("connect browser")?;
        let (mut sink, mut stream) = ws.split();
        sink.send(Message::Text(json!({"id": 1, "method": method, "params": params}).to_string().into())).await?;
        let reply = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(Ok(m)) = stream.next().await {
                if let Message::Text(t) = m {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    if v["id"] == 1 {
                        return Some(v);
                    }
                }
            }
            None
        }).await.ok().flatten().context("browser command: no reply")?;
        if let Some(e) = reply["error"]["message"].as_str() {
            bail!("{method}: {e}");
        }
        Ok(reply["result"].clone())
    }

    pub async fn new_tab(&self) -> anyhow::Result<Tab> {
        // Each tab in its own window, created in the background: its page stays "visible"
        // (no throttling) in a window parked off-screen, and Chrome never comes to the front.
        // (`/json/new` and a foreground target both activate the app: the user's typing broke.)
        let t = self.browser_send("Target.createTarget", json!({"url": "about:blank", "background": true, "newWindow": true})).await?;
        let id = t["targetId"].as_str().context("tab id")?.to_string();
        let ws = format!("ws://127.0.0.1:{}/devtools/page/{id}", self.port);
        let tab = Tab::connect(&ws, id, self.port, self.http.clone()).await?;
        // macOS clamps --window-position, so park the off-screen window over CDP instead
        // (a minimized window throttles rendering; a parked normal window does not).
        if std::fs::read_to_string(self.profile.join(".sam-mode")).is_ok_and(|m| m == "offscreen") {
            tab.show_window(false).await;
        }
        Ok(tab)
    }

    /// Save every cookie (incl. session cookies) for this screen: logins survive crashes and
    /// restarts even when Chrome's own session restore doesn't (Playwright storageState
    /// equivalent). AES-256-GCM file next to the profile; only its key is in the Keychain.
    pub async fn save_cookies(&self, _screen: u16) -> anyhow::Result<usize> {
        let tab = self.new_tab().await?;
        let r = tab.send("Storage.getCookies", json!({})).await;
        tab.close().await;
        let cookies = r?["cookies"].clone();
        let n = cookies.as_array().map(|a| a.len()).unwrap_or(0);
        let jar = self.profile.with_extension("cookies.enc");
        std::fs::write(&jar, crate::secrets::seal(cookies.to_string().as_bytes())?)?;
        Ok(n)
    }

    async fn restore_cookies(&self, _screen: u16) {
        let jar = self.profile.with_extension("cookies.enc");
        let Some(raw) = std::fs::read(&jar).ok().and_then(|b| crate::secrets::open(&b).ok()) else { return };
        let Ok(mut cookies) = serde_json::from_slice::<Vec<Value>>(&raw) else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        // Drop expired; keep session cookies (expires = -1). setCookies rejects some read-only fields.
        cookies.retain(|c| c["expires"].as_f64().is_none_or(|e| e < 0.0 || e > now));
        for c in cookies.iter_mut() {
            if let Some(o) = c.as_object_mut() {
                for k in ["size", "session", "priority", "sourceScheme", "sourcePort", "partitionKey"] {
                    o.remove(k);
                }
                if o.get("expires").and_then(|e| e.as_f64()).is_some_and(|e| e < 0.0) {
                    o.remove("expires");
                }
            }
        }
        if let Ok(tab) = self.new_tab().await {
            let _ = tab.send("Storage.setCookies", json!({"cookies": cookies})).await;
            tab.close().await;
        }
    }

    /// Releases this handle; the browser closes when no other handle uses it.
    pub async fn shutdown(self) {
        drop(self);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Keep logins across restarts: Chrome drops session cookies on exit unless the profile is set
/// to "continue where you left off" (restore_on_startup = 1). Also clears the crash flag so no
/// restore bubble appears (Bops bops-chrome).
fn persist_sessions(profile: &Path) {
    let prefs = profile.join("Default/Preferences");
    let _ = std::fs::create_dir_all(profile.join("Default"));
    let mut v: Value = std::fs::read_to_string(&prefs).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    v["session"]["restore_on_startup"] = json!(1);
    v["profile"]["exit_type"] = json!("Normal");
    v["profile"]["exited_cleanly"] = json!(true);
    let _ = std::fs::write(&prefs, v.to_string());
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// One page target. Commands are request/response; events go to an mpsc the caller drains.
pub struct Tab {
    pub id: String,
    port: u16,
    next: AtomicU64,
    tx: mpsc::UnboundedSender<Message>,
    pending: Pending,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<Value>>,
    http: reqwest::Client,
}

impl Tab {
    async fn connect(ws_url: &str, id: String, port: u16, http: reqwest::Client) -> anyhow::Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url).await.context("connect tab")?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Default::default();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let p = pending.clone();
        tokio::spawn(async move {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                        if let Some(i) = v["id"].as_u64() {
                            if let Some(s) = p.lock().unwrap().remove(&i) {
                                let _ = s.send(v);
                            }
                        } else if v.get("method").is_some() {
                            let _ = ev_tx.send(v);
                        }
                    }
                    Some(Ok(_)) => {}
                    other => {
                        // E.g. a frame with invalid UTF-8 (a lone surrogate from page text):
                        // the socket is unusable. Fail every waiting request now instead of
                        // letting each one run into its 30 s timeout.
                        tracing::warn!(?other, "CDP connection closed");
                        p.lock().unwrap().clear();
                        break;
                    }
                }
            }
        });
        let tab = Self { id, port, next: AtomicU64::new(1), tx, pending, events: tokio::sync::Mutex::new(ev_rx), http };
        tab.send("Page.enable", json!({})).await?;
        Ok(tab)
    }

    pub async fn send(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.tx.send(Message::Text(json!({"id": id, "method": method, "params": params}).to_string().into()))?;
        let r = tokio::time::timeout(Duration::from_secs(30), rx).await.context(format!("{method} timed out"))??;
        if let Some(e) = r.get("error") {
            return Err(anyhow!("{method}: {e}"));
        }
        Ok(r["result"].clone())
    }

    /// Evaluate a JS expression (promises awaited) and return its JSON value. Runs in an
    /// isolated world: page scripts can't see or tamper with it, and Runtime.enable (the main
    /// CDP automation tell) is never called. DOM access is identical.
    pub async fn eval(&self, expr: &str) -> anyhow::Result<Value> {
        tracing::debug!(expr = %expr.chars().take(60).collect::<String>().replace('\n', " "), "eval");
        let ctx = self.isolated_world().await;
        // Page text can hold lone UTF-16 surrogates (an emoji cut in half by .slice()). Chrome
        // then sends invalid UTF-8 and the websocket dies, so every string is made well-formed
        // before it leaves the page.
        let wrapped = format!(
            "(async () => {{ const __v = await ({expr}\n); return __v === undefined ? null : JSON.parse(JSON.stringify(__v, (k, x) => typeof x === 'string' && x.toWellFormed ? x.toWellFormed() : x)); }})()"
        );
        let mut params = json!({"expression": wrapped, "returnByValue": true, "awaitPromise": true});
        if let Some(c) = ctx {
            params["contextId"] = json!(c);
        }
        let r = self.send("Runtime.evaluate", params).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("js: {}", ex["exception"]["description"].as_str().unwrap_or("error"));
        }
        Ok(r["result"]["value"].clone())
    }

    async fn isolated_world(&self) -> Option<i64> {
        let tree = self.send("Page.getFrameTree", json!({})).await.ok()?;
        let frame = tree["frameTree"]["frame"]["id"].as_str()?.to_string();
        let w = self.send("Page.createIsolatedWorld", json!({"frameId": frame, "worldName": "sam", "grantUniveralAccess": true})).await.ok()?;
        w["executionContextId"].as_i64()
    }

    /// Navigate and wait for the load event (or `timeout`), then a short settle.
    pub async fn goto(&self, url: &str, timeout: Duration) -> anyhow::Result<()> {
        if !allowed_url(url) {
            bail!("refusing to open {url}: only http(s) pages");
        }
        let mut ev = self.events.lock().await;
        while ev.try_recv().is_ok() {}
        let r = self.send("Page.navigate", json!({"url": url})).await?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            bail!("navigate: {e}");
        }
        let _ = tokio::time::timeout(timeout, async {
            while let Some(e) = ev.recv().await {
                if e["method"] == "Page.loadEventFired" {
                    break;
                }
            }
        })
        .await;
        drop(ev);
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(())
    }

    /// Network responses whose URL contains `part`, collected for `wait` after `action`
    /// ("navigate:<url>", "scroll", or "" to just listen): [(HTTP status, body)]. For sites that
    /// load their results as JSON (X GraphQL, TikTok search), reading that JSON beats parsing
    /// the DOM: no class names to break.
    pub async fn responses(&self, action: &str, part: &str, wait: Duration) -> anyhow::Result<Vec<(u16, String)>> {
        self.send("Network.enable", json!({})).await?;
        let mut ev = self.events.lock().await;
        while ev.try_recv().is_ok() {}
        if let Some(url) = action.strip_prefix("navigate:") {
            if !allowed_url(url) {
                bail!("refusing to open {url}: only http(s) pages");
            }
            self.send("Page.navigate", json!({"url": url})).await?;
        } else if action == "scroll" {
            self.eval("window.scrollBy(0, document.body.scrollHeight)").await?;
        } else if let Some(sel) = action.strip_prefix("click:") {
            self.eval(&format!("(()=>{{const e=document.querySelector({}); if(e) e.click(); return !!e}})()", serde_json::to_string(sel)?)).await?;
        }
        let mut hits: Vec<(String, u16)> = vec![];
        let mut done = std::collections::HashSet::new();
        let end = tokio::time::Instant::now() + wait;
        while let Ok(Some(e)) = tokio::time::timeout_at(end, ev.recv()).await {
            match e["method"].as_str() {
                Some("Network.responseReceived") if e["params"]["response"]["url"].as_str().is_some_and(|u| u.contains(part)) => {
                    hits.push((e["params"]["requestId"].as_str().unwrap_or("").to_string(), e["params"]["response"]["status"].as_u64().unwrap_or(0) as u16));
                }
                Some("Network.loadingFinished") => {
                    done.insert(e["params"]["requestId"].as_str().unwrap_or("").to_string());
                }
                _ => {}
            }
        }
        drop(ev);
        let mut out = vec![];
        for (id, status) in hits {
            let body = if done.contains(&id) || status != 200 {
                self.send("Network.getResponseBody", json!({"requestId": id})).await.ok().and_then(|b| b["body"].as_str().map(str::to_string)).unwrap_or_default()
            } else {
                String::new()
            };
            out.push((status, body));
        }
        Ok(out)
    }

    /// Block what text extraction never needs: media, fonts, images, ad/tracker scripts.
    pub async fn block_heavy(&self) -> anyhow::Result<()> {
        self.send("Network.enable", json!({})).await?;
        let urls = [
            "*.png", "*.jpg", "*.jpeg", "*.gif", "*.webp", "*.avif", "*.svg", "*.ico", "*.woff", "*.woff2", "*.ttf", "*.otf", "*.mp4", "*.webm", "*.mp3", "*.m3u8",
            "*doubleclick.net*", "*googlesyndication.com*", "*google-analytics.com*", "*googletagmanager.com*", "*facebook.net*", "*hotjar.com*", "*criteo*", "*taboola*", "*outbrain*", "*amazon-adsystem*",
        ];
        self.send("Network.setBlockedURLs", json!({"urls": urls})).await.map(|_| ())
    }

    /// Whether this profile holds cookie `name` for `url` (e.g. a site's login-session cookie).
    pub async fn has_cookie(&self, url: &str, name: &str) -> bool {
        match self.send("Network.getCookies", json!({"urls": [url]})).await {
            Ok(v) => v["cookies"].as_array().is_some_and(|a| a.iter().any(|c| c["name"] == name)),
            Err(_) => false,
        }
    }

    pub async fn set_cookies(&self, cookies: Value) -> anyhow::Result<()> {
        self.send("Network.setCookies", json!({"cookies": cookies})).await.map(|_| ())
    }

    pub async fn close(self) {
        let _ = self.http.get(format!("http://127.0.0.1:{}/json/close/{}", self.port, self.id)).send().await;
    }

    /// Bring this tab's window on screen (login hand-off) or park it at the left screen edge.
    /// Best effort: failures are ignored.
    pub async fn show_window(&self, on: bool) {
        let Ok(w) = self.send("Browser.getWindowForTarget", json!({"targetId": self.id})).await else { return };
        let Some(wid) = w["windowId"].as_i64() else { return };
        let _ = self.send("Browser.setWindowBounds", json!({"windowId": wid, "bounds": {"windowState": "normal"}})).await;
        let (left, top) = if on { (100, 80) } else { super::workspace::park_origin() };
        let _ = self.send("Browser.setWindowBounds", json!({"windowId": wid, "bounds": {"left": left, "top": top, "width": 1280, "height": 960}})).await;
        if on {
            let _ = self.send("Page.bringToFront", json!({})).await;
        }
    }
}

/// Pages Sam's browser may open: http(s) and about:blank. `file:` only for the lab's test
/// pages (app/Tests/E2E/fixtures and /tmp/sam-e2e), never the user's files (a page or model asking for
/// file:///…/.ssh/id_rsa gets nothing).
pub fn allowed_url(url: &str) -> bool {
    let u = url.trim().to_lowercase();
    if u.starts_with("http://") || u.starts_with("https://") || u == "about:blank" {
        return true;
    }
    if let Some(p) = url.trim().strip_prefix("file://") {
        let p = std::path::Path::new(p.split(['?', '#']).next().unwrap_or(p));
        let lab = crate::config::repo_root().and_then(|r| r.join("app/Tests/E2E/fixtures").canonicalize().ok());
        let real = p.canonicalize().ok();
        return real.as_ref().is_some_and(|r| lab.as_ref().is_some_and(|l| r.starts_with(l)) || r.starts_with("/private/tmp/sam-e2e") || r.starts_with("/tmp/sam-e2e"));
    }
    false
}
