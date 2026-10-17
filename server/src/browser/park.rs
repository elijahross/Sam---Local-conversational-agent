//! Browser tabs waiting for the user's OK (website tasks in approval mode): the filled-in page
//! stays open in Sam's off-screen browser until the final click is approved, at most 30 min.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;

use super::cdp::{Chrome, Tab};

pub struct Parked {
    pub chrome: Chrome,
    pub tab: Tab,
    /// The final action (agent::Outcome::submit).
    pub info: Value,
    at: Instant,
}

static PARKED: LazyLock<Mutex<HashMap<String, Parked>>> = LazyLock::new(Default::default);
/// A parked tab holds a whole Chrome (~850 MB, perf lab): kept 15 min by default.
static TTL: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| Duration::from_secs(crate::env_num("PARK_TTL_MIN", 15u64) * 60));

static REAPER: std::sync::Once = std::sync::Once::new();

/// Keeps `tab` (and its browser) for a later approval; returns the parking id. Expired tabs are
/// closed by a reaper every minute (not only when the next tab is parked).
pub fn park(chrome: Chrome, tab: Tab, info: Value) -> String {
    REAPER.call_once(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let old: Vec<Parked> = {
                    let mut m = PARKED.lock().unwrap();
                    let ids: Vec<String> = m.iter().filter(|(_, p)| p.at.elapsed() > *TTL).map(|(k, _)| k.clone()).collect();
                    ids.iter().filter_map(|k| m.remove(k)).collect()
                };
                for p in old {
                    p.tab.close().await;
                    p.chrome.shutdown().await;
                }
            }
        });
    });
    let id = format!("p{}", chrono::Local::now().timestamp_millis());
    let expired: Vec<Parked> = {
        let mut m = PARKED.lock().unwrap();
        let old: Vec<String> = m.iter().filter(|(_, p)| p.at.elapsed() > *TTL).map(|(k, _)| k.clone()).collect();
        let out = old.iter().filter_map(|k| m.remove(k)).collect();
        m.insert(id.clone(), Parked { chrome, tab, info, at: Instant::now() });
        out
    };
    for p in expired {
        tokio::spawn(async move {
            p.tab.close().await;
            p.chrome.shutdown().await;
        });
    }
    id
}

/// Takes a parked tab (None if it expired or the server restarted).
pub fn take(id: &str) -> Option<Parked> {
    PARKED.lock().unwrap().remove(id).filter(|p| p.at.elapsed() <= *TTL)
}

/// Discards a parked tab (the user cancelled).
pub async fn discard(id: &str) {
    let p = PARKED.lock().unwrap().remove(id);
    if let Some(p) = p {
        p.tab.close().await;
        p.chrome.shutdown().await;
    }
}

/// Server shutdown: close every parked tab (and release its browser).
pub fn discard_all() {
    let all: Vec<Parked> = PARKED.lock().unwrap().drain().map(|(_, p)| p).collect();
    drop(all);
}
