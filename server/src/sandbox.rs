//! Runs agent-generated code in a throwaway VM. Default runtime: Apple `container` (one
//! lightweight VM per run); fallback: Docker (shared Linux VM). Lab: sandbox lab.
//!
//! Isolation per run (verified by sandbox lab):
//! - no network; read-only root fs; non-root uid; /tmp is a small tmpfs;
//! - inputs mounted READ-ONLY, and only paths that pass `Files::check` (user grants/denylist);
//! - code mounted read-only; a fresh /out is the only writable host path;
//! - CPU / memory (swap included) / pids caps; wall-clock timeout kills the VM;
//! - no env vars, no secrets, no host home: the VM sees /code, /in, /out only.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::Serialize;

use crate::files::Files;

pub const IMAGE: &str = "sam-sandbox:1";
const STDOUT_CAP: usize = 4_000;
const STDERR_CAP: usize = 1_500;
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Runtime {
    AppleContainer,
    Docker,
}

impl Runtime {
    fn bin(self) -> &'static str {
        match self {
            Runtime::AppleContainer => "container",
            Runtime::Docker => "docker",
        }
    }
    pub fn from_env() -> Self {
        if crate::env_or("SANDBOX", "container") == "docker" { Runtime::Docker } else { Runtime::AppleContainer }
    }
}

pub struct Sandbox {
    pub runtime: Runtime,
    pub image: String,
    /// Per-run dirs (code + out) live here: data/artifacts/sandbox/<id>/.
    pub root: PathBuf,
    pub memory_mb: u32,
    pub cpus: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub id: String,
    pub exit: i32,
    pub timed_out: bool,
    /// Capped; the model only ever sees these caps.
    pub stdout: String,
    pub stderr_tail: String,
    /// Files the code wrote to /out (name, bytes).
    pub outputs: Vec<(String, u64)>,
    pub out_dir: String,
    pub wall_ms: u128,
}

impl Sandbox {
    pub fn new(data_dir: &Path) -> Self {
        Self { runtime: Runtime::from_env(), image: IMAGE.into(), root: data_dir.join("artifacts/sandbox"), memory_mb: 1024, cpus: 2 }
    }

    /// One sandbox VM at a time (each costs ~1 GB): parallel tasks queue here.
    /// Run `code` (Python) with `inputs` mounted read-only at /in/<file name>.
    /// Every input goes through `files.check` first: no grant → no mount.
    pub async fn run(&self, files: &Files, code: &str, inputs: &[PathBuf], timeout: Duration) -> anyhow::Result<RunResult> {
        let t0 = Instant::now();
        // One VM at a time; parallel tasks wait here.
        let _vm = VM.acquire().await?;
        if self.runtime == Runtime::AppleContainer && !SERVICE_UP.swap(true, Ordering::SeqCst) {
            // The container service is stopped while idle (its VM holds ~1 GB): start on demand.
            let _ = tokio::process::Command::new("container").args(["system", "start"]).output().await;
        }
        let r = self.run_inner(files, code, inputs, timeout, t0).await;
        if self.runtime == Runtime::AppleContainer {
            schedule_service_stop();
        }
        r
    }

