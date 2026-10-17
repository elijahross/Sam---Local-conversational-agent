//! Background agent work that must never block the conversation.
//!
//! Memory consolidation + the "thinking" agent (`thinker.rs`: research, scrape, browser,
//! sandboxed code, files):
//! - jobs are submitted from the voice loop and return immediately;
//! - each job gets a `tasks` row (status, one-line summary) and, if it produces anything
//!   bigger than a sentence, an artifact directory `data/artifacts/<task id>/` — files,
//!   not DB rows, so the memory store isn't polluted (Bops stores results the same way);
//! - finished jobs are announced through `notices`: the session drains them into the next
//!   turn's prompt ("work status injection", vault pattern from Bops calls), so Sam
//!   mentions the result naturally instead of interrupting.
//!
//! Memory writes are serialized (one worker), like Bops' per-bank write queue.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use tokio::sync::mpsc;

use crate::{
    AppState,
    memory::{JsonLlm, Memory},
};

#[derive(Debug, Clone)]
pub enum Job {
    /// Extract facts + a summary from a session's not-yet-consolidated messages.
    Consolidate { session: String },
    /// A request Laya routed to background work (kind: email | research | browser | documents).
    /// Runs on the thinking agent; at most one at a time (one Chrome screen, RAM budget).
    Request { session: String, kind: String, text: String },
    /// Continue a durable job (after a restart, or after the user decided on its draft).
    Resume { session: String, task: String },
}

