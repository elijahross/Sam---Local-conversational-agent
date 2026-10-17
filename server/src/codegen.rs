//! "Write code instead of reading data": the model sees only a short preview of each input
//! (shape, first lines), writes ONE Python script, the script runs in the sandbox VM over the
//! full data, and only its compact JSON answer comes back.
//!
//! Token cost is independent of data size. Repairs carry only the latest script + the last
//! error tail (RawKit pattern) — context never accumulates across attempts.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    AppState,
    files::Files,
    llm::{self, Provider},
    sandbox::{RunResult, Sandbox},
};

const SYSTEM: &str = "You write ONE self-contained Python 3.12 script (standard library + pandas only; no network, no installs).\n\
- Inputs are read-only files in /in/ (names given below). Parse them robustly (currency symbols, thousands separators, missing values).\n\
- Write any requested output files to /out/.\n\
- The LAST line printed to stdout must be ONE compact JSON object with the answer (max ~1500 chars; never print whole datasets).\n\
Reply with only the script in a ```python block.";

/// Inputs up to this size are shown in full in the prompt.
const FULL_PREVIEW_CHARS: usize = 2_500;

#[derive(Debug, Clone)]
pub struct Options {
    pub model: Provider,
    pub max_repairs: usize,
    pub timeout: Duration,
    pub preview_lines: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { model: Provider::MiniMax, max_repairs: 2, timeout: Duration::from_secs(90), preview_lines: 4 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub n: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub exit: i32,
    pub error: Option<String>,
    pub run_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct Solution {
    pub task: String,
    /// "done" | "failed"
    pub status: String,
    pub answer: Value,
    pub outputs: Vec<(String, u64)>,
    pub out_dir: String,
    pub code: String,
    pub attempts: Vec<Attempt>,
    pub tokens: u64,
    pub wall_ms: u128,
}

pub async fn solve(app: &AppState, sb: &Sandbox, files: &Files, task: &str, inputs: &[PathBuf], opt: &Options) -> anyhow::Result<Solution> {
    let t0 = Instant::now();
    // Documents (PDF/Office) are converted to markdown first so scripts only face text.
    let staged = stage(files, &sb.root, inputs).await?;
    let previews: Vec<String> = staged.iter().map(|p| preview(p, opt.preview_lines)).collect();
    let mut user = format!("Task: {task}\n\nInputs:\n{}", previews.join("\n\n"));
    let mut attempts = Vec::new();
    let mut tokens = 0;
    let mut code = String::new();
    let mut last: Option<RunResult> = None;
    for n in 0..=opt.max_repairs {
        let (txt, u) = llm::complete_with(app, opt.model, crate::llm::Tier::Heavy, SYSTEM, &user, false, 4000).await?;
        tokens += u.prompt + u.completion;
        code = extract_code(&txt);
        let r = sb.run(files, &code, &staged, opt.timeout).await?;
        let answer = last_json(&r.stdout);
        let error = if r.timed_out {
            Some(format!("timed out after {} s — process the data in one pass", opt.timeout.as_secs()))
        } else if r.exit != 0 {
            Some(format!("exit {}: {}", r.exit, r.stderr_tail))
        } else if answer.is_none() {
            Some(format!("the last stdout line was not a JSON object. stdout tail: {}", tail(&r.stdout, 400)))
        } else if answer.as_ref().is_some_and(is_empty) {
            // Silent parse failures look like success ({"items": []}); force one look at the data.
            Some(format!("the answer is empty ({}); the parsing probably missed the data format — check the input preview", tail(&r.stdout, 200)))
        } else {
            None
        };
        attempts.push(Attempt { n, prompt_tokens: u.prompt, completion_tokens: u.completion, exit: r.exit, error: error.clone(), run_ms: r.wall_ms });
        if let (None, Some(a)) = (&error, answer) {
            return Ok(Solution { task: task.into(), status: "done".into(), answer: a, outputs: r.outputs, out_dir: r.out_dir, code, attempts, tokens, wall_ms: t0.elapsed().as_millis() });
        }
        // Constant-size repair turn: task + previews + latest script + latest error only.
        user = format!(
            "Task: {task}\n\nInputs:\n{}\n\nYour previous script failed.\nError: {}\n\nScript:\n```python\n{code}\n```\nReturn the fixed full script.",
            previews.join("\n\n"),
            error.unwrap_or_default()
        );
        last = Some(r);
    }
    let r = last.expect("at least one attempt");
    Ok(Solution { task: task.into(), status: "failed".into(), answer: Value::Null, outputs: r.outputs, out_dir: r.out_dir, code, attempts, tokens, wall_ms: t0.elapsed().as_millis() })
}

/// Text inputs pass through; documents become markdown under artifacts/sandbox/staged.
async fn stage(files: &Files, root: &Path, inputs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    const TEXT: &[&str] = &["csv", "tsv", "json", "jsonl", "txt", "md", "xml", "html", "yaml", "yml", "log"];
    let mut out = Vec::new();
    for p in inputs {
        let real = files.check(p)?;
        let ext = real.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if TEXT.contains(&ext.as_str()) || real.is_dir() {
            out.push(real);
        } else {
            let md = crate::parse::to_markdown(&real).await?;
            let dir = root.join("staged");
            std::fs::create_dir_all(&dir)?;
            let stem = real.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "doc".into());
            let dst = dir.join(format!("{stem}.{ext}.md"));
            std::fs::write(&dst, md)?;
            out.push(dst);
        }
    }
    Ok(out)
}

/// `/in/items.csv (52 KB, 201 lines)` + the first lines, each capped.
fn preview(p: &Path, lines: usize) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if p.is_dir() {
        let n = std::fs::read_dir(p).map(|r| r.count()).unwrap_or(0);
        return format!("/in/{name}/ (directory, {n} entries)");
    }
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let count = text.lines().count();
    // Small files are shown whole: the model can read them directly instead of guessing.
    if text.chars().count() <= FULL_PREVIEW_CHARS {
        return format!("/in/{name} ({count} lines, shown in full):\n{text}");
    }
    let head: Vec<String> = text.lines().take(lines).map(|l| if l.chars().count() > 300 { format!("{}…", l.chars().take(300).collect::<String>()) } else { l.to_string() }).collect();
    format!("/in/{name} ({} KB, {count} lines). First lines:\n{}", size / 1024, head.join("\n"))
}

fn extract_code(txt: &str) -> String {
    let t = txt.trim();
    if let Some(start) = t.find("```") {
        let rest = &t[start + 3..];
        let rest = rest.strip_prefix("python").or_else(|| rest.strip_prefix("py")).unwrap_or(rest);
        if let Some(end) = rest.find("```") {
            return rest[..end].trim().to_string();
        }
        return rest.trim().to_string();
    }
    t.to_string()
}

fn last_json(stdout: &str) -> Option<Value> {
    stdout.lines().rev().map(str::trim).find(|l| !l.is_empty()).and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v.is_object() || v.is_array())
}

fn tail(s: &str, n: usize) -> String {
    let c = s.chars().count();
    s.chars().skip(c.saturating_sub(n)).collect()
}

/// True when every value is null / "" / [] / {} (recursively): a script that parsed nothing.
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.iter().all(is_empty),
        Value::Object(o) => o.values().all(is_empty),
        _ => false,
    }
}
