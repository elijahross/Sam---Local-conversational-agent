use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, ws::WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;
use voice_agent::{AppState, env_or, llm, session};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    voice_agent::config::load_dev_env();
    tracing_subscriber::fmt()
        // html5ever warns once per odd node on every scraped page: thousands of lines of noise.
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()).add_directive("html5ever=error".parse().unwrap()))
        .init();
    // Chrome processes left over from a previous run (crash) hold RAM forever: close them.
    voice_agent::browser::cdp::reap_orphans(&voice_agent::thinker::data_dir());
    // A task's browser stays open a few minutes for the next task (no relaunch, no focus switch).
    voice_agent::browser::cdp::set_linger(180);
    // The server runs the background services (automations, monitors, campaigns, indexing).
    voice_agent::tasks::enable_background();
    let state = AppState::from_env().await?;
    llm::warm(&state).await;
    // The first Laya call loads/compiles the model; warm it so turn one isn't routed blind.
    if let Some(j) = &state.judge {
        let ok = j.ask(serde_json::json!("hello"), voice_agent::judge::questions(&["route", "kind", "done"]), 15_000).await.is_some();
        tracing::info!(ok, "laya warm");
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        // Task registry for the menu-bar status, notifications and the task overview (localhost only).
        .route("/api/tasks", get(tasks_list).post(task_submit))
        .route("/api/tasks/clear", post(tasks_clear))
        .route("/api/tasks/{id}", get(task_get).delete(task_delete))
        .route("/api/tasks/{id}/confirm", post(task_confirm))
        // Long-term history: hybrid search + filters (history.rs).
        .route("/api/history", get(history_search))
        // Saved-search monitors (monitors.rs).
        .route("/api/settings/llm", get(llm_settings_get).post(llm_settings_set))
        .route("/api/settings/voice", get(voice_get).post(voice_set))
        .route("/api/music", get(music_get).post(music_set))
        .route("/api/friend", get(friend_get).post(friend_set))
        .route("/api/friend/forget", post(friend_forget))
        .route("/api/friend/checkin", post(friend_checkin))
        .route("/api/music/spotify", post(spotify_setup))
        .route("/api/settings/storage", get(storage_get).post(storage_set))
        .route("/api/settings/storage/cleanup", post(storage_cleanup))
        .route("/api/automations", get(automations_list))
        .route("/api/automations/{id}", post(automation_action).delete(automation_delete))
        .route("/api/campaigns", get(campaigns_list))
        .route("/api/campaigns/{id}", post(campaign_action))
        .route("/api/monitors", get(monitors_list))
        .route("/api/monitors/{id}", get(monitor_get).post(monitor_action).delete(monitor_delete))
        .route("/api/file", get(file_get))
        .route("/api/connectors", get(connectors_list))
        .route("/api/connectors/{id}", post(connector_action))
        .route("/api/permissions/{app}", post(permission_probe))
        // /ws and /api: localhost Host only, no foreign browser Origin, native clients need the
        // local token (localauth.rs). The static page itself stays public on localhost.
        .layer(axum::middleware::from_fn(guard_local))
        .fallback_service(ServeDir::new(voice_agent::config::resources_dir().join("static")))
        .with_state(state);
    let bind = env_or("BIND", "127.0.0.1:47800");
    tracing::info!("listening on http://{bind}");
    // Ctrl-C / SIGTERM: close Sam's browsers too, so nothing keeps running in the background.
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        voice_agent::browser::park::discard_all();
        voice_agent::browser::cdp::kill_all();
        voice_agent::sandbox::stop_if_idle().await;
        tracing::info!("shutdown: browsers closed");
        // Open WebSockets (the island) would keep a graceful shutdown waiting: cap it.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            std::process::exit(0);
        });
    };
    axum::serve(tokio::net::TcpListener::bind(&bind).await?, app).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session::run(socket, st))
}

