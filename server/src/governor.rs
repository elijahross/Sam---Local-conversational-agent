//! Resource governor for background work: a parallel-task cap, a bounded queue, and memory-aware
//! admission, so many requests never open unlimited browsers / VMs or push the Mac into swap or
//! OOM. New requests beyond the queue are refused with a "busy" answer.
//!
//! Knobs (env): MAX_PARALLEL (2), MAX_QUEUE (8), MEM_RESERVE_MB (1500), TASK_TIMEOUT_MIN (20).

use std::time::Duration;

pub fn max_parallel() -> usize {
    crate::env_num("MAX_PARALLEL", 2usize).max(1)
}
pub fn max_queue() -> usize {
    crate::env_num("MAX_QUEUE", 8usize)
}
pub fn reserve_mb() -> u64 {
    crate::env_num("MEM_RESERVE_MB", 1500u64)
}
pub fn task_timeout() -> Duration {
    Duration::from_secs(crate::env_num("TASK_TIMEOUT_MIN", 20u64) * 60)
}

/// Memory a request of this kind needs on top of what's running (rough, measured in perf lab).
pub fn need_mb(kind: &str, text: &str) -> u64 {
    let t = text.to_lowercase();
    match kind {
        "code" => 1500,
        "research" | "browser" | "web" | "message" | "scrape" => 900,
        "email" if t.contains("gmail") => 900,
        "documents" => 400,
        _ if t.contains("http") || t.contains("website") || t.contains("browse") => 900,
        _ => 150,
    }
}

/// Memory macOS can hand out without swapping: free + inactive + speculative + purgeable pages.
pub fn available_mb() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let page: u64 = s.split("page size of ").nth(1)?.split_whitespace().next()?.parse().ok()?;
    let n = |k: &str| -> u64 { s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().trim_end_matches('.').parse().ok()).unwrap_or(0) };
    Some((n("Pages free") + n("Pages inactive") + n("Pages speculative") + n("Pages purgeable")) * page / (1 << 20))
}

/// Waits until the job fits in memory (need + reserve). `alone` = nothing else running: then
/// only `need` is required (the reserve is for running work), and after `patience` it starts
/// anyway if at least `need` is free — a busy Mac delays work, it never deadlocks it.
pub async fn wait_for_memory(need: u64, alone: impl Fn() -> bool, mut on_wait: impl FnMut(u64)) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        let avail = tokio::task::spawn_blocking(available_mb).await.ok().flatten().unwrap_or(u64::MAX);
        let want = if alone() { need } else { need + reserve_mb() };
        if avail >= want {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(600) {
            return Err(format!("not enough free memory ({avail} MB free, {need} MB needed); close some apps and ask again"));
        }
        on_wait(avail);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
