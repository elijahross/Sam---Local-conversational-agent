//! Durable multi-step jobs (orchestration lab). A request too big for one plan (more
//! than 3 steps) becomes a job:
//! - the plan (outline: what Sam intends) and the execution state (every call and its result,
//!   the side effects done, waiting approvals) are separate and both persisted in
//!   data/jobs.db after EVERY call — a crash or restart resumes at the first open step;
//! - execution is a tool loop with ONE call per turn and the full (capped) history: lab winner
//!   (23/24 goals; plan-and-execute per step was 5× the tokens and repeated side effects,
//!   compacting old results made the model re-fetch);
//! - side-effect guard: a write with the same target (same email recipient + subject, same
//!   reminder, same file…) runs at most once per job, whatever the model asks;
//! - a drafted write that needs the user's OK pauses the job ("waiting"); the user's decision
//!   is recorded in the history and the job continues where it stopped;
//! - budgets (turns, tokens, runtime) end runaway jobs with a report of what's done / open.

use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::{AppState, llm, thinker::{Outcome, Plan, Step, StepResult}, tools::Registry};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Call {
    pub tool: String,
    pub args: Value,
    /// done | failed | input_required | skipped | approved | cancelled
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tokens: u64,
    pub max_minutes: i64,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_turns: crate::config::env_num("JOB_MAX_TURNS", 30), max_tokens: crate::config::env_num("JOB_MAX_TOKENS", 120_000), max_minutes: crate::config::env_num("JOB_MAX_MINUTES", 90) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub task: String,
    pub session: String,
    pub goal: String,
    /// The plan: what Sam intends (shown in the overview; guidance for every turn).
    pub outline: Vec<String>,
    /// running | waiting | done | failed
    pub state: String,
    /// The execution state: every call with its result.
    pub calls: Vec<Call>,
    /// Side effects done in this job (guard keys → their result).
    pub effects: serde_json::Map<String, Value>,
    pub answer: String,
    pub turns: u32,
    pub tokens: u64,
    pub budget: Budget,
    pub created: i64,
    pub updated: i64,
}

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

pub struct Store {
    db: Mutex<Connection>,
}

static STORE: std::sync::OnceLock<Option<std::sync::Arc<Store>>> = std::sync::OnceLock::new();

pub fn store() -> Option<std::sync::Arc<Store>> {
    STORE.get_or_init(|| Store::open(&crate::thinker::data_dir()).map_err(|e| tracing::warn!("jobs unavailable: {e:#}")).ok().map(std::sync::Arc::new)).clone()
}

