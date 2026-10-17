//! What Sam keeps on disk, and keeping it from growing forever: sizes for the onboarding's
//! "Requirements & storage" step, the user's retention settings, and a janitor that applies
//! them. Automation and monitor runs are routine: their outcome stays in their own stores
//! (automations.db `fired`, monitors.db), and their task records leave the task log and
//! history after `keep_runs_days`, so a daily briefing doesn't fill the history for years.
//!
//! Settings live in `data/storage.json`:
//!   cleanup_every   daily | weekly | monthly | never   (temporary files: browser caches,
//!                   run folders of finished work past the retention)
//!   keep_tasks_days 30 | 90 | 365 | 0 = forever        (finished tasks: history + artifacts)
//!   keep_runs_days  default 14                         (automation / monitor runs)
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "d_every")]
    pub cleanup_every: String,
    #[serde(default = "d_keep")]
    pub keep_tasks_days: i64,
    #[serde(default = "d_runs")]
    pub keep_runs_days: i64,
    #[serde(default)]
    pub last_cleanup: Option<i64>,
    #[serde(default)]
    pub last_freed_bytes: u64,
}
fn d_every() -> String { "weekly".into() }
fn d_keep() -> i64 { 90 }
fn d_runs() -> i64 { 14 }

impl Default for Settings {
    fn default() -> Self {
        Self { cleanup_every: d_every(), keep_tasks_days: d_keep(), keep_runs_days: d_runs(), last_cleanup: None, last_freed_bytes: 0 }
    }
}

fn path() -> PathBuf {
    crate::thinker::data_dir().join("storage.json")
}

pub fn settings() -> Settings {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(crate::thinker::data_dir())?;
    std::fs::write(path(), serde_json::to_string_pretty(s)?)?;
    Ok(())
}

/// Update from the onboarding / settings UI; unknown values keep the current ones.
pub fn update(v: &Value) -> anyhow::Result<Settings> {
    let mut s = settings();
    if let Some(e) = v["cleanup_every"].as_str().filter(|e| ["daily", "weekly", "monthly", "never"].contains(e)) {
        s.cleanup_every = e.into();
    }
    if let Some(d) = v["keep_tasks_days"].as_i64().filter(|d| [0, 30, 90, 365].contains(d)) {
        s.keep_tasks_days = d;
    }
    if let Some(d) = v["keep_runs_days"].as_i64().filter(|d| (1..=365).contains(d)) {
        s.keep_runs_days = d;
    }
    save(&s)?;
    Ok(s)
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            n += if md.is_dir() { dir_size(&e.path()) } else { md.len() };
        }
    }
    n
}

/// Sam's folders and their sizes (bytes), plus the volume's free space.
pub fn sizes() -> Value {
    let data = crate::thinker::data_dir();
    let root = data.parent().map(Path::to_path_buf).unwrap_or_else(|| data.clone());
    let free = fs2_free(&data).unwrap_or(0);
    json!({
        "models": dir_size(&root.join("models")),
        "runtime": dir_size(&root.join("runtime")),
        "browser": dir_size(&data.join("browser")),
        "artifacts": dir_size(&data.join("artifacts")),
        "history": dir_size(&data.join("history.db")) + dir_size(&data.join("history.db-wal")),
        "memory": dir_size(&data.join("memory.sqlite")) + dir_size(&data.join("knowledge.db")),
        "data_total": dir_size(&data),
        "free": free,
        "data_dir": data.display().to_string(),
        "chrome": Path::new(crate::browser::cdp::CHROME).exists(),
    })
}

fn fs2_free(p: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

fn remove_tree(p: &Path) -> u64 {
    let n = dir_size(p);
    let _ = std::fs::remove_dir_all(p);
    n
}

fn age_days(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).map(|d| (d.as_secs() / 86_400) as i64).unwrap_or(0)
}

/// One cleanup pass. Returns what it freed (bytes) and a few lines for the log / UI.
pub async fn cleanup(app: &crate::AppState, s: &Settings) -> (u64, Vec<String>) {
    let data = crate::thinker::data_dir();
    let mut freed = 0u64;
    let mut notes = Vec::new();
    let now = chrono::Local::now().timestamp_millis();
    // 1. Automation / monitor runs: routine records out of the task log and the history.
    if let Some(t) = app.tasks.get() {
        let cutoff = now - s.keep_runs_days * 86_400_000;
        let old: Vec<String> = t.log.list().into_iter()
            .filter(|r| matches!(r.session.as_str(), "automation" | "monitor") && r.updated < cutoff && !matches!(r.status.as_str(), "running" | "queued" | "needs_action"))
            .map(|r| r.id).collect();
        let mut n = 0;
        for id in &old {
            if t.log.delete(id).is_ok() { n += 1; }
            freed += remove_tree(&data.join("artifacts/reports").join(id));
        }
        if let Some(h) = t.log.history.clone() {
            n += h.delete_runs_before(cutoff);
        }
        if n > 0 { notes.push(format!("{n} automation / monitor runs older than {} days removed", s.keep_runs_days)); }
        // 2. Finished tasks past the retention: history rows and their reports.
        if s.keep_tasks_days > 0 {
            let cutoff = now - s.keep_tasks_days * 86_400_000;
            if let Some(h) = t.log.history.clone() {
                let ids = h.finished_before(cutoff);
                for id in &ids {
                    let _ = t.log.delete(id);
                    h.delete(id);
                    freed += remove_tree(&data.join("artifacts/reports").join(id));
                }
                if !ids.is_empty() { notes.push(format!("{} finished tasks older than {} days removed from the history", ids.len(), s.keep_tasks_days)); }
            }
        }
    }
    // 3. Temporary files: run folders of finished work past the retention, browser caches.
    let keep = if s.keep_tasks_days > 0 { s.keep_tasks_days } else { 365 };
    for kind in ["research", "scrape", "reddit", "events", "sandbox", "knowledge", "leads"] {
        let base = data.join("artifacts").join(kind);
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && age_days(&p) > keep {
                freed += remove_tree(&p);
            }
        }
    }
    // Chrome caches are rebuilt on demand; cookies and sessions stay. Only while no Sam browser runs.
    if crate::browser::cdp::shared_counts().is_empty() {
        if let Ok(rd) = std::fs::read_dir(data.join("browser/profiles")) {
            for prof in rd.flatten() {
                for cache in ["Default/Cache", "Default/Code Cache", "Default/GPUCache", "Default/Service Worker/CacheStorage", "Default/DawnCache", "GrShaderCache", "ShaderCache", "GraphiteDawnCache"] {
                    let p = prof.path().join(cache);
                    if p.is_dir() { freed += remove_tree(&p); }
                }
            }
        }
        notes.push("browser caches cleared (sessions kept)".into());
    }
    (freed, notes)
}

fn due(s: &Settings) -> bool {
    let every_days = match s.cleanup_every.as_str() { "daily" => 1, "weekly" => 7, "monthly" => 30, _ => return false };
    match s.last_cleanup {
        None => true,
        Some(t) => chrono::Local::now().timestamp_millis() - t > every_days * 86_400_000,
    }
}

/// Runs the cleanup when due (checked hourly; first check a few minutes after start).
pub fn spawn(app: std::sync::Arc<crate::AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
        loop {
            let s = settings();
            if due(&s) {
                let (freed, notes) = cleanup(&app, &s).await;
                let mut s2 = settings();
                s2.last_cleanup = Some(chrono::Local::now().timestamp_millis());
                s2.last_freed_bytes = freed;
                let _ = save(&s2);
                tracing::info!(freed_mb = freed / 1_048_576, ?notes, "storage: cleanup done");
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}