async fn tasks_list(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // The short list (active + last 24 h); older tasks are found through /api/history.
    // It stays light (polled often): reports only come with a single task.
    let list: Vec<_> = st.tasks.get().map(|t| t.log.recent()).unwrap_or_default().into_iter().map(|mut r| {
        if r.report.is_some() {
            r.report = None;
            r.report_status = "ready".into();
        }
        r
    }).collect();
    Json(serde_json::json!({"tasks": list}))
}

async fn task_get(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    match st.tasks.get().and_then(|t| t.log.get(&id)) {
        Some(r) => {
            // Pending actions carry their confirmation card (editable fields), as in the island.
            let cards: serde_json::Map<String, serde_json::Value> = r.actions.iter().filter(|a| a.status == "pending")
                .filter_map(|a| voice_agent::tasks::action_card(&a.id).map(|c| (a.id.clone(), c))).collect();
            let mut v = serde_json::json!(r);
            v["cards"] = serde_json::Value::Object(cards);
            Json(v).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Approve / cancel a drafted action from the menu bar or the overview: the same deterministic
/// gate as the island button and "yes, go ahead".
async fn task_delete(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(t) = st.tasks.get() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    match t.log.delete(&id) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(why) => (if why == "no such task" { StatusCode::NOT_FOUND } else { StatusCode::CONFLICT }, Json(serde_json::json!({"ok": false, "message": why}))).into_response(),
    }
}

/// What Sam keeps on disk, the retention settings, and this Mac's resources (for onboarding).
async fn storage_get() -> impl IntoResponse {
    let s = voice_agent::storage::settings();
    Json(serde_json::json!({"settings": s, "sizes": voice_agent::storage::sizes(), "ram": ram_bytes()}))
}

async fn storage_set(Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    match voice_agent::storage::update(&b) {
        Ok(s) => Json(serde_json::json!({"ok": true, "settings": s})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

async fn storage_cleanup(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let s = voice_agent::storage::settings();
    let (freed, notes) = voice_agent::storage::cleanup(&st, &s).await;
    let mut s2 = voice_agent::storage::settings();
    s2.last_cleanup = Some(chrono::Local::now().timestamp_millis());
    s2.last_freed_bytes = freed;
    let _ = voice_agent::storage::save(&s2);
    Json(serde_json::json!({"ok": true, "freed": freed, "notes": notes}))
}

#[cfg(target_os = "macos")]
fn ram_bytes() -> u64 {
    let mut v: u64 = 0;
    let mut n = std::mem::size_of::<u64>();
    let name = std::ffi::CString::new("hw.memsize").unwrap();
    unsafe { libc::sysctlbyname(name.as_ptr(), &mut v as *mut u64 as *mut libc::c_void, &mut n, std::ptr::null_mut(), 0) };
    v
}

/// Other platforms (the Linux build for security scanning): /proc/meminfo.
#[cfg(not(target_os = "macos"))]
fn ram_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())).map(|kb| kb * 1024).unwrap_or(0)
}

async fn history_search(State(st): State<Arc<AppState>>, axum::extract::Query(q): axum::extract::Query<voice_agent::history::Query>) -> impl IntoResponse {
    let Some(h) = st.tasks.get().and_then(|t| t.log.history.clone()) else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    match h.search(&q).await {
        Ok((hits, total)) => Json(voice_agent::history::hits_json(&hits, total)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("{e:#}")}))).into_response(),
    }
}

/// A model download started from the onboarding (Ollama), for the UI to poll.
#[derive(Clone, Default, serde::Serialize)]
struct Pull { model: String, status: String, completed: u64, total: u64, done: bool, error: Option<String> }
static PULL: std::sync::Mutex<Option<Pull>> = std::sync::Mutex::new(None);