/// Background services (automations, monitors, lead campaigns, knowledge indexing, job resume)
/// run in the server only: lab / CLI binaries build an AppState too and must not start a second
/// copy of the schedulers (double firings, double LinkedIn sessions).
static BACKGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called by the server before building its AppState.
pub fn enable_background() {
    BACKGROUND.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn background() -> bool {
    BACKGROUND.load(std::sync::atomic::Ordering::SeqCst)
}

/// The queue's sender, for code that resumes jobs from outside the dispatcher (approvals).
static QUEUE: std::sync::OnceLock<mpsc::UnboundedSender<Job>> = std::sync::OnceLock::new();

pub fn resume(session: &str, task: &str) {
    if let Some(q) = QUEUE.get() {
        let _ = q.send(Job::Resume { session: session.into(), task: task.into() });
    }
}

/// A request: the thinker's plan, or — when the plan outgrows one step list — a durable job.
async fn run_request(app: &AppState, task: &str, session: &str, text: &str, kind: &str, tx: tokio::sync::mpsc::UnboundedSender<String>, resume_job: Option<crate::jobs::Job>) -> anyhow::Result<crate::thinker::Outcome> {
    if let Some(j) = resume_job {
        return crate::jobs::run(app, j, Some(tx)).await;
    }
    let o = crate::thinker::run(app, text, Some(kind).filter(|k| !k.is_empty()), Some(tx.clone())).await?;
    if o.status != "job" {
        return Ok(o);
    }
    let outline: Vec<String> = o.plan.steps.iter().map(|s| format!("{} {}", s.tool, s.args.to_string().chars().take(80).collect::<String>())).collect();
    let mut j = crate::jobs::create(task, session, text, outline);
    j.tokens = o.tokens;
    crate::jobs::run(app, j, Some(tx)).await
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub session: String,
    pub text: String,
}

pub struct Tasks {
    tx: mpsc::UnboundedSender<Job>,
    notices: Arc<Mutex<Vec<Notice>>>,
    running: Running,
    /// Island cards (result / confirm / error) per session, drained by the session.
    cards: Cards,
    /// Every request as a record for the menu-bar status and the task overview.
    pub log: crate::tasklog::TaskLog,
    /// Open voice sessions, oldest first. The island drops its session a minute after it
    /// closes, so a request often finishes after its session is gone: its result then belongs
    /// to the newest open session (the app reconnects to have it spoken).
    live: Arc<Mutex<Vec<String>>>,
}

/// Sessions of the voice island ("s20261010-204939"), not background producers
/// ("automation", "monitor", "leads"), whose results are never handed to a conversation.
fn is_voice(session: &str) -> bool {
    session.starts_with('s') && session[1..].starts_with(|c: char| c.is_ascii_digit())
}

fn owns_in(live: &[String], session: &str, owner: &str) -> bool {
    // "friend": Sam's own check-in (friend.rs), spoken by the newest open conversation.
    owner == session || ((owner == "friend" || (is_voice(owner) && !live.iter().any(|s| s == owner))) && live.last().is_some_and(|s| s == session))
}

#[cfg(test)]
mod handover_tests {
    #[test]
    fn closed_session_results_go_to_newest_open_session() {
        let live = vec!["s20261010-210000".to_string(), "s20261010-211500".to_string()];
        // The request's session closed before it finished: the newest open session reports it.
        assert!(super::owns_in(&live, "s20261010-211500", "s20261010-204939"));
        assert!(!super::owns_in(&live, "s20261010-210000", "s20261010-204939"));
        // A still-open session keeps its own results.
        assert!(!super::owns_in(&live, "s20261010-211500", "s20261010-210000"));
        assert!(super::owns_in(&live, "s20261010-210000", "s20261010-210000"));
        // Background producers are never spoken in a conversation.
        assert!(!super::owns_in(&live, "s20261010-211500", "automation"));
        assert!(!super::owns_in(&live, "s20261010-211500", "monitor"));
    }
}

type Cards = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// (session, kind, request, started) of background requests not finished yet.
type Running = Arc<Mutex<Vec<(String, String, String, std::time::Instant)>>>;

impl Tasks {
    pub fn spawn(app: Arc<AppState>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let _ = QUEUE.set(tx.clone());
        let _ = APP.set(app.clone());
        let notices: Arc<Mutex<Vec<Notice>>> = Default::default();
        let n2 = notices.clone();
        let running: Running = Default::default();
        let r2 = running.clone();
        let cards: Cards = Default::default();
        let c2 = cards.clone();
        // Governor: at most MAX_PARALLEL requests at once, a bounded queue behind them, and
        // memory-aware admission (governor.rs).
        let heavy = Arc::new(Semaphore::new(crate::governor::max_parallel()));
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = crate::tasklog::TaskLog::open(&crate::thinker::data_dir());
        // History embeddings in the background (the mind sidecar may start later than us).
        if let Some(h) = log.history.clone() {
            tokio::spawn(async move {
                loop {
                    match h.embed_pending().await {
                        Ok(n) if n > 0 => continue,
                        Ok(_) => tokio::time::sleep(std::time::Duration::from_secs(20)).await,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
                    }
                }
            });
        }
        // Jobs a stop interrupted continue where they were (their execution state is on disk).
        if let Some(js) = crate::jobs::store().filter(|_| background()) {
            for j in js.interrupted() {
                tracing::info!(job = j.id, calls = j.calls.len(), "resuming interrupted job");
                let _ = tx.send(Job::Resume { session: j.session.clone(), task: j.task.clone() });
            }
        }
        // Cross-source knowledge index (knowledge.rs): incremental, in the background.
        if background() {
            crate::knowledge::spawn(app.clone());
            // Storage janitor (storage.rs): the user's retention settings, applied on schedule.
            crate::storage::spawn(app.clone());
        }
        // Automations (automations.rs): a firing runs its request as a background task.
        if background() {
            let qa = tx.clone();
            crate::automations::spawn(app.clone(), move |_name, text| {
                let _ = qa.send(Job::Request { session: "automation".into(), kind: "automation".into(), text });
            });
        }
        // Long-running lead lists (leads.rs): paced LinkedIn slices over days / weeks.
        if background() {
            crate::leads::spawn(app.clone(), log.clone());
        }
        // Saved-search monitors (monitors.rs): their alerts become task records.
        if background() {
            crate::monitors::spawn(app.clone(), log.clone());
        }
        // The friend's daily check-in (friend.rs): one small thing, at a quiet moment.
        if background() {
            crate::friend::spawn(app.clone());
        }
        let l2 = log.clone();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                match job {
                    // Requests run beside the memory worker so a 2-minute scrape never delays
                    // consolidation; the semaphore keeps them one at a time.
                    Job::Request { session, kind, text } => {
                        let (app, n2, heavy, r2, c2, l3, active) = (app.clone(), n2.clone(), heavy.clone(), r2.clone(), c2.clone(), l2.clone(), active.clone());
                        // Approvals / cancellations of drafts are tiny and never wait in the queue.
                        let decision = kind == "confirm_click" || ((crate::tools::is_confirmation(&text) || crate::tools::is_cancellation(&text)) && text.split_whitespace().count() <= 12);
                        let waiting = r2.lock().unwrap().len();
                        if !decision && waiting >= crate::governor::max_parallel() + crate::governor::max_queue() {
                            let msg = format!("Sam is busy ({waiting} tasks running or queued); ask again in a bit");
                            tracing::warn!(waiting, "request refused: queue full");
                            n2.lock().unwrap().push(Notice { session: session.clone(), text: msg.clone() });
                            c2.lock().unwrap().push((session.clone(), serde_json::json!({"type": "card", "kind": "error", "id": format!("b{}", chrono::Local::now().timestamp_millis()),
                                "blocks": [{"header": {"icon": "hourglass", "title": "Busy", "detail": "queue full"}}, {"text": msg}]})));
                            continue;
                        }
                        r2.lock().unwrap().push((session.clone(), kind.clone(), text.clone(), std::time::Instant::now()));
                        // Visible as "queued" right away (overview, menu bar) until a slot is free.
                        let queued = (!decision).then(|| l3.queue(&session, &kind, &text));
                        tokio::spawn(async move {
                            let _permit = if decision { None } else { heavy.acquire().await.ok() };
                            if let Some(id) = &queued {
                                let need = crate::governor::need_mb(&kind, &text);
                                let a2 = active.clone();
                                let wait = crate::governor::wait_for_memory(need, move || a2.load(std::sync::atomic::Ordering::Relaxed) == 0, |avail| {
                                    l3.update(id, |r| r.current = format!("waiting for memory ({avail} MB free, {need} MB needed)"));
                                }).await;
                                if let Err(e) = wait {
                                    l3.finish(id, "failed", &e);
                                    n2.lock().unwrap().push(Notice { session: session.clone(), text: e });
                                    r2.lock().unwrap().retain(|r| !(r.0 == session && r.2 == text));
                                    return;
                                }
                            }
                            active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            // A hung task frees its slot (and its browser handles) after the timeout.
                            let run = request(&app, &n2, &c2, &l3, session.clone(), kind, text.clone(), queued.clone());
                            if tokio::time::timeout(crate::governor::task_timeout(), run).await.is_err() {
                                if let Some(id) = &queued {
                                    l3.finish(id, "failed", "stopped: took longer than the task time limit");
                                }
                                n2.lock().unwrap().push(Notice { session: session.clone(), text: "A background task took too long and was stopped.".into() });
                            }
                            active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                            r2.lock().unwrap().retain(|r| !(r.0 == session && r.2 == text));
                        });
                    }
                    Job::Resume { session, task } => {
                        let Some(j) = crate::jobs::store().and_then(|s| s.by_task(&task)) else { continue };
                        let (app, n2, heavy, c2, l3) = (app.clone(), n2.clone(), heavy.clone(), c2.clone(), l2.clone());
                        tokio::spawn(async move {
                            let _permit = heavy.acquire().await.ok();
                            let budget = std::time::Duration::from_secs((j.budget.max_minutes.max(5) * 60) as u64);
                            let goal = j.goal.clone();
                            let run = request_or_resume(&app, &n2, &c2, &l3, session, "job".into(), goal, None, Some(j));
                            let _ = tokio::time::timeout(budget, run).await;
                        });
                    }
                    job => {
                        let Some(memory) = app.memory.get() else { continue };
                        run(&app, memory, job).await;
                    }
                }
            }
        });
        Self { tx, notices, running, cards, log, live: Default::default() }
    }

    pub fn submit(&self, job: Job) {
        if let Job::Request { session, kind, text } = &job {
            tracing::info!(session, kind, request = %text.chars().take(80).collect::<String>(), "task submitted");
        }
        let _ = self.tx.send(job);
    }

    pub fn attach(&self, session: &str) {
        self.live.lock().unwrap().push(session.into());
    }

    pub fn detach(&self, session: &str) {
        self.live.lock().unwrap().retain(|s| s != session);
    }

    /// Work of `owner` is reported in `session`: its own, or a closed voice session's when
    /// `session` is the newest open one.
    pub fn owns(&self, session: &str, owner: &str) -> bool {
        if owner == session {
            return true;
        }
        owns_in(&self.live.lock().unwrap(), session, owner)
    }

    /// Finished work to mention in the session's next turn (drained once).
    /// One finished result per turn (the oldest): each is spoken on its own and gets its own
    /// window for the user's feedback; the rest wait their turn (no mix-up of two results).
    pub fn take_notices(&self, session: &str) -> Vec<Notice> {
        let mut n = self.notices.lock().unwrap();
        match n.iter().position(|x| self.owns(session, &x.session)) {
            Some(i) => vec![n.remove(i)],
            None => vec![],
        }
    }

    pub fn has_notices(&self, session: &str) -> bool {
        self.notices.lock().unwrap().iter().any(|n| self.owns(session, &n.session))
    }

    pub fn take_cards(&self, session: &str) -> Vec<serde_json::Value> {
        let mut c = self.cards.lock().unwrap();
        let (mine, rest): (Vec<_>, Vec<_>) = c.drain(..).partition(|x| self.owns(session, &x.0));
        *c = rest;
        mine.into_iter().map(|x| x.1).collect()
    }

    /// One line per unfinished background request of this session (for the prompt).
    pub fn running(&self, session: &str) -> Vec<String> {
        self.running.lock().unwrap().iter().filter(|r| self.owns(session, &r.0)).map(|r| format!("- {} task \"{}\" (running for {} s)", r.1, short(&r.2), r.3.elapsed().as_secs())).collect()
    }

    pub fn notify(&self, notice: Notice) {
        self.notices.lock().unwrap().push(notice);
    }

    pub fn push_card(&self, session: &str, card: serde_json::Value) {
        self.cards.lock().unwrap().push((session.to_string(), card));
    }
}

