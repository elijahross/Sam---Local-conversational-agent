//! Task registry: every background request as one record the UI can show (menu-bar status,
//! drop-down notifications, the task overview). Live progress comes from the thinker through a
//! task-local channel (`report`), approvals and their execution attach to the task that drafted
//! them. The recent records are kept in memory and in `data/tasks/<id>.json` (last 100); every
//! record is also written to the long-term history (history.rs: searchable, never evicted).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::thinker::StepResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRec {
    pub tool: String,
    /// running | done | failed | input_required | unknown | cancelled
    pub status: String,
    pub summary: String,
    pub file: Option<String>,
    pub dir: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRec {
    pub id: String,
    pub tool: String,
    pub summary: String,
    /// pending | approved | cancelled | done | failed
    pub status: String,
}

/// A deterministic headline from the user's words: leading fillers off, first clause, capped.
/// Good enough while the task runs; a light model call replaces it with a better one.
pub fn headline(request: &str) -> String {
    let first = request.lines().next().unwrap_or(request).trim();
    let mut t = first.to_string();
    let leads = ["in the background,", "in the background", "im hintergrund,", "im hintergrund", "please", "bitte", "can you", "could you", "would you", "kannst du", "könntest du",
                 "hey sam,", "hey sam", "sam,", "ok sam,", "okay sam,", "and also,", "and also", "also,", "and now,", "and now", "now,", "hm,", "okay,", "ok,", "so,"];
    // Fillers stack ("in the background, please look up…"): strip until none is left.
    loop {
        let lower = t.to_lowercase();
        let Some(lead) = leads.iter().find(|l| lower.starts_with(*l)) else { break };
        t = t[lead.len()..].trim_start_matches([',', ' ']).to_string();
    }
    let t = t.trim().trim_end_matches(['.', '!', '?', ',']);
    let words: Vec<&str> = t.split_whitespace().collect();
    let mut h = words.iter().take(9).copied().collect::<Vec<_>>().join(" ");
    if words.len() > 9 { h.push('…'); }
    if h.is_empty() { return "Background task".into(); }
    let mut c = h.chars();
    match c.next() { Some(f) => f.to_uppercase().collect::<String>() + c.as_str(), None => h }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRec {
    pub id: String,
    pub session: String,
    pub kind: String,
    pub request: String,
    /// A short generated headline for lists and the overview (the report's title when there
    /// is one); the user's own words stay in `request`, shown quoted beneath it.
    #[serde(default)]
    pub title: String,
    /// What the window/selection/file was, if the request came with context (chip text).
    pub context: String,
    /// queued | running | needs_action | done | failed
    pub status: String,
    /// One line: what is happening right now / how it ended.
    pub current: String,
    pub plan: Vec<String>,
    pub steps: Vec<StepRec>,
    pub actions: Vec<ActionRec>,
    pub files: Vec<String>,
    pub notes: Vec<String>,
    pub result: String,
    pub tokens: u64,
    /// The presented result (present.rs): built after a research / list / document task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<serde_json::Value>,
    /// "building" while the report is being made.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub report_status: String,
    /// A typed result for the island card and the overview ("music", "weather", "reminder",
    /// "event"): one sentence was all a quick task showed before (2026-10-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<serde_json::Value>,
    pub created: i64,
    pub updated: i64,
}

/// Live events from a running thinker.
#[derive(Debug, Clone)]
pub enum Progress {
    Plan { say: String, tools: Vec<String> },
    StepStart { tool: String },
    Step(StepResult),
    /// The structured result of a step (see `TaskRec::view`).
    View(serde_json::Value),
}

tokio::task_local! {
    static SINK: tokio::sync::mpsc::UnboundedSender<Progress>;
}

/// Runs `fut` with progress reporting to `tx` (the thinker calls `report`).
pub async fn scoped<F: std::future::Future>(tx: tokio::sync::mpsc::UnboundedSender<Progress>, fut: F) -> F::Output {
    SINK.scope(tx, fut).await
}

/// Reports progress if the current task runs under `scoped` (no-op in the CLIs).
pub fn report(p: Progress) {
    let _ = SINK.try_with(|tx| tx.send(p));
}

fn now() -> i64 {
    chrono::Local::now().timestamp_millis()
}

#[derive(Clone)]
pub struct TaskLog {
    dir: PathBuf,
    recs: Arc<Mutex<Vec<TaskRec>>>,
    /// Tasks replaced by a corrected request: they finish quietly (no notice, no card).
    superseded: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Requests (session + normalised text) replaced by a restatement before their task existed:
    /// the earlier turn is still being judged when the whole sentence arrives, so its submission
    /// must be dropped when it comes.
    superseded_req: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Long-term, searchable copy of every record (None if the database can't be opened).
    pub history: Option<Arc<crate::history::History>>,
}

impl TaskLog {
    pub fn open(data: &std::path::Path) -> Self {
        let dir = data.join("tasks");
        let _ = std::fs::create_dir_all(&dir);
        let mut recs: Vec<TaskRec> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
            .collect();
        recs.sort_by_key(|r| r.created);
        // A task that was running or queued when the server stopped will never finish.
        for r in recs.iter_mut().filter(|r| r.status == "running" || r.status == "queued") {
            r.status = "failed".into();
            r.current = "interrupted (server restarted)".into();
        }
        let history = crate::history::History::open(data, &std::env::var("MIND_URL").unwrap_or_else(|_| "http://127.0.0.1:47802".into()), reqwest::Client::new())
            .map_err(|e| tracing::warn!("history unavailable: {e:#}"))
            .ok()
            .map(Arc::new);
        if let Some(h) = &history {
            let n = h.import_dir(&dir);
            if n > 0 {
                tracing::info!(n, "history: imported task records");
            }
            for r in recs.iter().filter(|r| r.current == "interrupted (server restarted)") {
                h.upsert(r);
            }
        }
        Self { dir, recs: Arc::new(Mutex::new(recs)), superseded: Default::default(), superseded_req: Default::default(), history }
    }

    fn save(&self, r: &TaskRec) {
        let _ = std::fs::write(self.dir.join(format!("{}.json", r.id)), serde_json::to_string_pretty(r).unwrap_or_default());
        if let Some(h) = &self.history {
            h.upsert(r);
        }
    }

    pub fn start(&self, session: &str, kind: &str, request: &str) -> String {
        // Unique even for a burst within one millisecond (ids keyed every update).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!("t{}{:03}", now(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1000);
        let (req, ctx) = split_context(request);
        let r = TaskRec {
            id: id.clone(),
            session: session.into(),
            kind: kind.into(),
            title: headline(&req),
            request: req,
            context: ctx,
            status: "running".into(),
            current: "planning".into(),
            plan: vec![],
            steps: vec![],
            actions: vec![],
            files: vec![],
            notes: vec![],
            result: String::new(),
            tokens: 0,
            report: None,
            report_status: String::new(),
            view: None,
            created: now(),
            updated: now(),
        };
        self.save(&r);
        let mut recs = self.recs.lock().unwrap();
        recs.push(r);
        // Keep the last 100 here (files too); the history keeps everything.
        while recs.len() > 100 {
            let old = recs.remove(0);
            let _ = std::fs::remove_file(self.dir.join(format!("{}.json", old.id)));
        }
        id
    }

    /// A request waiting for a free slot (governor): shown as "queued" until it starts.
    pub fn queue(&self, session: &str, kind: &str, request: &str) -> String {
        let id = self.start(session, kind, request);
        self.update(&id, |r| {
            r.status = "queued".into();
            r.current = "waiting for a free slot".into();
        });
        id
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut TaskRec)) {
        let mut recs = self.recs.lock().unwrap();
        if let Some(r) = recs.iter_mut().find(|r| r.id == id) {
            f(r);
            r.updated = now();
            // Status follows the actions: anything waiting = needs_action; an approved action
            // still executing = running (not done before the message is actually sent).
            if r.status != "failed" && r.actions.iter().any(|a| a.status == "pending") {
                r.status = "needs_action".into();
            } else if r.actions.iter().any(|a| a.status == "approved") {
                r.status = "running".into();
            } else if r.status == "needs_action" || (r.status == "running" && !r.actions.is_empty() && r.actions.iter().all(|a| a.status != "approved" && a.status != "pending")) {
                let failed = r.actions.iter().any(|a| a.status == "failed");
                r.status = if failed { "failed".into() } else { "done".into() };
            }
            self.save(r);
        }
    }

    pub fn apply(&self, id: &str, p: Progress) {
        let replaced = self.is_superseded(id);
        self.update(id, |r| match p {
            // A replaced task keeps running quietly; its label stays "replaced …".
            _ if replaced => {}
            Progress::Plan { say, tools } => {
                r.plan = tools;
                r.current = if say.is_empty() { "working".into() } else { say };
            }
            Progress::StepStart { tool } => {
                r.current = format!("running {tool}");
                r.steps.push(StepRec { tool, status: "running".into(), summary: String::new(), file: None, dir: None, at: now() });
            }
            Progress::View(v) => {
                r.view = Some(v);
            }
            Progress::Step(s) => {
                let status = if !s.status.is_empty() { s.status.clone() } else if s.ok { "done".into() } else { "failed".into() };
                let rec = StepRec { tool: s.tool.clone(), status, summary: s.summary.chars().take(2000).collect(), file: s.file.clone(), dir: s.dir.clone(), at: now() };
                match r.steps.iter_mut().rev().find(|x| x.tool == s.tool && x.status == "running") {
                    Some(x) => *x = rec,
                    None => r.steps.push(rec),
                }
                for f in [s.file, s.dir].into_iter().flatten() {
                    if !r.files.contains(&f) {
                        r.files.push(f);
                    }
                }
                r.tokens += s.tokens;
            }
        });
    }

    /// A hand-off line from the thinker ("Needs your OK … (action ab12c)", "please log in …").
    pub fn note(&self, id: &str, text: &str) {
        let action = text.split("(action ").nth(1).and_then(|x| x.split(')').next()).map(str::to_string);
        self.update(id, |r| {
            if let Some(aid) = action {
                let tool = crate::tools::Pending::new(&crate::thinker::data_dir()).get(&aid).map(|a| a.tool).unwrap_or_default();
                let summary = text.trim_start_matches("Needs your OK before I do it: ").split(". Say \"yes").next().unwrap_or(text).to_string();
                if !r.actions.iter().any(|a| a.id == aid) {
                    r.actions.push(ActionRec { id: aid, tool, summary, status: "pending".into() });
                }
                r.current = "waiting for your OK".into();
            } else {
                r.notes.push(text.to_string());
                r.current = text.chars().take(160).collect();
            }
        });
    }

    pub fn finish(&self, id: &str, status: &str, result: &str) {
        if self.is_superseded(id) {
            self.update(id, |r| r.result = result.chars().take(4000).collect());
            return;
        }
        self.update(id, |r| {
            r.result = result.chars().take(4000).collect();
            if !r.actions.iter().any(|a| a.status == "pending") {
                r.status = status.into();
                r.current = if status == "done" { "done".into() } else { "failed".into() };
            }
            for s in r.steps.iter_mut().filter(|s| s.status == "running") {
                s.status = "failed".into();
            }
        });
    }

    /// The task that drafted action `aid` (approvals attach to it).
    pub fn by_action(&self, aid: &str) -> Option<String> {
        self.recs.lock().unwrap().iter().rev().find(|r| r.actions.iter().any(|a| a.id == aid)).map(|r| r.id.clone())
    }

    pub fn set_action(&self, task: &str, aid: &str, status: &str) {
        self.update(task, |r| {
            if let Some(a) = r.actions.iter_mut().find(|a| a.id == aid) {
                a.status = status.into();
            }
            r.current = match status {
                "approved" => "doing it now (approved)".into(),
                "cancelled" => "cancelled an action".into(),
                s => s.to_string(),
            };
        });
    }

    /// Deletes a finished / failed / waiting task record — from the recent list and the history.
    /// A waiting task's pending actions are withdrawn with it (they can never be approved
    /// afterwards). Running and queued tasks stay (they'd keep writing to the record). Files the
    /// task produced are kept.
    pub fn delete(&self, id: &str) -> Result<(), &'static str> {
        let in_recent = self.remove_recent(id);
        let in_history = self.history.as_ref().is_some_and(|h| h.delete(id));
        match in_recent {
            Ok(()) => Ok(()),
            Err(e) if e == "no such task" && in_history => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Takes a record off the recent list only (the history keeps it).
    fn remove_recent(&self, id: &str) -> Result<(), &'static str> {
        let mut recs = self.recs.lock().unwrap();
        let i = recs.iter().position(|r| r.id == id).ok_or("no such task")?;
        if matches!(recs[i].status.as_str(), "running" | "queued") {
            return Err("the task is still running");
        }
        let r = recs.remove(i);
        let pending = crate::tools::Pending::new(&crate::thinker::data_dir());
        for a in r.actions.iter().filter(|a| a.status == "pending") {
            pending.take(&a.id);
        }
        let _ = std::fs::remove_file(self.dir.join(format!("{}.json", r.id)));
        Ok(())
    }

    /// The user corrected the request a live task was started with ("Gina" → "China"): that task
    /// is marked replaced, its drafted actions withdrawn, and its result won't be announced.
    /// Returns the replaced task's kind.
    pub fn supersede_matching(&self, session: &str, request: &str) -> Option<String> {
        // Compared without case and punctuation: the recogniser capitalises differently from one
        // turn to the next ("Pause Test." vs "pause test").
        let norm = |t: &str| t.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect::<String>().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        let want = norm(request);
        // The original request or an earlier clarified version of it ("… — clarified: …").
        let hits: Vec<(String, String, Vec<String>)> = {
            let recs = self.recs.lock().unwrap();
            recs.iter()
                .filter(|r| r.session == session && !want.is_empty() && norm(&r.request).starts_with(&want) && matches!(r.status.as_str(), "running" | "queued" | "needs_action"))
                .map(|r| (r.id.clone(), r.kind.clone(), r.actions.iter().filter(|a| a.status == "pending").map(|a| a.id.clone()).collect()))
                .collect()
        };
        let kind = hits.last()?.1.clone();
        let pending = crate::tools::Pending::new(&crate::thinker::data_dir());
        for (id, _, actions) in &hits {
            for a in actions {
                pending.take(a);
            }
            self.superseded.lock().unwrap().insert(id.clone());
            self.update(id, |r| {
                for a in r.actions.iter_mut().filter(|a| a.status == "pending") {
                    a.status = "cancelled".into();
                }
                // Not a failure: the user restated or corrected the request, and the new one runs.
                r.status = "cancelled".into();
                r.current = "replaced by a corrected request".into();
                r.result = "replaced: the request was restated".into();
            });
        }
        Some(kind)
    }

    fn req_key(session: &str, request: &str) -> String {
        let n = request.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect::<String>().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        format!("{session}\n{n}")
    }

    /// Mark a request as replaced before any task for it exists (the restated sentence won).
    pub fn supersede_request(&self, session: &str, request: &str) {
        self.superseded_req.lock().unwrap().insert(Self::req_key(session, request));
    }

    /// True when this exact request was replaced by a later restatement: don't start it.
    pub fn request_superseded(&self, session: &str, request: &str) -> bool {
        self.superseded_req.lock().unwrap().remove(&Self::req_key(session, request))
    }

    pub fn is_superseded(&self, id: &str) -> bool {
        self.superseded.lock().unwrap().contains(id)
    }

    /// Clears every finished and failed task off the recent list (the "Clear" button); they stay
    /// in the history. Returns how many.
    pub fn clear_finished(&self) -> usize {
        let ids: Vec<String> = self.recs.lock().unwrap().iter().filter(|r| r.status == "done" || r.status == "failed").map(|r| r.id.clone()).collect();
        ids.iter().filter(|id| self.remove_recent(id).is_ok()).count()
    }

    pub fn list(&self) -> Vec<TaskRec> {
        self.recs.lock().unwrap().iter().rev().cloned().collect()
    }

    /// The short list (menu-bar drop-down, overview "Recent"): everything still active plus what
    /// finished in the last 24 hours, newest first, at most 40.
    pub fn recent(&self) -> Vec<TaskRec> {
        let cutoff = now() - 24 * 3600 * 1000;
        self.list().into_iter().filter(|r| matches!(r.status.as_str(), "running" | "queued" | "needs_action") || r.updated >= cutoff).take(40).collect()
    }

    /// A record from the recent list, else from the history (older tasks opened from search).
    pub fn get(&self, id: &str) -> Option<TaskRec> {
        self.recs.lock().unwrap().iter().find(|r| r.id == id).cloned().or_else(|| self.history.as_ref().and_then(|h| h.get(id)))
    }
}

/// "do X\n\nContext — the window the user pointed at …: app Safari · window \"W\"\n…" →
/// ("do X", "Safari · W").
fn split_context(request: &str) -> (String, String) {
    match request.split_once("\n\nContext") {
        Some((req, ctx)) => {
            let label = ctx.split(": app ").nth(1).and_then(|x| x.lines().next()).unwrap_or("").replace('"', "").trim().to_string();
            (req.trim().to_string(), label)
        }
        None => (request.trim().to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn delete_rules() {
        let d = std::env::temp_dir().join(format!("tasklog-{}", std::process::id()));
        let log = super::TaskLog::open(&d);
        let a = log.start("s", "", "running task");
        assert!(log.delete(&a).is_err(), "running tasks stay");
        log.finish(&a, "failed", "boom");
        let b = log.start("s", "", "done task");
        log.finish(&b, "done", "ok");
        assert!(log.delete(&a).is_ok());
        assert!(log.get(&a).is_none() && !d.join("tasks").join(format!("{a}.json")).exists());
        let c = log.start("s", "", "still running");
        assert_eq!(log.clear_finished(), 1);
        // Cleared = off the recent list, still in the history; deleted = gone from both.
        assert!(log.recent().iter().all(|r| r.id != b) && log.recent().iter().any(|r| r.id == c));
        assert!(log.get(&b).is_some(), "cleared tasks stay findable");
        assert!(log.delete(&b).is_ok() && log.get(&b).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod status_tests {
    #[test]
    fn approved_action_is_running_until_it_finishes() {
        let d = std::env::temp_dir().join(format!("tasklog-st-{}", std::process::id()));
        let log = super::TaskLog::open(&d);
        let id = log.start("s", "", "send a message");
        log.update(&id, |r| r.actions.push(super::ActionRec { id: "ab12c".into(), tool: "chat_send".into(), summary: "msg".into(), status: "pending".into() }));
        log.finish(&id, "done", "drafted");
        assert_eq!(log.get(&id).unwrap().status, "needs_action");
        log.set_action(&id, "ab12c", "approved");
        assert_eq!(log.get(&id).unwrap().status, "running", "approved but not sent yet");
        log.set_action(&id, "ab12c", "done");
        assert_eq!(log.get(&id).unwrap().status, "done");
        let id2 = log.start("s", "", "send b");
        log.update(&id2, |r| r.actions.push(super::ActionRec { id: "cd34e".into(), tool: "chat_send".into(), summary: "msg".into(), status: "pending".into() }));
        log.finish(&id2, "done", "drafted");
        log.set_action(&id2, "cd34e", "approved");
        log.set_action(&id2, "cd34e", "failed");
        assert_eq!(log.get(&id2).unwrap().status, "failed");
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod headline_tests {
    #[test]
    fn fillers_off_and_capped() {
        assert_eq!(super::headline("in the background, please look up the weather in Vienna tomorrow"), "Look up the weather in Vienna tomorrow");
        assert_eq!(super::headline("Hey Sam, can you research the three best coffee houses in Vienna and then pick one for me?"), "Research the three best coffee houses in Vienna and…");
        assert_eq!(super::headline(""), "Background task");
    }
}

#[cfg(test)]
mod supersede_tests {
    #[test]
    fn corrections_replace_earlier_versions() {
        let d = std::env::temp_dir().join(format!("tasklog-sup-{}", std::process::id()));
        let log = super::TaskLog::open(&d);
        let a = log.start("s", "research", "trip to Gina");
        let b = log.start("s", "research", "trip to Gina — clarified: I meant Chino.");
        let other = log.start("s", "research", "something else");
        assert_eq!(log.supersede_matching("s", "trip to Gina").as_deref(), Some("research"));
        assert!(log.is_superseded(&a) && log.is_superseded(&b) && !log.is_superseded(&other));
        log.finish(&b, "done", "Chino results");
        assert_eq!(log.get(&b).unwrap().status, "cancelled");
        assert_eq!(log.get(&b).unwrap().current, "replaced by a corrected request");
        let _ = std::fs::remove_dir_all(&d);
    }
}