impl Store {
    pub fn open(data: &std::path::Path) -> anyhow::Result<Self> {
        let db = Connection::open(data.join("jobs.db"))?;
        db.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, task TEXT NOT NULL, state TEXT NOT NULL, json TEXT NOT NULL, updated INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS jobs_task ON jobs(task);")?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// Write-ahead: called after every call and state change.
    pub fn save(&self, j: &Job) {
        let _ = self.db.lock().unwrap().execute("INSERT INTO jobs (id, task, state, json, updated) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO UPDATE SET state = ?3, json = ?4, updated = ?5",
            params![j.id, j.task, j.state, serde_json::to_string(j).unwrap_or_default(), j.updated]);
    }

    pub fn by_task(&self, task: &str) -> Option<Job> {
        let s: Option<String> = self.db.lock().unwrap().query_row("SELECT json FROM jobs WHERE task = ?1 ORDER BY updated DESC LIMIT 1", [task], |x| x.get(0)).optional().ok().flatten();
        s.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Jobs a stop interrupted (resumed at boot).
    pub fn interrupted(&self) -> Vec<Job> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT json FROM jobs WHERE state = 'running'") else { return vec![] };
        st.query_map([], |x| x.get::<_, String>(0)).map(|r| r.flatten().filter_map(|s| serde_json::from_str(&s).ok()).collect()).unwrap_or_default()
    }
}

/// Guard key for a write: the tool and its target, normalized. Two drafts of the same email
/// (same recipient + subject) or the same reminder are one side effect.
pub fn effect_key(tool: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or("").trim().to_lowercase();
    let target = match tool {
        "email_send" => format!("{}|{}", s("to"), s("subject")),
        "chat_send" => format!("{}|{}", s("chat"), s("site")),
        "reminder_create" | "note_save" | "journal_add" => s("title"),
        "calendar_create" => format!("{}|{}", s("title"), s("start")),
        "monitor_create" => format!("{}|{}|{}|{}", s("site"), s("query"), s("place"), s("url")),
        _ => {
            let mut v = args.clone();
            if let Some(o) = v.as_object_mut() {
                o.retain(|k, _| !["instructions", "body", "text", "note"].contains(&k.as_str()));
            }
            v.to_string().to_lowercase()
        }
    };
    format!("{tool}|{target}")
}

const ACT: &str = "You complete the user's goal in the background with tools, ONE call per reply. Use only facts from tool results. \
Side effects (writes) happen exactly once and only when the goal asks for them; tools marked [needs user confirmation] create a draft the user approves — \
the job pauses until then, never try to work around it. Prefer official sources over forums; retry a call that failed temporarily once; \
when something can't be found, say so. Reply ONLY with JSON {\"calls\": [{\"tool\": \"<name>\", \"args\": {...}}]} or \
{\"done\": true, \"answer\": \"for the user: what was done, the results, what remains open or uncertain\"}.";

fn cap(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn history(j: &Job) -> String {
    // Full history (lab: compaction made the model re-fetch); each result capped, and only very
    // long jobs fold their oldest calls into one line each.
    let n = j.calls.len();
    j.calls.iter().enumerate().map(|(i, c)| {
        let (a, r) = if n > 24 && i + 16 < n { (80, 160) } else { (300, 1200) };
        format!("{}. {}({}) → {}: {}{}", i + 1, c.tool, cap(&c.args.to_string(), a), c.status, cap(&c.summary, r), c.file.as_ref().map(|f| format!(" [file {f}]")).unwrap_or_default())
    }).collect::<Vec<_>>().join("\n")
}

/// A new job for a task whose plan outgrew one step list.
pub fn create(task: &str, session: &str, goal: &str, outline: Vec<String>) -> Job {
    Job { id: format!("j{}", now()), task: task.into(), session: session.into(), goal: goal.into(), outline, state: "running".into(), calls: vec![], effects: Default::default(),
          answer: String::new(), turns: 0, tokens: 0, budget: Budget::default(), created: now(), updated: now() }
}

/// Runs (or resumes) a job until it's done, waiting for the user, or out of budget.
pub async fn run(app: &AppState, mut j: Job, on_user: Option<UnboundedSender<String>>) -> anyhow::Result<Outcome> {
    let st = store().context("job store unavailable")?;
    let root = crate::config::resources_dir();
    let data = crate::thinker::data_dir();
    let reg = Registry::load(&root.join("tools"), &root.join("mcp.json"), &data.join("mcp")).await;
    let model = llm::Provider::parse(&crate::env_or("THINKER_MODEL", "")).unwrap_or(app.cfg.provider);
    // The tools for this goal (and every tool its outline names).
    let mut chosen: Vec<&crate::tools::ToolSpec> = reg.select(&j.goal, None, None, 10);
    for o in &j.outline {
        for t in reg.select(o, None, None, 2) {
            if !chosen.iter().any(|x| x.name == t.name) {
                chosen.push(t);
            }
        }
    }
    let tools = Registry::one_liners(&chosen);
    j.state = "running".into();
    st.save(&j);
    crate::tasklog::report(crate::tasklog::Progress::Plan { say: j.outline.join(" → "), tools: j.outline.iter().map(|o| o.split_whitespace().next().unwrap_or("").to_string()).collect() });
    let started = now();
    loop {
        let over = if j.turns >= j.budget.max_turns { Some("turn budget") } else if j.tokens >= j.budget.max_tokens { Some("token budget") } else if now() - started > j.budget.max_minutes * 60_000 { Some("time budget") } else { None };
        if let Some(why) = over {
            j.state = "failed".into();
            j.answer = format!("Stopped ({why} used up) after {} steps. Done so far:\n{}", j.calls.len(), j.calls.iter().filter(|c| c.status == "done").map(|c| format!("- {}: {}", c.tool, cap(&c.summary, 160))).collect::<Vec<_>>().join("\n"));
            break;
        }
        let nowt = chrono::Local::now();
        let user = format!("Now: {} ({}).\nTools:\n{tools}\n\nGoal: {}\nPlan: {}\n\nCalls so far:\n{}{}",
            nowt.format("%Y-%m-%d %H:%M"), nowt.format("%A"), j.goal, j.outline.iter().enumerate().map(|(i, s)| format!("{}. {s}", i + 1)).collect::<Vec<_>>().join(" "),
            if j.calls.is_empty() { "(none)".to_string() } else { history(&j) },
            if j.effects.is_empty() { String::new() } else { format!("\nSide effects already done (never repeat): {}", j.effects.keys().cloned().collect::<Vec<_>>().join("; ")) });
        let mut reply: Option<Value> = None;
        for attempt in 0..2 {
            let prompt = if attempt == 0 { user.clone() } else { format!("{user}\n\nYour last reply had no call and no final answer. Reply with {{\"calls\": [...]}} or {{\"done\": true, \"answer\": \"...\"}}.") };
            match llm::complete(app, model, ACT, &prompt, true, 4000).await {
                Ok((txt, u)) => {
                    j.tokens += u.prompt + u.completion;
                    let v: Value = serde_json::from_str(&txt).unwrap_or_default();
                    if v["done"] == true || v["calls"].as_array().is_some_and(|c| !c.is_empty()) {
                        reply = Some(v);
                        break;
                    }
                }
                Err(e) => tracing::info!(job = j.id, "job turn: {e:#}"),
            }
        }
        j.turns += 1;
        let Some(v) = reply else {
            j.state = "failed".into();
            j.answer = "The model gave no usable next step twice in a row.".into();
            break;
        };
        if v["done"] == true {
            j.state = "done".into();
            j.answer = v["answer"].as_str().unwrap_or("").to_string();
            break;
        }
        let c = &v["calls"][0];
        let (tool, args) = (c["tool"].as_str().unwrap_or("").to_string(), c["args"].clone());
        let spec = reg.get(&tool);
        let is_write = spec.is_some_and(|t| t.effects == "write");
        let key = effect_key(&tool, &args);
        if is_write && j.effects.contains_key(&key) {
            j.calls.push(Call { tool, args, status: "skipped".into(), summary: format!("already done earlier in this job: {}", j.effects[&key]), at: now(), ..Default::default() });
            j.updated = now();
            st.save(&j);
            continue;
        }
        // The same call failed twice already: don't hammer it (lab: the model retried an app
        // that didn't answer 7 times); it's reported as unavailable instead.
        let same_failed = j.calls.iter().filter(|c| c.tool == tool && c.args == args && c.status == "failed").count();
        if same_failed >= 2 {
            j.calls.push(Call { tool, args, status: "failed".into(), summary: "not retried: this exact call already failed twice — treat it as unavailable and say so in the answer".into(), at: now(), ..Default::default() });
            j.updated = now();
            st.save(&j);
            continue;
        }
        let out = crate::thinker::run_with(app, &j.goal, Some("job"), on_user.clone(), Some(Plan { steps: vec![Step { tool: tool.clone(), args: args.clone() }], say: String::new() })).await;
        let r: StepResult = match out {
            Ok(o) => {
                j.tokens += o.tokens;
                o.steps.into_iter().next().unwrap_or(StepResult { status: "failed".into(), tool: tool.clone(), ok: false, summary: "not run: the tool isn't available here, or the user didn't ask for this kind of action".into(), dir: None, file: None, tokens: 0 })
            }
            Err(e) => StepResult { status: "failed".into(), tool: tool.clone(), ok: false, summary: format!("{e:#}"), dir: None, file: None, tokens: 0 },
        };
        let status = if r.status.is_empty() { if r.ok { "done".to_string() } else { "failed".to_string() } } else { r.status.clone() };
        if is_write && (status == "done" || status == "input_required") {
            j.effects.insert(key, json!(cap(&r.summary, 120)));
        }
        j.calls.push(Call { tool, args, status: status.clone(), summary: r.summary.clone(), file: r.file.clone(), dir: r.dir.clone(), at: now() });
        j.updated = now();
        st.save(&j);
        if status == "input_required" {
            // The draft waits for the user; the job continues when they decide (on_decision).
            j.state = "waiting".into();
            j.answer = format!("Waiting for your OK: {}", cap(&r.summary, 200));
            break;
        }
    }
    j.updated = now();
    st.save(&j);
    Ok(outcome(&j))
}

/// The job as a thinker Outcome (task record, notices, cards, report work as for any request).
pub fn outcome(j: &Job) -> Outcome {
    // A finished job is done even if some calls failed on the way (it said what's missing).
    let steps: Vec<StepResult> = j.calls.iter().map(|c| StepResult { status: c.status.clone(), tool: c.tool.clone(), ok: j.state == "done" || c.status != "failed", summary: c.summary.clone(), dir: c.dir.clone(), file: c.file.clone(), tokens: 0 }).collect();
    Outcome {
        request: j.goal.clone(),
        plan: Plan { steps: j.calls.iter().map(|c| Step { tool: c.tool.clone(), args: c.args.clone() }).collect(), say: j.outline.join(" → ") },
        dir: j.calls.iter().rev().find_map(|c| c.dir.clone()),
        steps,
        tokens: j.tokens,
        summary: j.answer.clone(),
        status: match j.state.as_str() { "waiting" => "input_required", "done" => "done", _ => "failed" }.into(),
    }
}

/// The user approved / cancelled a draft of a waiting job: record it and mark it runnable.
pub fn on_decision(task: &str, decision: &str, summary: &str) -> Option<Job> {
    let st = store()?;
    let mut j = st.by_task(task).filter(|j| j.state == "waiting")?;
    if let Some(c) = j.calls.iter_mut().rev().find(|c| c.status == "input_required") {
        c.status = if decision == "approved" { "approved".into() } else { "cancelled".into() };
        c.summary = format!("{} — user {decision}: {}", c.summary, cap(summary, 300));
    }
    j.state = "running".into();
    j.updated = now();
    st.save(&j);
    Some(j)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_keys() {
        let a = effect_key("email_send", &json!({"to": "Boss@AlpinePack.at", "subject": "Recommendation ", "instructions": "x"}));
        let b = effect_key("email_send", &json!({"to": "boss@alpinepack.at", "subject": "recommendation", "instructions": "a different body"}));
        assert_eq!(a, b, "same recipient + subject = one email, whatever the body");
        assert_ne!(a, effect_key("email_send", &json!({"to": "markus@alpinepack.at", "subject": "recommendation"})));
        assert_eq!(effect_key("reminder_create", &json!({"title": "Contract v2", "due": "x"})), effect_key("reminder_create", &json!({"title": "contract v2", "due": "y"})));
    }

    #[test]
    fn persist_resume_and_decision() {
        let d = std::env::temp_dir().join(format!("jobs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let _ = std::fs::remove_file(d.join("jobs.db"));
        let s = Store::open(&d).unwrap();
        let mut j = create("t1", "s", "research and email", vec!["research".into(), "email".into()]);
        j.calls.push(Call { tool: "research".into(), status: "done".into(), summary: "3 suppliers".into(), at: 1, ..Default::default() });
        s.save(&j);
        assert_eq!(s.interrupted().len(), 1, "a running job is resumed at boot");
        assert_eq!(s.interrupted()[0].calls.len(), 1, "with its execution state");
        j.calls.push(Call { tool: "email_send".into(), status: "input_required".into(), summary: "draft to boss".into(), at: 2, ..Default::default() });
        j.state = "waiting".into();
        s.save(&j);
        assert!(s.interrupted().is_empty(), "a waiting job waits for the user, not for a restart");
        let o = outcome(&j);
        assert_eq!(o.status, "input_required");
    }
}
