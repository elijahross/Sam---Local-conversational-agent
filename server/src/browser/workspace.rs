//! "Sam Workspace": an invisible virtual display (sidecar/workspace) for Sam's own browser
//! windows, so a Chrome launch or a new window never flashes on the user's screens. Started on
//! the first browser use and kept for the server's lifetime (one display reconfiguration, not
//! one per task); it exits by itself when the server does (its stdin closes).
//! `WORKSPACE_DISPLAY=0` disables it (windows are then parked off-screen as before).

use std::sync::{Mutex, OnceLock};

/// (x, y, w, h) of the workspace display in global top-left coordinates.
pub type Frame = (i64, i64, i64, i64);

static FRAME: OnceLock<Option<Frame>> = OnceLock::new();
static CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

fn start() -> Option<Frame> {
    if crate::env_or("WORKSPACE_DISPLAY", "1") == "0" {
        return None;
    }
    let bin = std::env::var("WORKSPACE_BIN").unwrap_or_else(|_| match crate::config::app_resources() {
        Some(r) => r.join("bin/sam-workspace").display().to_string(),
        None => concat!(env!("CARGO_MANIFEST_DIR"), "/../sidecar/workspace/build/sam-workspace").to_string(),
    });
    let mut child = std::process::Command::new(&bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| tracing::warn!("workspace display unavailable ({bin}): {e}"))
        .ok()?;
    let mut line = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    let f = (v["x"].as_i64()?, v["y"].as_i64()?, v["w"].as_i64()?, v["h"].as_i64()?);
    tracing::info!(?f, "Sam Workspace display ready");
    // Keep the child (and its stdin) alive for the server's lifetime.
    *CHILD.lock().unwrap() = Some(child);
    Some(f)
}

/// The workspace frame, starting the display on first use (blocking ~1 s, once).
pub fn frame() -> Option<Frame> {
    *FRAME.get_or_init(start)
}

/// Where Sam's browser windows go: inside the workspace, else far off-screen.
pub fn park_origin() -> (i64, i64) {
    match frame() {
        Some((x, y, _, _)) => (x + 40, y + 40),
        None => (-2400, 0),
    }
}