    async fn run_inner(&self, files: &Files, code: &str, inputs: &[PathBuf], timeout: Duration, t0: Instant) -> anyhow::Result<RunResult> {
        let id = format!("{}-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let dir = self.root.join(&id);
        let (code_dir, out_dir) = (dir.join("code"), dir.join("out"));
        std::fs::create_dir_all(&code_dir)?;
        std::fs::create_dir_all(&out_dir)?;
        std::fs::write(code_dir.join("main.py"), code)?;
        // The VM runs as uid 10001: /out must be writable for it, code readable.
        set_mode(&out_dir, 0o777);
        set_mode(&code_dir, 0o755);
        set_mode(&code_dir.join("main.py"), 0o644);

        let mut mounts = Vec::new();
        let mut names = std::collections::HashSet::new();
        for p in inputs {
            let real = files.check(p)?;
            // ':' or ',' in a path would break / inject into the -v mount spec.
            if real.to_string_lossy().contains([':', ',']) {
                anyhow::bail!("input path contains ':' or ',' — rename it first: {}", real.display());
            }
            let mut name = real.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "input".into());
            let mut n = 2;
            while !names.insert(name.clone()) {
                name = format!("{n}_{name}");
                n += 1;
            }
            mounts.push(format!("{}:/in/{}:ro", real.display(), name));
        }

        let name = format!("sam-sb-{id}");
        let mut args: Vec<String> = vec![
            "run".into(), "--rm".into(), "--name".into(), name.clone(),
            "--network".into(), "none".into(), "--read-only".into(),
            "--tmpfs".into(), "/tmp".into(),
            "--cpus".into(), self.cpus.to_string(),
            "--memory".into(), format!("{}m", self.memory_mb),
            "-v".into(), format!("{}:/code:ro", code_dir.display()),
            "-v".into(), format!("{}:/out", out_dir.display()),
        ];
        if self.runtime == Runtime::Docker {
            for a in ["--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "128"] {
                args.push(a.into());
            }
            args.push("--memory-swap".into());
            args.push(format!("{}m", self.memory_mb));
        }
        for m in mounts {
            args.push("-v".into());
            args.push(m);
        }
        args.push(self.image.clone());

        let child = tokio::process::Command::new(self.runtime.bin())
            .args(&args)
            .env_clear()
            .env("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {}", self.runtime.bin()))?;
        let (exit, timed_out, stdout, stderr) = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => {
                let o = out?;
                (o.status.code().unwrap_or(-1), false, String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
            }
            Err(_) => {
                let _ = tokio::process::Command::new(self.runtime.bin()).args(["kill", &name]).output().await;
                (-1, true, String::new(), format!("timed out after {} s", timeout.as_secs()))
            }
        };
        let outputs = std::fs::read_dir(&out_dir)
            .map(|rd| rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), e.metadata().ok()?.len()))).collect())
            .unwrap_or_default();
        Ok(RunResult {
            id,
            exit,
            timed_out,
            stdout: head(&stdout, STDOUT_CAP),
            stderr_tail: tail(&clean_stderr(&stderr), STDERR_CAP),
            outputs,
            out_dir: out_dir.display().to_string(),
            wall_ms: t0.elapsed().as_millis(),
        })
    }
}

/// Apple `container` prints progress lines on stderr; keep only the program's own output.
fn clean_stderr(s: &str) -> String {
    s.lines().filter(|l| !l.starts_with('[') || l.contains("Error")).collect::<Vec<_>>().join("\n")
}

fn head(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…[truncated]", s.chars().take(n).collect::<String>()) }
}

fn tail(s: &str, n: usize) -> String {
    let c = s.chars().count();
    if c <= n { s.to_string() } else { format!("[…]{}", s.chars().skip(c - n).collect::<String>()) }
}

fn set_mode(p: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
    }
}

static VM: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static SERVICE_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_USE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Stops the container service 5 minutes after the last sandbox run (no run in between), so
/// its VM doesn't hold ~1 GB in the background. Started again on the next run.
fn schedule_service_stop() {
    let now = chrono::Local::now().timestamp();
    LAST_USE.store(now, Ordering::SeqCst);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(crate::env_num("SANDBOX_IDLE_S", 300u64))).await;
        if LAST_USE.load(Ordering::SeqCst) == now && VM.available_permits() == 1 {
            // Only if nothing else (e.g. the user's own containers) runs on the service.
            let busy = tokio::process::Command::new("container").args(["ls", "-q"]).output().await.map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty()).unwrap_or(true);
            if !busy && SERVICE_UP.swap(false, Ordering::SeqCst) {
                let _ = tokio::process::Command::new("container").args(["system", "stop"]).output().await;
                tracing::info!("sandbox container service stopped (idle)");
            }
        }
    });
}

/// Process exit (CLI end, server shutdown): stop the container service now if this process
/// started it and nothing else runs on it.
pub async fn stop_if_idle() {
    if !SERVICE_UP.swap(false, Ordering::SeqCst) {
        return;
    }
    let busy = tokio::process::Command::new("container").args(["ls", "-q"]).output().await.map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty()).unwrap_or(true);
    if !busy {
        let _ = tokio::process::Command::new("container").args(["system", "stop"]).output().await;
    }
}