fn start_pull(st: Arc<AppState>, model: String) {
    let mut g = PULL.lock().unwrap();
    if g.as_ref().is_some_and(|p| !p.done) {
        return;
    }
    *g = Some(Pull { model: model.clone(), status: "starting".into(), ..Default::default() });
    drop(g);
    tokio::spawn(async move {
        let r: anyhow::Result<()> = async {
            use futures_util::StreamExt;
            let resp = st.http.post(format!("{}/api/pull", st.cfg.ollama_url.trim_end_matches('/'))).json(&serde_json::json!({"model": model, "stream": true}))
                .timeout(std::time::Duration::from_secs(4 * 3600)).send().await?.error_for_status()?;
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                buf.extend_from_slice(&chunk?);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else { continue };
                    if let Some(e) = v["error"].as_str() {
                        anyhow::bail!("{e}");
                    }
                    if let Some(p) = PULL.lock().unwrap().as_mut() {
                        p.status = v["status"].as_str().unwrap_or("").to_string();
                        if let Some(t) = v["total"].as_u64() {
                            p.total = t;
                            p.completed = v["completed"].as_u64().unwrap_or(0);
                        }
                    }
                }
            }
            Ok(())
        }.await;
        if let Some(p) = PULL.lock().unwrap().as_mut() {
            p.done = true;
            if let Err(e) = r {
                p.error = Some(format!("{e:#}"));
            }
        }
        tracing::info!("ollama pull finished");
    });
}

/// The AI provider (onboarding): which providers are ready (a key stored, Ollama running, Claude
/// Code signed in) — never a key itself. For Ollama also the installed and recommended models
/// and the state of a download in progress.
async fn llm_settings_get(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    use voice_agent::llm::Provider;
    let ollama = voice_agent::llm::ollama_models(&st).await;
    let claude = match &st.cfg.claude_bin { Some(b) => voice_agent::claude_cli::auth_ok(b).await, None => false };
    let providers: Vec<serde_json::Value> = Provider::ALL.iter().map(|p| {
        let models = [voice_agent::llm::Tier::Light, voice_agent::llm::Tier::Standard, voice_agent::llm::Tier::Heavy].map(|t| st.cfg.model(*p, t).to_string());
        let ready = match p { Provider::Ollama => ollama.is_some(), Provider::ClaudeCode => claude, _ => st.cfg.key(*p).is_some() };
        let mut v = serde_json::json!({"id": p.id(), "label": p.label(), "has_key": ready, "needs_key": p.needs_key(), "models": models});
        if *p == Provider::Ollama {
            v["installed"] = serde_json::json!(ollama.clone().unwrap_or_default());
            v["recommended"] = serde_json::json!(voice_agent::llm::OLLAMA_RECOMMENDED.iter().map(|(m, d)| serde_json::json!([m, d])).collect::<Vec<_>>());
        }
        if *p == Provider::ClaudeCode {
            v["installed"] = serde_json::json!(st.cfg.claude_bin.as_ref().map(|b| vec![b.display().to_string()]).unwrap_or_default());
        }
        v
    }).collect();
    Json(serde_json::json!({"provider": st.cfg.provider.id(), "providers": providers, "pull": PULL.lock().unwrap().clone()}))
}