async fn run(app: &AppState, memory: &Memory, job: Job) {
    match job {
        Job::Request { .. } | Job::Resume { .. } => unreachable!("requests and jobs run in `request`"),
        Job::Consolidate { session } => {
            let id = memory.task_start("consolidate", &session);
            let llm = JsonLlm::new(app);
            match memory.consolidate(&llm, app.judge.as_ref(), &session).await {
                Ok(summary) => {
                    tracing::info!(session, summary, "memory consolidated");
                    memory.task_finish(id, "done", &summary, None);
                    if memory.take_card_dirty() {
                        match memory.refresh_card(&llm).await {
                            Ok(n) => tracing::info!(lines = n, "user card refreshed"),
                            Err(e) => tracing::warn!("user card refresh failed: {e:#}"),
                        }
                    } else {
                        tracing::info!("user card unchanged (no new or superseded facts): refresh skipped");
                    }
                    // Dream gate (vault: "Debounced Memory Consolidation Trigger"): enough new
                    // facts AND a cooldown since the last dream; one worker = never concurrent.
                    let (new_facts, last) = memory.facts_since_dream();
                    let cooldown = crate::env_num("DREAM_COOLDOWN_H", 8i64) * 3600;
                    if new_facts >= crate::env_num("DREAM_MIN_FACTS", 20i64) && chrono::Local::now().timestamp() - last >= cooldown {
                        let id = memory.task_start("dream", &format!("{new_facts} new facts"));
                        match memory.dream(&llm).await {
                            Ok(s) => memory.task_finish(id, "done", &s, None),
                            Err(e) => memory.task_finish(id, "failed", &format!("{e:#}"), None),
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(session, "memory consolidation failed: {e:#}");
                    memory.task_finish(id, "failed", &format!("{e:#}"), None);
                }
            }
        }
    }
}

/// One background request: thinking agent + task row + notices (hand-offs and the result).
async fn request(app: &AppState, notices: &Arc<Mutex<Vec<Notice>>>, cards: &Cards, log: &crate::tasklog::TaskLog, session: String, kind: String, text: String, queued: Option<String>) {
    request_or_resume(app, notices, cards, log, session, kind, text, queued, None).await
}

#[allow(clippy::too_many_arguments)]
static APP: std::sync::OnceLock<Arc<AppState>> = std::sync::OnceLock::new();

/// A short headline for the overview from the user's words, off the hot path (light tier).
/// The deterministic one (tasklog::headline) stands until this arrives; a report's title wins.
fn spawn_title(log: crate::tasklog::TaskLog, task: String, request: String) {
    let Some(app) = APP.get().cloned() else { return };
    tokio::spawn(async move {
        let words = request.split("\n\nContext").next().unwrap_or(&request).chars().take(600).collect::<String>();
        let sys = "Give this spoken request a title for a task list: 3 to 8 words, the topic and the action, no quotes, no trailing period, same language as the request. Reply with the title only.";
        if let Ok((t, _)) = crate::llm::complete_tier(&app, crate::llm::Tier::Light, sys, &words, false, 40).await {
            let t = t.trim().trim_matches('"').trim_end_matches('.').to_string();
            if (3..=90).contains(&t.chars().count()) && !t.contains('\n') {
                log.update(&task, |r| if r.report.is_none() { r.title = t.clone() });
            }
        }
    });
}

async fn request_or_resume(app: &AppState, notices: &Arc<Mutex<Vec<Notice>>>, cards: &Cards, log: &crate::tasklog::TaskLog, session: String, kind: String, text: String, queued: Option<String>, resume_job: Option<crate::jobs::Job>) {
    // Replaced while it waited in the queue (the user finished the sentence after a pause and the
    // merged request was submitted): there was no task record yet to cancel, so skip it here
    // (2026-10-16: with whole-reply speech the first half started after the merged one).
    if resume_job.is_none() {
        // Its queued record was cancelled while it waited for a slot (supersede_matching): don't
        // start it and don't flip that record back to "running".
        if queued.as_deref().is_some_and(|id| log.is_superseded(id)) {
            tracing::info!(request = %text.chars().take(80).collect::<String>(), "not started: replaced by the user's whole sentence while it waited");
            return;
        }
        let own = text.split("\n\nContext").next().unwrap_or(&text);
        if queued.is_none() && log.request_superseded(&session, own) {
            tracing::info!(request = %own.chars().take(80).collect::<String>(), "not started: replaced by the user's whole sentence before it was queued");
            return;
        }
    }
    let id = app.memory.get().map(|m| m.task_start(&format!("request:{kind}"), &text));
    // An approval / cancellation of a drafted action belongs to the task that drafted it.
    let action = regex::Regex::new(r"\b([0-9a-f]{4,6})\b").unwrap().captures_iter(&text).filter_map(|c| log.by_action(&c[1]).map(|t| (t, c[1].to_string()))).next();
    let decision = if crate::tools::is_cancellation(&text) { "cancelled" } else if crate::tools::is_confirmation(&text) { "approved" } else { "" };
    let task = match (&action, decision) {
        _ if resume_job.is_some() => {
            let t = resume_job.as_ref().map(|j| j.task.clone()).unwrap_or_default();
            log.update(&t, |r| {
                r.status = "running".into();
                r.current = "continuing the job".into();
            });
            t
        }
        (Some((t, aid)), d) if !d.is_empty() => {
            log.set_action(t, aid, d);
            t.clone()
        }
        _ => match queued {
            Some(id) => {
                log.update(&id, |r| {
                    r.status = "running".into();
                    r.current = "planning".into();
                });
                id
            }
            None => {
                let t = log.start(&session, &kind, &text);
                if kind != "confirm_click" && !matches!(session.as_str(), "automation" | "monitor" | "leads") {
                    spawn_title(log.clone(), t.clone(), text.clone());
                }
                t
            }
        },
    };
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<crate::tasklog::Progress>();
    let (l4, t4) = (log.clone(), task.clone());
    let prog = tokio::spawn(async move {
        while let Some(p) = prx.recv().await {
            l4.apply(&t4, p);
        }
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (n3, s3, c3, l5, t5) = (notices.clone(), session.clone(), cards.clone(), log.clone(), task.clone());
    // Hand-off requests (log in, solve a check, approve an action) reach the next turn while the
    // job waits; drafted actions also become a confirmation card in the island.
    let fwd = tokio::spawn(async move {
        while let Some(t) = rx.recv().await {
            if let Some(card) = confirm_card(&t) {
                c3.lock().unwrap().push((s3.clone(), card));
            }
            l5.note(&t5, &t);
            n3.lock().unwrap().push(Notice { session: s3.clone(), text: t });
        }
    });
    let resumed = resume_job.is_some();
    let res = crate::tasklog::scoped(ptx, run_request(app, &task, &session, &text, &kind, tx, resume_job)).await;
    let _ = fwd.await;
    let _ = prog.await;
    let (status, summary, dir) = match &res {
        Ok(o) => (if o.status == "done" || (o.status == "input_required" && o.steps.iter().all(|s| s.ok)) { "done" } else { "failed" }, o.summary.clone(), o.dir.clone()),
        Err(e) => ("failed", format!("{e:#}"), None),
    };
    tracing::info!(kind, status, summary, "background request finished");
    if status == "done" {
        crate::knowledge::ingest_soon();
    }
    if let Some((t, aid)) = action.as_ref().filter(|_| !decision.is_empty() && !resumed) {
        // The approved action ran (or was cancelled): record its outcome on the drafting task.
        let st = if decision == "cancelled" { "cancelled" } else if status == "done" { "done" } else { "failed" };
        log.set_action(t, aid, st);
        // A job waiting for this decision continues where it stopped.
        if crate::jobs::on_decision(t, if st == "cancelled" { "cancelled" } else { "approved" }, &summary).is_some() {
            resume(&session, t);
            return;
        }
        log.finish(t, if st == "failed" { "failed" } else { "done" }, &summary);
    } else if res.as_ref().is_ok_and(|o| o.status == "input_required") && crate::jobs::store().and_then(|s| s.by_task(&task)).is_some_and(|j| j.state == "waiting") {
        // A job paused for the user's OK: the task waits too (its draft card is out).
        log.update(&task, |r| {
            r.status = "needs_action".into();
            r.current = summary.chars().take(200).collect();
        });
    } else {
        log.finish(&task, status, &summary);
    }
    if let (Some(m), Some(id)) = (app.memory.get(), id) {
        m.task_finish(id, status, &summary, dir.as_deref().map(std::path::Path::new));
    }
    // Replaced by a corrected request: finish quietly (the corrected task reports instead).
    if log.is_superseded(&task) {
        return;
    }
    let request_text = text.clone();
    let text = match status {
        "done" => format!("Background {kind} task finished (\"{}\"): {}{}", short(&text), findings(res.as_ref().ok(), &summary), dir.map(|d| format!(" Files: {d}")).unwrap_or_default()),
        _ => format!("Background {kind} task failed (\"{}\"): {summary}", short(&text)),
    };
    let report = status == "done" && res.as_ref().is_ok_and(|o| crate::present::wanted(&o.steps));
    let text = if report { format!("{text} (A full report is being put together in the task overview.)") } else { text };
    // The notice goes first: a result card makes the session speak its notice right away.
    notices.lock().unwrap().push(Notice { session: session.clone(), text });
    if let Ok(o) = &res {
        if o.status != "input_required" && !o.steps.is_empty() {
            let view = log.get(&task).and_then(|r| r.view);
            cards.lock().unwrap().push((session.clone(), result_card(o, &kind, &task, view)));
        }
    } else if let Err(e) = &res {
        cards.lock().unwrap().push((session.clone(), serde_json::json!({"type": "card", "kind": "error", "id": format!("e{}", chrono::Local::now().timestamp_millis()),
            "blocks": [{"header": {"icon": "exclamationmark.triangle", "title": "Couldn't finish that", "detail": kind}}, {"text": format!("{e:#}").chars().take(300).collect::<String>()}]})));
    }    // Research / lists / documents get a presented report for the overview (after the notice
    // and card went out, so it never delays them).
    if let Ok(o) = &res {
        if status == "done" && crate::present::wanted(&o.steps) {
            log.update(&task, |r| r.report_status = "building".into());
            let model = crate::llm::Provider::parse(&crate::env_or("THINKER_MODEL", "")).unwrap_or(app.cfg.provider);
            match crate::present::build(app, &task, &request_text, &o.steps, model).await {
                Ok(r) => log.update(&task, |x| {
                    if let Some(t) = r["title"].as_str().filter(|t| !t.trim().is_empty()) {
                        x.title = t.trim().to_string();
                    }
                    x.report = Some(r);
                    x.report_status = String::new();
                }),
                Err(e) => {
                    tracing::warn!("report for {task}: {e:#}");
                    log.update(&task, |x| x.report_status = String::new());
                }
            }
        }
    }
}


/// What a finished request found: every step's summary when it had several (a Reddit + Meetup
/// validation is reported as both, not as the last search only).
fn findings(o: Option<&crate::thinker::Outcome>, summary: &str) -> String {
    match o {
        Some(o) if o.steps.len() > 1 => o.steps.iter().filter(|s| s.ok).map(|s| s.summary.chars().take(700).collect::<String>()).collect::<Vec<_>>().join(" | "),
        _ => summary.to_string(),
    }
}

/// The island confirmation card for a pending action (also served with a task's details).
pub fn action_card(id: &str) -> Option<serde_json::Value> {
    confirm_card(&format!("(action {id})"))
}

/// "Needs your OK … (action <id>)." → a confirmation card built from the pending action.
fn confirm_card(notice: &str) -> Option<serde_json::Value> {
    let id = notice.split("(action ").nth(1)?.split(')').next()?.to_string();
    let a = crate::tools::Pending::new(&crate::thinker::data_dir()).get(&id)?;
    let editable = ["to", "cc", "subject", "body", "title", "text", "path", "start", "end", "due", "location"];
    let fields: Vec<serde_json::Value> = a.args.as_object().into_iter().flatten()
        .filter(|(k, _)| editable.contains(&k.as_str()))
        .map(|(k, v)| {
            let val = match v { serde_json::Value::Array(xs) => xs.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "), serde_json::Value::String(s) => s.clone(), other => other.to_string() };
            // Exact timestamps are shown readably; editing them keeps RFC 3339.
            let display = chrono::DateTime::parse_from_rfc3339(&val).map(|t| t.with_timezone(&chrono::Local).format("%a %-d %b, %H:%M").to_string()).ok();
            let lbl = if a.tool == "chat_send" && k == "text" { "Message".to_string() } else { label(k) };
            serde_json::json!({"key": k, "label": lbl, "value": val, "display": display, "multiline": k == "body" || k == "text", "array": v.is_array()})
        })
        .collect();
    let (icon, title) = match a.tool.as_str() {
        "email_send" => ("envelope", "Send this email?"),
        "calendar_create" => ("calendar.badge.plus", "Add this event?"),
        "reminder_create" => ("checklist", "Add this reminder?"),
        "chat_send" => ("bubble.left.and.text.bubble.right", "Send this message?"),
        "web_task" => {
            let l = a.args["info"]["label"].as_str().unwrap_or("").to_lowercase();
            if ["order", "buy", "pay", "purchase", "bestell", "kauf", "bezahl"].iter().any(|w| l.contains(w)) { ("cart", "Place this order?") }
            else if ["post", "comment", "publish", "veröffentlich", "kommentar"].iter().any(|w| l.contains(w)) { ("text.bubble", "Post this?") }
            else { ("checkmark.rectangle", "Submit this form?") }
        }
        t if t.contains('.') => ("puzzlepiece.extension", "Run this action?"),
        _ => ("checkmark.circle", "Do this?"),
    };
    // Calendar conflicts as a small warning on the card (VoiceOS: "nothing hidden at approval time").
    let conflicts: Vec<String> = a.args["conflicts"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect();
    let mut blocks = vec![serde_json::json!({"header": {"icon": icon, "title": title, "detail": a.tool}})];
    if !conflicts.is_empty() {
        blocks.push(serde_json::json!({"warning": format!("Overlaps with {}", conflicts.join(", "))}));
    }
    // Chat: the verified recipient (read from the chat header in Sam's browser) is fixed, and
    // the message being answered is shown, so the user sees exactly where this goes.
    // Website task: exactly what the final click commits (page, button, filled values, total).
    if a.tool == "web_task" {
        let i = &a.args["info"];
        if let Some(t) = i["total"].as_str().filter(|t| !t.is_empty()) {
            blocks.push(serde_json::json!({"warning": t}));
        }
        let mut kv = vec![serde_json::json!({"label": "Page", "value": i["title"].as_str().unwrap_or("")}), serde_json::json!({"label": "Button", "value": i["label"].as_str().unwrap_or("")})];
        // A checked option reads as "5 – Very satisfied ✓"; text fields as "Label: value".
        kv.extend(i["fields"].as_array().into_iter().flatten().take(3).map(|f| {
            let (l, v) = (f["label"].as_str().unwrap_or(""), f["value"].as_str().unwrap_or(""));
            if v == "✓" { serde_json::json!({"label": "Chosen", "value": format!("{l} ✓")}) } else { serde_json::json!({"label": l.chars().take(14).collect::<String>(), "value": v}) }
        }));
        blocks.push(serde_json::json!({"keyValue": kv}));
    }
    if a.tool == "chat_send" {
        let site = a.args["site"].as_str().unwrap_or("chat");
        let mut kv = vec![serde_json::json!({"label": "To", "value": format!("{} · {}{} ✓", a.args["chat"].as_str().unwrap_or("?"), if site == "whatsapp" { "WhatsApp" } else { site }, if a.args["via"] == "app" { " app" } else { "" })})];
        if let Some(l) = a.args["replying_to"].as_str().filter(|l| !l.is_empty()) {
            kv.push(serde_json::json!({"label": "Replying to", "value": l.chars().take(90).collect::<String>()}));
        }
        if let Some(w) = a.args["warning"].as_str() {
            kv.push(serde_json::json!({"label": "⚠︎ Note", "value": w}));
        }
        blocks.push(serde_json::json!({"keyValue": kv}));
    }
    Some(serde_json::json!({"type": "card", "kind": "confirm", "id": format!("c-{id}"),
        "blocks": blocks,
        "confirm": {"action_id": id, "fields": fields, "summary": a.summary.chars().take(300).collect::<String>()}}))
}

fn label(k: &str) -> String {
    match k { "to" => "To", "cc" => "Cc", "subject" => "Subject", "body" => "Message", "path" => "Save to", "start" => "Starts", "end" => "Ends", "due" => "Due", "title" => "Title", "text" => "Note", "location" => "Where", "calendar" => "Calendar", "list" => "List", other => other }.to_string()
}

/// A finished background request → one glanceable result card (VoiceOS-style, ≤ 3 blocks).
fn result_card(o: &crate::thinker::Outcome, kind: &str, task: &str, view: Option<serde_json::Value>) -> serde_json::Value {
    let last = o.steps.last();
    // A typed result (now playing, weather, a reminder or event that was added) is its own card:
    // the app draws it from the data, not from a sentence.
    let typed_last = last.is_some_and(|s| matches!(s.tool.as_str(), "music_play" | "weather" | "reminder_create" | "calendar_create" | "calendar_events" | "reminders_list" | "find_files"));
    if o.status == "done" && typed_last {
        if let Some(v) = view.filter(|v| v["type"].is_string()) {
            return serde_json::json!({"type": "card", "kind": "result", "id": format!("r{}", chrono::Local::now().timestamp_millis()),
                "blocks": [{"view": v}], "open": {"task": task}});
        }
    }
    let icon = match last.map(|s| s.tool.as_str()).unwrap_or("") {
        "research" => "magnifyingglass", "scrape" | "details" => "tablecells", "email_search" | "email_read" | "email_send" => "envelope",
        "weather" => "cloud.sun", "calendar_events" | "calendar_create" => "calendar", "code" => "terminal", "note_save" => "note.text",
        "recall_memory" => "brain", _ => "sparkles",
    };
    let ok = o.status == "done";
    // Weather → a real glance card (place · temperature, stats, next days), not raw JSON.
    if ok && last.is_some_and(|s| s.tool == "weather") {
        if let Some(w) = o.summary.split_once(": ").and_then(|(_, j)| serde_json::from_str::<serde_json::Value>(j).ok()) {
            let n = &w["now"];
            let num = |v: &serde_json::Value| v.as_f64().map(|x| format!("{x:.0}°")).unwrap_or_default();
            let days: Vec<serde_json::Value> = w["daily"].as_array().into_iter().flatten().take(3).map(|d| serde_json::json!({
                "title": chrono::NaiveDate::parse_from_str(d["date"].as_str().unwrap_or(""), "%Y-%m-%d").map(|x| x.format("%a %-d").to_string()).unwrap_or_default(),
                "subtitle": d["sky"], "trailing": format!("{} / {} · {}%", num(&d["max_c"]), num(&d["min_c"]), d["rain_chance_pct"].as_i64().unwrap_or(0))})).collect();
            return serde_json::json!({"type": "card", "kind": "result", "id": format!("r{}", chrono::Local::now().timestamp_millis()), "blocks": [
                {"header": {"icon": "cloud.sun", "title": format!("{} · {}", w["place"].as_str().unwrap_or(""), num(&n["temp_c"])), "detail": n["sky"]}},
                {"stats": [{"label": "Feels like", "value": num(&n["feels_c"])}, {"label": "Wind", "value": format!("{:.0} km/h", n["wind_kmh"].as_f64().unwrap_or(0.0))}]},
                {"list": days}], "open": {"task": task}});
        }
    }
    let title: String = o.request.chars().take(58).collect();
    let mut blocks = vec![serde_json::json!({"header": {"icon": icon, "title": title, "detail": if ok { "done".to_string() } else { "failed".to_string() }}})];
    blocks.push(serde_json::json!({"text": glance_text(&o.summary)}));
    if o.tokens > 0 {
        blocks.push(serde_json::json!({"badges": [{"label": format!("{} steps", o.steps.len())}, {"label": format!("{} tokens", o.tokens)}, {"label": kind.to_string()}]}));
    }
    // Opening a result shows its task in the overview (the presented report), never a raw file
    // in whatever app owns .md on this Mac.
    let open = Some(serde_json::json!({"task": task}));
    serde_json::json!({"type": "card", "kind": if ok { "result" } else { "error" }, "id": format!("r{}", chrono::Local::now().timestamp_millis()), "blocks": blocks, "open": open})
}

fn short(s: &str) -> String {
    if s.chars().count() > 80 { format!("{}…", s.chars().take(80).collect::<String>()) } else { s.to_string() }
}

/// The one glance line a result card shows: a tool's "name: {json}" summary becomes its scalar
/// facts, markdown loses its marks, tables fold into their first row, bullets become one line
/// (2026-10-14: the island showed raw tables and JSON while the overview rendered them).
pub fn glance_text(summary: &str) -> String {
    let s = summary.trim();
    // "tool: {…}" / "[…]" → scalar facts
    if let Some(i) = s.find(['{', '[']) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s[i..]) {
            let pre = s[..i].trim().trim_end_matches(':').trim();
            let mut facts: Vec<String> = Vec::new();
            match &v {
                serde_json::Value::Object(m) => {
                    for (k, x) in m.iter() {
                        let shown = match x {
                            serde_json::Value::String(t) => t.chars().take(60).collect::<String>(),
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            serde_json::Value::Array(a) => format!("{} items", a.len()),
                            _ => continue,
                        };
                        facts.push(format!("{} {}", k.replace('_', " "), shown));
                        if facts.len() == 4 { break; }
                    }
                }
                serde_json::Value::Array(a) => facts.push(format!("{} items", a.len())),
                _ => {}
            }
            let body = facts.join(" · ");
            return trim_glance(&if pre.is_empty() { body } else if body.is_empty() { pre.to_string() } else { format!("{pre}: {body}") });
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut table_rows = 0usize;
    for raw in s.lines() {
        let l = raw.trim();
        if l.starts_with("```") { in_code = !in_code; continue; }
        if in_code || l.is_empty() || l == "---" { continue; }
        if l.starts_with('|') {
            // a table: its header and first data row as "head value" pairs, the rest dropped
            if l.contains("---") { continue; }
            table_rows += 1;
            if table_rows == 1 { out.push(format!("\u{1}{}", l)); } else if table_rows == 2 {
                let cells = |x: &str| x.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect::<Vec<_>>();
                if let Some(head) = out.iter().rposition(|x| x.starts_with('\u{1}')) {
                    let h = cells(&out[head][1..]); let d = cells(l);
                    out[head] = h.iter().zip(&d).filter(|(a, b)| !a.is_empty() && !b.is_empty()).take(4).map(|(a, b)| format!("{a} {b}")).collect::<Vec<_>>().join(", ");
                }
            }
            continue;
        }
        table_rows = 0;
        let mut t = l.trim_start_matches('#').trim().to_string();
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) { t = format!("• {}", rest.trim()); }
        else if t.len() > 3 && t.as_bytes()[0].is_ascii_digit() && (t.starts_with(|c: char| c.is_ascii_digit()) && t[1..].starts_with(". ") || t[1..].starts_with(") ")) { t = format!("• {}", t[3..].trim()); }
        t = t.replace("**", "").replace("__", "").replace('`', "");
        // [text](url) → text; bare images dropped
        let mut cleaned = String::new();
        let mut rest = t.as_str();
        while let Some(a) = rest.find('[') {
            if let Some(b) = rest[a..].find("](") {
                if let Some(c) = rest[a + b..].find(')') {
                    let label = &rest[a + 1..a + b];
                    cleaned.push_str(&rest[..a]);
                    if !rest[..a].ends_with('!') { cleaned.push_str(label); } else { cleaned.pop(); }
                    rest = &rest[a + b + c + 1..];
                    continue;
                }
            }
            cleaned.push_str(&rest[..=a]); rest = &rest[a + 1..];
        }
        cleaned.push_str(rest);
        let cleaned = cleaned.trim().to_string();
        if !cleaned.is_empty() { out.push(cleaned); }
    }
    let joined = out.into_iter().map(|x| x.trim_start_matches('\u{1}').to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
    trim_glance(&joined)
}

/// ≤ 300 characters, cut at a sentence end if one is near, else at a word.
fn trim_glance(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= 300 { return s; }
    let head: String = s.chars().take(300).collect();
    let cut = head.rfind(['.', '!', '?']).filter(|&i| i > 160).map(|i| i + 1).or_else(|| head.rfind(' ')).unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end())
}

#[cfg(test)]
mod glance_tests {
    use super::glance_text;

    #[test]
    fn tables_json_and_markdown_fold_into_one_line() {
        let w = glance_text("**Vienna, Austria**: now 11 °C (feels 10 °C), overcast, wind 2 km/h\n\n| Day | Sky | Temp | Rain |\n|---|---|---|---|\n| Wed 14 Oct | rain showers | 8–19 °C | 45% |\n| Thu 15 Oct | overcast | 10–17 °C | 0% |");
        assert_eq!(w, "Vienna, Austria: now 11 °C (feels 10 °C), overcast, wind 2 km/h Day Wed 14 Oct, Sky rain showers, Temp 8–19 °C, Rain 45%");
        let j = glance_text("scrape: {\"title\": \"Honda Jazz\", \"price\": 3900, \"listings\": [1, 2, 3], \"nested\": {\"a\": 1}}");
        assert_eq!(j, "scrape: title Honda Jazz · price 3900 · listings 3 items");
        let m = glance_text("# Summary\n- First [point](https://x.y) with `code`\n- Second\n\n```\nraw\n```\nDone.");
        assert_eq!(m, "Summary • First point with code • Second Done.");
        assert!(glance_text(&"word ".repeat(100)).ends_with('…'));
    }
}