/// {"provider": "minimax|openai|anthropic|gemini|mistral|ollama|claude-code", "key": "…" (cloud
/// providers; optional if one is stored), "model": "…" (ollama: the model for every tier)}:
/// checks the provider with a real call, stores the key in the Keychain, saves the choice +
/// verified tier models in data/llm.json and restarts the server so everything uses it. An
/// Ollama model that isn't downloaded yet is pulled first: the reply says `pulling` and the UI
/// polls GET until `pull.done`, then asks again.
async fn llm_settings_set(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    use voice_agent::llm::{Provider, Tier};
    let Some(p) = b["provider"].as_str().and_then(Provider::parse) else { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": "unknown provider"}))).into_response() };
    let mut wanted = [Tier::Light, Tier::Standard, Tier::Heavy].map(|t| st.cfg.model(p, t).to_string());
    let key = if p.needs_key() {
        let key = b["key"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_string).or_else(|| st.cfg.key(p).map(str::to_string));
        let Some(key) = key else { return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": "paste the API key"}))).into_response() };
        key
    } else {
        String::new()
    };
    if p == Provider::Ollama {
        if let Some(m) = b["model"].as_str().map(str::trim).filter(|m| !m.is_empty()) {
            wanted = [m.to_string(), m.to_string(), m.to_string()];
        }
        let Some(installed) = voice_agent::llm::ollama_models(&st).await else {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": "Ollama isn't running: install it from ollama.com, open it, then try again"}))).into_response();
        };
        if !installed.iter().any(|i| i == &wanted[1]) {
            start_pull(st.clone(), wanted[1].clone());
            return Json(serde_json::json!({"ok": false, "pulling": true, "message": format!("Downloading {}…", wanted[1])})).into_response();
        }
    }
    let models = match voice_agent::llm::check_key(&st, p, &key, wanted).await {
        Ok(m) => m,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    };
    // Always in the Keychain (also a key that came from a development .env): Sam.app has no .env.
    if p.needs_key() {
        if let Err(e) = voice_agent::secrets::kc_set("sam.llm", p.id(), &key) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "message": format!("couldn't store the key in the Keychain: {e:#}")}))).into_response();
        }
    }
    let mut settings = voice_agent::config::llm_settings();
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    settings["provider"] = serde_json::json!(p.id());
    settings["models"][p.id()] = serde_json::json!(models);
    let _ = std::fs::write(voice_agent::thinker::data_dir().join("llm.json"), serde_json::to_string_pretty(&settings).unwrap_or_default());
    tracing::info!(provider = p.id(), ?models, "AI provider set; restarting");
    restart_soon();
    Json(serde_json::json!({"ok": true, "provider": p.id(), "models": models, "restarting": true})).into_response()
}

/// The friend layer (friend.rs): the user's settings, what Sam knows about them (every line
/// deletable), and the check-in log. Never a secret.
async fn friend_get(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let s = voice_agent::friend::load();
    let lines: Vec<serde_json::Value> = voice_agent::friend::card_lines(&st).into_iter().map(|(t, src)| serde_json::json!({"text": t, "source": src})).collect();
    // Open threads: what Sam will ask about ("how did the interview go?"), deletable on the You tab.
    let threads = st.memory.get().map(|m| m.open_threads()).unwrap_or_default();
    Json(serde_json::json!({"settings": s, "adult": s.adult(), "local_only": voice_agent::friend::local_only(&st), "lines": lines, "threads": threads, "stats": voice_agent::friend::stats()}))
}

/// {"name", "birthday", "tastes": {cat: [..]}, "psyche": [..], "tease", "spark", "nudges", "checkin", "adult_confirmed"}: any subset.
async fn friend_set(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let mut s = voice_agent::friend::load();
    if let Some(v) = b["name"].as_str() { s.name = v.trim().chars().take(60).collect(); }
    if let Some(v) = b["birthday"].as_str() { s.birthday = v.trim().to_string(); }
    if let Some(v) = b["home"].as_str() { let v = v.trim().chars().take(80).collect::<String>(); if v != s.home { s.home_lat = None; s.home_lon = None; } s.home = v; }
    if let Some(o) = b["tastes"].as_object() {
        s.tastes = o.iter().map(|(k, v)| (k.clone(), v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| x.trim().chars().take(40).collect::<String>()).filter(|x| !x.is_empty()).take(12).collect())).collect();
    }
    if let Some(a) = b["psyche"].as_array() { s.psyche = a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().chars().take(120).collect()).filter(|x: &String| !x.is_empty()).take(6).collect(); }
    for (k, field) in [("tease", &mut s.tease), ("spark", &mut s.spark), ("nudges", &mut s.nudges)] {
        if let Some(v) = b[k].as_str() { *field = v.trim().to_lowercase(); }
    }
    if let Some(v) = b["checkin"].as_bool() { s.checkin = v; }
    if let Some(v) = b["adult_confirmed"].as_bool() { s.adult_confirmed = v; }
    // Flirting is for adults who said so; the birthday decides.
    if s.spark == "flirty" && !(s.adult() && s.adult_confirmed) { s.spark = "playful".into(); }
    if let Err(e) = voice_agent::friend::save(&s) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response();
    }
    let _ = &st;
    tracing::info!(name = %s.name, home = %s.home, birthday = %s.birthday, tease = %s.tease, spark = %s.spark, checkin = s.checkin, tastes = s.tastes.values().map(|v| v.len()).sum::<usize>(), psyche = s.psyche.len(), "friend settings saved");
    Json(serde_json::json!({"ok": true, "settings": s, "adult": s.adult()})).into_response()
}

/// {"line": "..."}: forget one profile line for good.
async fn friend_forget(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    // {"thread": id}: Sam won't ask about it (kept as history, status dropped).
    if let Some(id) = b["thread"].as_i64() {
        return match st.memory.get() {
            Some(m) => {
                m.mark_thread(id, "dropped");
                Json(serde_json::json!({"ok": true})).into_response()
            }
            None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    }
    let Some(line) = b["line"].as_str().filter(|l| !l.trim().is_empty()) else { return StatusCode::BAD_REQUEST.into_response() };
    match voice_agent::friend::forget(&st, line) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

/// Runs the check-in now (the overview's "try it", tests): timing rules skipped, everything else applies.
async fn friend_checkin(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    // {"say", "action", "intent": {...}}: ground and deliver this one item (tests, the lab).
    let seed = b.get("intent").filter(|i| i.is_object()).map(|_| b.clone());
    match voice_agent::friend::run_once(&st, true, seed).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::warn!("check-in now failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response()
        }
    }
}

/// {"client_id", "client_secret"}: Spotify developer-app credentials for searching (Keychain).
async fn spotify_setup(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    match voice_agent::music::spotify_setup(&st, &b).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

/// What Sam is playing (the island's now-playing card asks after a tap).
async fn music_get() -> impl IntoResponse {
    Json(voice_agent::music::now_playing().map(|n| voice_agent::music::view(&n)).unwrap_or(serde_json::json!({"type": "music", "state": "none"})))
}

/// The now-playing card's buttons: {"action": "pause" | "resume" | "next" | "previous"}.
async fn music_set(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let action = b["action"].as_str().unwrap_or("");
    if !matches!(action, "pause" | "resume" | "next" | "previous") {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": "action must be pause, resume, next or previous"}))).into_response();
    }
    match voice_agent::music::play(&st, "", "", action).await {
        Ok(said) => Json(serde_json::json!({"ok": true, "said": said, "now": voice_agent::music::now_playing().map(|n| voice_agent::music::view(&n))})).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

async fn voice_get() -> impl IntoResponse {
    Json(serde_json::json!({"voice": voice_agent::config::voice_choice(), "model": voice_agent::config::voice_model_choice()}))
}

/// {"voice": "male" | "female", "model": "tiny" | "light" | "steady"} (either key alone keeps the other):
/// Sam's voice for new conversations and the voice model the app launches the sidecar with.
async fn voice_set(Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let cur_v = voice_agent::config::voice_choice();
    let v = match b["voice"].as_str() { Some("female") => "female".to_string(), Some("male") => "male".to_string(), Some(_) => return StatusCode::BAD_REQUEST.into_response(), None => cur_v.clone() };
    let m = match b["model"].as_str() { Some(m @ ("tiny" | "light" | "steady")) => m.to_string(), Some(_) => return StatusCode::BAD_REQUEST.into_response(), None => voice_agent::config::voice_model_choice() };
    let _ = std::fs::write(voice_agent::thinker::data_dir().join("voice.json"), serde_json::json!({"voice": v, "model": m}).to_string());
    if v != cur_v {
        restart_soon();
    }
    Json(serde_json::json!({"ok": true, "voice": v, "model": m})).into_response()
}

/// Re-executes the server in place (same binary, args, env) so new settings apply everywhere.
fn restart_soon() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(800));
        use std::os::unix::process::CommandExt;
        if let Ok(exe) = std::env::current_exe() {
            let err = std::process::Command::new(exe).args(std::env::args().skip(1)).exec();
            tracing::warn!("restart failed: {err}");
        }
    });
}

async fn automations_list() -> impl IntoResponse {
    let Some(st) = voice_agent::automations::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let list: Vec<serde_json::Value> = st.list().into_iter().map(|a| { let h = st.history(&a.id, 10); serde_json::json!({"automation": a, "history": h}) }).collect();
    Json(serde_json::json!({"automations": list})).into_response()
}

/// {"action": "pause" | "resume"}
async fn automation_action(Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(st) = voice_agent::automations::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let ok = match b["action"].as_str() { Some("pause") => st.set_active(&id, false), Some("resume") => st.set_active(&id, true), _ => return StatusCode::BAD_REQUEST.into_response() };
    if ok { Json(serde_json::json!({"ok": true})).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn automation_delete(Path(id): Path<String>) -> impl IntoResponse {
    let Some(st) = voice_agent::automations::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    if st.delete(&id) { Json(serde_json::json!({"ok": true})).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn campaigns_list() -> impl IntoResponse {
    let Some(st) = voice_agent::leads::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let dir = voice_agent::thinker::data_dir().join("artifacts/leads");
    let list: Vec<serde_json::Value> = st.campaigns().into_iter().map(|mut c| {
        let csv = dir.join(format!("{}.csv", c["id"].as_str().unwrap_or("")));
        c["csv"] = serde_json::json!(csv.exists().then(|| csv.display().to_string()));
        c
    }).collect();
    Json(serde_json::json!({"campaigns": list})).into_response()
}

/// {"action": "pause" | "resume" | "export"}
async fn campaign_action(Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(st) = voice_agent::leads::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    match b["action"].as_str() {
        Some("pause") => st.set_campaign(&id, Some("paused"), None, None, Some("paused by the user"), None),
        Some("resume") => st.set_campaign(&id, Some("active"), None, Some(0), Some(""), None),
        Some("export") => { let _ = st.export_csv(&id, &voice_agent::thinker::data_dir().join("artifacts/leads")); }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn monitors_list() -> impl IntoResponse {
    let Some(st) = voice_agent::monitors::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    Json(serde_json::json!({"monitors": st.list()})).into_response()
}

async fn monitor_get(Path(id): Path<String>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> impl IntoResponse {
    let Some(st) = voice_agent::monitors::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Some(m) = st.list().into_iter().find(|m| m.id == id) else { return StatusCode::NOT_FOUND.into_response() };
    let since = q.get("since").and_then(|s| s.parse().ok());
    Json(serde_json::json!({"monitor": m, "listings": st.listings(&id, since, 300), "runs": st.recent_runs(&id, 20)})).into_response()
}

/// {"action": "pause" | "resume" | "run" | "interval", "minutes": n}
async fn monitor_action(Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(st) = voice_agent::monitors::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let ok = match b["action"].as_str() {
        Some("pause") => st.set_active(&id, false),
        Some("resume") => st.set_active(&id, true),
        Some("run") => st.run_now(&id),
        Some("interval") => b["minutes"].as_i64().is_some_and(|m| st.set_interval(&id, m)),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    if ok { Json(serde_json::json!({"ok": true})).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn monitor_delete(Path(id): Path<String>) -> impl IntoResponse {
    let Some(st) = voice_agent::monitors::store() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    if st.delete(&id) { Json(serde_json::json!({"ok": true})).into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn tasks_clear(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(t) = st.tasks.get() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    Json(serde_json::json!({"ok": true, "deleted": t.log.clear_finished()})).into_response()
}

async fn task_confirm(State(st): State<Arc<AppState>>, Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(t) = st.tasks.get() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Some(rec) = t.log.get(&id) else { return StatusCode::NOT_FOUND.into_response() };
    let Some(aid) = b["action_id"].as_str().filter(|a| rec.actions.iter().any(|x| x.id == *a && x.status == "pending")) else {
        return StatusCode::CONFLICT.into_response();
    };
    if let Some(args) = b.get("args").filter(|a| a.is_object()) {
        voice_agent::tools::Pending::new(&voice_agent::thinker::data_dir()).update_args(aid, args);
    }
    let text = if b["decision"] == "approve" { format!("yes, go ahead {aid}") } else { format!("no, cancel {aid}") };
    t.submit(voice_agent::tasks::Job::Request { session: rec.session.clone(), kind: "confirm_click".into(), text });
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn guard_local(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let path = req.uri().path();
    if path == "/ws" || path.starts_with("/api/") {
        let h = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok());
        let port = env_or("BIND", "127.0.0.1:47800").rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(47800);
        if let Err(why) = voice_agent::localauth::check(h("host"), h("origin"), h("x-sam-token"), port) {
            tracing::warn!(path, why, "local request refused");
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(req).await
}

fn files() -> anyhow::Result<voice_agent::files::Files> {
    voice_agent::files::Files::open(&voice_agent::thinker::data_dir())
}

async fn connectors_list() -> impl IntoResponse {
    match files() {
        Ok(f) => Json(voice_agent::connectors::list(&f)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn connector_action(State(st): State<Arc<AppState>>, Path(id): Path<String>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let r = match files() {
        Ok(f) => voice_agent::connectors::action(&st, &f, &id, b["action"].as_str().unwrap_or(""), &b["values"]).await,
        Err(e) => Err(e),
    };
    match r {
        Ok(msg) => Json(serde_json::json!({"ok": true, "message": msg})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

async fn permission_probe(Path(app): Path<String>) -> impl IntoResponse {
    match voice_agent::apps::probe(&app).await {
        Ok(msg) => Json(serde_json::json!({"ok": true, "message": msg})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "message": format!("{e:#}")}))).into_response(),
    }
}

/// Submit a background request (overview / lab workloads): {"text": "...", "kind": "..."}.
async fn task_submit(State(st): State<Arc<AppState>>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(t) = st.tasks.get() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let text = b["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    t.submit(voice_agent::tasks::Job::Request { session: "api".into(), kind: b["kind"].as_str().unwrap_or("").into(), text });
    Json(serde_json::json!({"ok": true})).into_response()
}

/// A task's own file for the overview (reports, briefs, images): only Sam's artifacts or
/// files a task lists, never anything else on disk (the app itself can't read ~/Documents
/// without a privacy prompt, and this must not become a general file reader).
async fn file_get(State(st): State<Arc<AppState>>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> impl IntoResponse {
    let Some(want) = q.get("path").and_then(|p| std::path::Path::new(p).canonicalize().ok()) else { return StatusCode::NOT_FOUND.into_response() };
    let artifacts = voice_agent::thinker::data_dir().join("artifacts").canonicalize().ok();
    let listed = st.tasks.get().is_some_and(|t| t.log.list().iter().any(|r| r.files.iter().chain(r.steps.iter().filter_map(|s| s.file.as_ref())).any(|f| std::path::Path::new(f).canonicalize().ok().as_ref() == Some(&want))));
    let inside = artifacts.as_ref().is_some_and(|a| want.starts_with(a));
    if !(inside || listed) || voice_agent::files::is_denied(&want) || want.is_dir() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(meta) = std::fs::metadata(&want) else { return StatusCode::NOT_FOUND.into_response() };
    if meta.len() > 20 << 20 {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let ext = want.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let ctype = match ext.as_str() {
        "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "gif" => "image/gif", "webp" => "image/webp", "svg" => "image/svg+xml", "pdf" => "application/pdf",
        "json" | "jsonl" => "application/json", "csv" => "text/csv", "html" => "text/plain; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };
    match std::fs::read(&want) {
        Ok(b) => ([(axum::http::header::CONTENT_TYPE, ctype)], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
