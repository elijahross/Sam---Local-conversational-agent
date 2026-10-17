//! Open-ended browser tasks (log in, fill forms, navigate, extract) with a deterministic page
//! interface and a tiny action grammar, so the LLM sees ~1-2k tokens per step and context never
//! grows with retries.
//!
//! Page → snapshot (stable ids):            LLM → action lines (batched):
//!   FIELDS  f1 email "Email" = ""            fill f1 {{username}}
//!           f2 password "Password" = ""      fill f2 {{password}}
//!   BUTTONS b1 "Sign in"                     click b1
//!   LINKS   l3 "Pricing" → /pricing          done "logged in"
//!   CONTENT …top excerpts for the goal…
//!
//! Credentials stay in the Keychain (secrets.rs): placeholders are resolved for the page's own
//! site and only into matching field types. Laya handles consent walls and page/blocker state
//! before the LLM is asked; each step's history line carries Laya's action_ok verdict.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};

use super::{cdp::Tab, consent, extract};
use crate::{
    AppState,
    judge,
    llm::{self, Provider, Usage},
    mind, secrets,
};

#[derive(Debug, Clone)]
pub struct Options {
    pub max_steps: usize,
    pub model: Provider,
    pub max_links: usize,
    pub max_buttons: usize,
    pub content_chars: usize,
    pub mind_url: String,
    /// Approval gate: the final irreversible click (post, send, submit, pay, place order) is
    /// not executed; the run stops with status "ready" and `submit` describes it. Payment
    /// card fields are never filled.
    pub gate: bool,
    /// With `gate`: a run may still end with "done" (read-only goals, e.g. browse "check my order").
    pub done_ok: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_steps: 12, model: Provider::MiniMax, max_links: 15, max_buttons: 20, content_chars: 1200, mind_url: "http://127.0.0.1:47802".into(), gate: false, done_ok: false }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub n: usize,
    pub url: String,
    pub snapshot_chars: usize,
    pub actions: Vec<String>,
    pub outcome: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub goal: String,
    /// "done" | "failed" | "needs_user" | "max_steps"
    pub status: String,
    pub answer: String,
    pub steps: Vec<Step>,
    pub total_tokens: u64,
    pub wall_ms: u128,
    pub final_url: String,
    /// With `gate`: the final action waiting for the user's OK (button id + label, what was
    /// filled in, totals found on the page).
    pub submit: Option<Value>,
}

/// Collects interactive elements (incl. open shadow roots, same-origin iframes), tags them with
/// data-sam-id, and returns a compact description plus pruned page text.
const SNAPSHOT_JS: &str = r#"(() => {
  const docs = [document]; const roots = [document];
  const walk = (r) => r.querySelectorAll('*').forEach(e => { if (e.shadowRoot) { roots.push(e.shadowRoot); walk(e.shadowRoot); } });
  walk(document);
  document.querySelectorAll('iframe').forEach(f => { try { if (f.contentDocument) { roots.push(f.contentDocument); walk(f.contentDocument); } } catch (_) {} });
  const vis = (e) => { const r = e.getBoundingClientRect(); const s = getComputedStyle(e); return r.width > 1 && r.height > 1 && s.visibility !== 'hidden' && s.display !== 'none' && +s.opacity > 0.05; };
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const labelOf = (e) => {
    const id = e.id && e.getRootNode().querySelector && e.getRootNode().querySelector(`label[for="${CSS.escape(e.id)}"]`);
    return clean(e.getAttribute('aria-label') || (id && id.innerText) || (e.closest('label') && e.closest('label').innerText) || e.getAttribute('placeholder') || e.getAttribute('title') || e.getAttribute('name') || e.id || '').slice(0, 60);
  };
  // Ids are per snapshot: drop old tags first, or a now-hidden element (an earlier page of a
  // multi-step form) would keep an id that a visible one also gets.
  for (const r of roots) r.querySelectorAll('[data-sam-id]').forEach(e => e.removeAttribute('data-sam-id'));
  const out = {fields: [], buttons: [], links: [], checks: []}; const seen = new Set(); let k = {f: 0, b: 0, l: 0, c: 0};
  for (const r of roots) for (const e of r.querySelectorAll('input,textarea,select,button,a[href],[role="button"],[role="link"],[contenteditable="true"],[role="checkbox"],[role="radio"],[role="combobox"],[role="textbox"]')) {
    if (seen.has(e) || !vis(e) || e.disabled) continue; seen.add(e);
    const tag = e.tagName.toLowerCase(), type = (e.getAttribute('type') || '').toLowerCase();
    if (tag === 'input' && ['hidden'].includes(type)) continue;
    if ((tag === 'input' && ['checkbox', 'radio'].includes(type)) || ['checkbox', 'radio'].includes(e.getAttribute('role'))) {
      const id = 'c' + (++k.c); e.setAttribute('data-sam-id', id);
      out.checks.push({id, kind: type || e.getAttribute('role'), label: labelOf(e) || clean(e.parentElement && e.parentElement.innerText).slice(0, 60), checked: !!(e.checked || e.getAttribute('aria-checked') === 'true')});
    } else if (tag === 'input' && ['submit', 'button', 'image', 'reset'].includes(type) || tag === 'button' || e.getAttribute('role') === 'button') {
      const t = clean(e.innerText || e.value || e.getAttribute('aria-label') || e.title).slice(0, 60); if (!t) continue;
      const id = 'b' + (++k.b); e.setAttribute('data-sam-id', id); out.buttons.push({id, text: t});
    } else if (tag === 'input' || tag === 'textarea' || tag === 'select' || e.isContentEditable || ['combobox', 'textbox'].includes(e.getAttribute('role'))) {
      const id = 'f' + (++k.f); e.setAttribute('data-sam-id', id);
      const f = {id, kind: tag === 'select' ? 'select' : tag === 'textarea' ? 'textarea' : (type || 'text'), label: labelOf(e), value: tag === 'select' ? clean(e.selectedOptions[0] && e.selectedOptions[0].text) : (type === 'password' ? (e.value ? '••••' : '') : clean(e.value || e.innerText).slice(0, 40)), required: !!e.required};
      if (tag === 'select') f.options = [...e.options].slice(0, 15).map(o => clean(o.text)).filter(Boolean);
      out.fields.push(f);
    } else if (tag === 'a') {
      const t = clean(e.innerText || e.getAttribute('aria-label') || e.title).slice(0, 70); if (!t) continue;
      const id = 'l' + (++k.l); e.setAttribute('data-sam-id', id);
      let href = e.getAttribute('href') || ''; try { const u = new URL(e.href); href = u.host === location.host ? u.pathname + u.search : u.host + u.pathname; } catch (_) {}
      out.links.push({id, text: t, href: href.slice(0, 80)});
    }
  }
  const body = document.body ? document.body.cloneNode(true) : null;
  if (body) body.querySelectorAll('script,style,noscript,svg,nav,footer,header,form,button,select,input,textarea').forEach(e => e.remove());
  out.text = body ? clean(body.innerText).slice(0, 20000) : '';
  out.url = location.href; out.title = document.title;
  return out;
})()"#;

const FIND_JS: &str = r#"(id => { const find = (r) => { const e = r.querySelector(`[data-sam-id="${id}"]`); if (e) return e; for (const x of r.querySelectorAll('*')) if (x.shadowRoot) { const f = find(x.shadowRoot); if (f) return f; } return null; };
  let e = find(document); if (!e) document.querySelectorAll('iframe').forEach(f => { try { e = e || find(f.contentDocument); } catch (_) {} }); return e; })"#;

const SYSTEM: &str = "You operate a web browser to reach the user's goal. Each turn you see the current page: numbered FIELDS (f), BUTTONS (b), LINKS (l), CHECKS (c) and CONTENT.\n\
Reply with ONLY action lines, exactly one action per line. Available actions:\n\
fill f3 \"text\"\n\
fill f1 {{username}}   (also {{password}}, {{totp}}: stored credentials, never type real ones)\n\
select f4 \"option text\"\n\
check c2   /   uncheck c2\n\
click b1   (or a link: click l5)\n\
press Enter\n\
goto \"https://example.com/page\"\n\
scroll down\n\
wait 2\n\
ask_user \"question\"\n\
done \"short answer\"\n\
fail \"reason\"\n\
Elements marked * are new since your last step. Rules: batch every action that needs no new page view (fill a whole form, then click submit). Never invent ids. Use the credential placeholders for logins; never ask for or type real credentials. \
Use ask_user only for captchas or information only the user has. When the goal is reached, reply done with the answer or result. Do not repeat an action that already failed.";

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Fill(String, String),
    Select(String, String),
    Check(String, bool),
    Click(String),
    /// The final irreversible click, handed to the user (gate mode).
    Submit(String),
    Press(String),
    Goto(String),
    Scroll(bool),
    Wait(f32),
    AskUser(String),
    Done(String),
    Fail(String),
}

/// Parse LLM output into actions; unknown lines are reported, not executed.
pub fn parse_actions(text: &str) -> (Vec<Action>, Vec<String>) {
    let mut acts = Vec::new();
    let mut bad = Vec::new();
    let verbs = ["fill", "type", "select", "check", "uncheck", "click", "submit", "press", "goto", "open", "scroll", "wait", "ask_user", "done", "fail"];
    let split: Vec<String> = text
        .lines()
        .flat_map(|l| {
            // Models sometimes join actions with " | " on one line: split where a verb follows.
            let parts: Vec<&str> = l.split(" | ").collect();
            if parts.len() > 1 && parts.iter().skip(1).all(|p| verbs.iter().any(|v| p.trim_start().to_lowercase().starts_with(v))) {
                parts.into_iter().map(str::to_string).collect::<Vec<_>>()
            } else {
                vec![l.to_string()]
            }
        })
        .collect();
    for raw in split.iter() {
        let l = raw.trim().trim_start_matches(|c: char| c == '-' || c == '*' || c.is_ascii_digit() || c == '.' || c == ')').trim();
        if l.is_empty() || l.starts_with("```") {
            continue;
        }
        let (verb, rest) = l.split_once(char::is_whitespace).map(|(a, b)| (a.to_lowercase(), b.trim())).unwrap_or((l.to_lowercase(), ""));
        let arg = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
        let two = |s: &str| s.split_once(char::is_whitespace).map(|(a, b)| (a.to_string(), arg(b)));
        let a = match verb.as_str() {
            "fill" | "type" => two(rest).map(|(i, v)| Action::Fill(i, v)),
            "select" => two(rest).map(|(i, v)| Action::Select(i, v)),
            "check" => Some(Action::Check(arg(rest), true)),
            "uncheck" => Some(Action::Check(arg(rest), false)),
            "click" => Some(Action::Click(arg(rest))),
            "submit" => Some(Action::Submit(arg(rest))),
            "press" => Some(Action::Press(arg(rest))),
            "goto" | "open" => Some(Action::Goto(arg(rest))),
            "scroll" => Some(Action::Scroll(!rest.contains("up"))),
            "wait" => Some(Action::Wait(rest.trim().parse().unwrap_or(1.0f32).min(10.0))),
            "ask_user" => Some(Action::AskUser(arg(rest))),
            "done" => Some(Action::Done(arg(rest))),
            "fail" => Some(Action::Fail(arg(rest))),
            _ => None,
        };
        match a {
            Some(a) => acts.push(a),
            None => bad.push(l.chars().take(80).collect()),
        }
    }
    (acts, bad)
}

pub async fn run(app: &AppState, tab: &Tab, goal: &str, start_url: Option<&str>, opt: &Options) -> anyhow::Result<Outcome> {
    let t0 = Instant::now();
    let judge = app.judge.as_ref();
    if let Some(u) = start_url {
        tab.goto(u, Duration::from_secs(20)).await?;
    }
    let gv = mind::embed(&app.http, &opt.mind_url, &[goal.to_string()], "query").await.ok().and_then(|mut v| v.pop());
    let mut history: Vec<String> = Vec::new();
    let mut steps = Vec::new();
    let mut last_errors: Vec<String> = Vec::new();
    let mut tokens = 0u64;
    let mut prev_sigs = std::collections::HashSet::new();
    let mut early_done = 0;
    let finish = |status: &str, answer: String, steps: Vec<Step>, tokens: u64, url: String| Outcome { goal: goal.into(), status: status.into(), answer, steps, total_tokens: tokens, wall_ms: t0.elapsed().as_millis(), final_url: url, submit: None };
    let system = if opt.gate { format!("{SYSTEM}\n{GATE_RULES}") } else { SYSTEM.to_string() };

    for n in 1..=opt.max_steps {
        let st = Instant::now();
        // Cheap page handling before the LLM: consent walls and blockers.
        let mut snap = tab.eval(SNAPSHOT_JS).await?;
        if let Some(j) = judge {
            let probe = format!("{}\n{}", snap["title"].as_str().unwrap_or(""), snap["text"].as_str().unwrap_or("").chars().take(600).collect::<String>());
            if let Some((state, p)) = j.ask(json!(probe), judge::questions(&["page_state"]), 3000).await.and_then(|a| judge::choice(&a, "page_state")) {
                if state == "cookie_banner" && p >= 0.5 {
                    consent::dismiss(tab, Some(j)).await;
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    snap = tab.eval(SNAPSHOT_JS).await?;
                } else if state == "captcha" && p >= 0.75 && {
                    let t = probe.to_lowercase();
                    ["captcha", "robot", "verify you are human", "are you human", "security check", "unusual traffic", "challenge"].iter().any(|w| t.contains(w))
                } {
                    return Ok(finish("needs_user", "A captcha / bot check blocks the page: run `agent --show <url>`, solve it once, then retry.".into(), steps, tokens, snap["url"].as_str().unwrap_or("").into()));
                }
            }
        }
        let host = url::Url::parse(snap["url"].as_str().unwrap_or("")).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let view = mask_secrets(render(app, &snap, gv.as_deref(), opt, &prev_sigs).await, &host);
        prev_sigs = element_sigs(&snap);
        let user = format!(
            "GOAL: {goal}\n\nHISTORY:\n{}\n\nCURRENT PAGE:\n{view}\n\nYour action lines:",
            if history.is_empty() { "(start)".to_string() } else { history.join("\n") }
        );
        let (reply, u) = match llm::complete(app, opt.model, &system, &user, false, 1500).await {
            Ok(r) => r,
            Err(e) => {
                // A failed/slow LLM call is one step's error, not a crash; two in a row end the run.
                history.push(format!("{n}. (model call failed: {})", e.to_string().chars().take(80).collect::<String>()));
                if last_errors.first().is_some_and(|x| x.starts_with("model")) {
                    return Ok(finish("failed", format!("model unavailable: {e}"), steps, tokens, snap["url"].as_str().unwrap_or("").into()));
                }
                last_errors = vec![format!("model {e}")];
                continue;
            }
        };
        tokens += u.prompt + u.completion;
        let (actions, bad) = parse_actions(&reply);
        let before = page_brief(&snap);
        let url_before = snap["url"].as_str().unwrap_or("").to_string();
        let mut done_lines = Vec::new();
        let mut errors: Vec<String> = bad.iter().map(|b| format!("unparsed: {b}")).collect();
        let mut terminal: Option<(String, String)> = None;
        for a in &actions {
            // Gate: a click on a final-looking control is the submit step, whatever the model called it.
            let a = &match a {
                Action::Click(id) if opt.gate && is_final(tab, id).await => Action::Submit(id.clone()),
                // "submit" on Next / Continue / Add to cart only moves on: a plain click.
                Action::Submit(id) if opt.gate && moves_on(tab, id).await => Action::Click(id.clone()),
                // "done" with exactly one final button in view: that button is the submit step.
                Action::Done(_) if opt.gate && !opt.done_ok && final_buttons(&snap).len() == 1 => Action::Submit(final_buttons(&snap)[0].clone()),
                // Approval mode always ends at a final button: an early "done" is a mistake.
                Action::Done(t) if opt.gate && !opt.done_ok && early_done < 2 => {
                    early_done += 1;
                    errors.push(format!("not finished: \"{}\" — keep going until the final post/submit/order button, then reply submit <id>", t.chars().take(60).collect::<String>()));
                    break;
                }
                other => other.clone(),
            };
            if let (true, Action::Submit(id)) = (opt.gate, a) {
                let info = submit_info(tab, id).await?;
                steps.push(step_rec(n, &url_before, view.len(), &done_lines, &format!("ready to submit: {}", info["label"].as_str().unwrap_or("")), u, st));
                let url = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                let mut out = finish("ready", format!("ready: {}", info["label"].as_str().unwrap_or("submit")), steps, tokens, url);
                out.submit = Some(info);
                return Ok(out);
            }
            // Without approval mode (e.g. a login during a scrape) destructive controls are refused.
            if let (false, Action::Click(id)) = (opt.gate, a) {
                if label_has(&control_label(tab, id).await, DESTRUCTIVE_WORDS) {
                    errors.push(format!("{id} is a destructive control (delete / remove / cancel…): not clicked"));
                    break;
                }
            }
            if opt.gate {
                if let Some(e) = gate_refusal(tab, a).await {
                    errors.push(e.clone());
                    if e.starts_with("payment") {
                        steps.push(step_rec(n, &url_before, view.len(), &done_lines, &e, u, st));
                        return Ok(finish("needs_user", e, steps, tokens, url_before.clone()));
                    }
                    break;
                }
            }
            match exec(tab, a, &url_before).await {
                Ok(Exec::Done(s, text)) => {
                    terminal = Some((s, text));
                    break;
                }
                Ok(Exec::Ok(line)) => done_lines.push(line),
                Ok(Exec::Navigated(line)) => {
                    done_lines.push(line);
                    break; // ids are stale after navigation: the rest of the batch waits for a new view
                }
                Err(e) => {
                    errors.push(format!("{}: {e:#}", short(a)));
                    break;
                }
            }
        }
        if let Some((status, text)) = terminal {
            steps.push(step_rec(n, &url_before, view.len(), &done_lines, &format!("{status}: {text}"), u, st));
            // Approval mode never ends with "done": that would claim a result that wasn't committed.
            if opt.gate && status == "done" && !opt.done_ok {
                let url = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                return Ok(finish("failed", format!("stopped before the final step (nothing was submitted); the agent said: {text}"), steps, tokens, url));
            }
            let status = match status.as_str() { "done" => "done", "ask_user" => "needs_user", _ => "failed" };
            let url = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            return Ok(finish(status, text, steps, tokens, url));
        }
        settle(tab).await;
        let after_snap = tab.eval(SNAPSHOT_JS).await.unwrap_or(json!({}));
        let after = page_brief(&after_snap);
        let ok = match judge {
            Some(j) if !done_lines.is_empty() => j
                .ask(json!({"goal": goal, "action": done_lines.join("; "), "before": before, "after": after}), judge::questions(&["action_ok"]), 3000)
                .await
                .and_then(|a| judge::noul(&a, "action_ok")),
            _ => None,
        };
        let moved = after_snap["url"].as_str().unwrap_or("") != url_before;
        let outcome = format!(
            "{}{}{}",
            if done_lines.is_empty() { "no action".to_string() } else { done_lines.join(", ") },
            if moved { format!(" → now at {}", short_url(after_snap["url"].as_str().unwrap_or(""))) } else { format!(" → same page, {}", dom_diff(&snap, &after_snap)) },
            ok.map(|p| format!(" (progress {:.0}%)", p * 100.0)).unwrap_or_default()
        );
        // History: one line per step; errors as short stubs, never earlier page views (RawKit).
        let mut line = format!("{n}. {outcome}");
        if !errors.is_empty() {
            line.push_str(&format!(" | error: {}", errors.join("; ").chars().take(160).collect::<String>()));
        }
        history.push(line);
        steps.push(step_rec(n, &url_before, view.len(), &done_lines, &outcome, u, st));
        // Circuit breaker: the same error twice in a row ends the run instead of burning tokens.
        if !errors.is_empty() {
            if last_errors == errors {
                return Ok(finish("failed", format!("stuck: {}", errors.join("; ")), steps, tokens, after_snap["url"].as_str().unwrap_or("").into()));
            }
            last_errors = errors;
        } else {
            last_errors.clear();
        }
    }
    let url = tab.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    Ok(finish("max_steps", "step limit reached".into(), steps, tokens, url))
}

enum Exec {
    Ok(String),
    Navigated(String),
    Done(String, String),
}

async fn exec(tab: &Tab, a: &Action, url_before: &str) -> anyhow::Result<Exec> {
    let host = url::Url::parse(url_before).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    match a {
        Action::Fill(id, v) => {
            let kind = tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return null; return (e.getAttribute('type') || e.tagName).toLowerCase() + '|' + (e.getAttribute('autocomplete') || '') + '|' + (e.name || '') + '|' + (e.id || ''); }})()", json!(id))).await?;
            let kind = kind.as_str().context(format!("no element {id}"))?.to_string();
            let value = resolve_secret(v, &host, &kind)?;
            tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); e.scrollIntoView({{block: 'center'}}); e.focus(); if (e.select) e.select(); else document.execCommand('selectAll'); return true; }})()", json!(id))).await?;
            tab.send("Input.insertText", json!({"text": value})).await?;
            tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); e.dispatchEvent(new Event('change', {{bubbles: true}})); e.blur && e.blur(); return true; }})()", json!(id))).await?;
            let shown = if v.starts_with("{{") { v.clone() } else { format!("\"{}\"", v.chars().take(30).collect::<String>()) };
            Ok(Exec::Ok(format!("filled {id} {shown}")))
        }
        Action::Select(id, opt) => {
            let r = tab
                .eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return 'missing'; const o = [...e.options].find(o => o.text.trim().toLowerCase() === {}.toLowerCase()) || [...e.options].find(o => o.text.toLowerCase().includes({}.toLowerCase())); if (!o) return 'no option'; e.value = o.value; e.dispatchEvent(new Event('input', {{bubbles: true}})); e.dispatchEvent(new Event('change', {{bubbles: true}})); return 'ok'; }})()", json!(id), json!(opt), json!(opt)))
                .await?;
            match r.as_str() {
                Some("ok") => Ok(Exec::Ok(format!("selected {id} \"{opt}\""))),
                Some(e) => bail!("{e}"),
                None => bail!("select failed"),
            }
        }
        Action::Check(id, on) => {
            let r = tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return false; const cur = !!(e.checked || e.getAttribute('aria-checked') === 'true'); if (cur !== {on}) e.click(); return true; }})()", json!(id))).await?;
            if r.as_bool() != Some(true) {
                bail!("no element {id}");
            }
            Ok(Exec::Ok(format!("{} {id}", if *on { "checked" } else { "unchecked" })))
        }
        Action::Click(id) => {
            let r = tab
                .eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return null; e.scrollIntoView({{block: 'center'}}); const r = e.getBoundingClientRect(); return {{x: r.left + r.width / 2, y: r.top + r.height / 2}}; }})()", json!(id)))
                .await?;
            if r.is_null() {
                bail!("no element {id}");
            }
            let (x, y) = (r["x"].as_f64().unwrap_or(0.0), r["y"].as_f64().unwrap_or(0.0));
            for t in ["mouseMoved", "mousePressed", "mouseReleased"] {
                tab.send("Input.dispatchMouseEvent", json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": 1})).await?;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let now = tab.eval("location.href").await.unwrap_or_default();
            if now.as_str().is_some_and(|u| u != url_before) { Ok(Exec::Navigated(format!("clicked {id}"))) } else { Ok(Exec::Ok(format!("clicked {id}"))) }
        }
        Action::Press(key) => {
            let (k, code, vk) = match key.to_lowercase().as_str() {
                "enter" | "return" => ("Enter", "Enter", 13),
                "tab" => ("Tab", "Tab", 9),
                "escape" | "esc" => ("Escape", "Escape", 27),
                _ => bail!("unsupported key {key}"),
            };
            for t in ["keyDown", "keyUp"] {
                let mut p = json!({"type": t, "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk});
                if t == "keyDown" && k == "Enter" {
                    p["text"] = json!("\r");
                }
                tab.send("Input.dispatchKeyEvent", p).await?;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let now = tab.eval("location.href").await.unwrap_or_default();
            if now.as_str().is_some_and(|u| u != url_before) { Ok(Exec::Navigated(format!("pressed {k}"))) } else { Ok(Exec::Ok(format!("pressed {k}"))) }
        }
        Action::Goto(u) => {
            if !u.starts_with("http") {
                bail!("goto needs an absolute http(s) URL");
            }
            tab.goto(u, Duration::from_secs(20)).await?;
            Ok(Exec::Navigated(format!("opened {}", short_url(u))))
        }
        Action::Scroll(down) => {
            tab.eval(&format!("window.scrollBy(0, {} * innerHeight * 0.8)", if *down { 1 } else { -1 })).await?;
            Ok(Exec::Ok(format!("scrolled {}", if *down { "down" } else { "up" })))
        }
        Action::Wait(s) => {
            tokio::time::sleep(Duration::from_secs_f32(*s)).await;
            Ok(Exec::Ok(format!("waited {s}s")))
        }
        Action::Submit(id) => bail!("submit {id} is only for approval mode"),
        Action::AskUser(q) => Ok(Exec::Done("ask_user".into(), q.clone())),
        Action::Done(t) => Ok(Exec::Done("done".into(), t.clone())),
        Action::Fail(t) => Ok(Exec::Done("fail".into(), t.clone())),
    }
}

const GATE_RULES: &str = "APPROVAL RULES: never click the button that finally posts, sends, submits, pays or places the order. \
Fill in and select everything first, move through the pages, then reply `submit <id>` with that final button's id: the user approves it. \
Never type payment card numbers, CVC or bank details. Use the payment method the user asked for if it's already saved on the page (saved card, invoice); \
if they asked for one you may not enter (a new card), don't switch to another method: ask_user. \
Use only facts from the goal; if a required field needs information you don't have (address, email…), ask_user.";

/// Words of final, irreversible controls (EN/DE).
const FINAL_WORDS: &[&str] = &["submit", "post", "publish", "send", "place order", "buy", "pay", "purchase", "checkout now", "complete order", "confirm order",
    "order now", "book now", "sign up", "absenden", "senden", "abschicken", "veröffentlichen", "kaufen", "bestellen", "bezahlen", "zahlungspflichtig", "buchen",
    // committing / confirming controls: also need the user's OK in approval mode
    "confirm", "ok", "yes", "accept", "agree", "approve", "apply", "save", "transfer", "bestätigen", "speichern", "zustimmen", "übernehmen", "überweisen"];

/// Destructive controls: never clicked by the agent on its own. In approval mode they become the
/// approval step; otherwise they're refused.
const DESTRUCTIVE_WORDS: &[&str] = &["delete", "remove", "erase", "discard", "trash", "unsubscribe", "cancel subscription", "cancel order", "cancel booking",
    "close account", "delete account", "deactivate", "terminate", "revoke", "disconnect", "reset", "wipe", "empty bin", "empty trash",
    "löschen", "entfernen", "kündigen", "stornieren", "abmelden", "deaktivieren", "zurücksetzen", "konto schließen"];

fn label_has(label: &str, words: &[&str]) -> bool {
    let l = format!(" {} ", label.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != 'ä' && c != 'ö' && c != 'ü' && c != 'ß', " "));
    words.iter().any(|w| l.contains(&format!(" {w} ")))
}

/// Ids of visible final-looking buttons in a snapshot (Post comment, Place order, Submit…).
fn final_buttons(snap: &Value) -> Vec<String> {
    snap["buttons"].as_array().into_iter().flatten().filter(|b| {
        let t = b["text"].as_str().unwrap_or("").to_lowercase();
        !MOVES_ON.iter().any(|w| t.contains(w)) && label_has(&t, FINAL_WORDS)
    }).filter_map(|b| b["id"].as_str().map(str::to_string)).collect()
}

async fn control_label(tab: &Tab, id: &str) -> String {
    tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return ''; return ((e.innerText || e.value || e.getAttribute('aria-label') || '') + ' ' + (e.type === 'submit' ? '[submit]' : '')).toLowerCase(); }})()", json!(id))).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// "Next", "Continue to review", "Go to checkout", "Add to cart" move on; they're not final.
const MOVES_ON: &[&str] = &["next", "continue", "weiter", "go to checkout", "review", "back", "zurück", "add to cart", "in den warenkorb", "proceed to"];

async fn moves_on(tab: &Tab, id: &str) -> bool {
    let l = control_label(tab, id).await;
    MOVES_ON.iter().any(|w| l.contains(w))
}

async fn is_final(tab: &Tab, id: &str) -> bool {
    let label = control_label(tab, id).await;
    label_has(&label, DESTRUCTIVE_WORDS) || (!MOVES_ON.iter().any(|w| label.contains(w)) && label_has(&label, FINAL_WORDS))
}

/// Gate refusals: payment details are never typed; Enter in a form field would submit it.
async fn gate_refusal(tab: &Tab, a: &Action) -> Option<String> {
    match a {
        Action::Fill(id, _) => {
            let k = tab.eval(&format!("(() => {{ const e = {FIND_JS}({}); if (!e) return ''; return [e.getAttribute('autocomplete'), e.name, e.id, e.getAttribute('aria-label'), e.placeholder].join(' ').toLowerCase(); }})()", json!(id))).await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            ["cc-number", "cc-csc", "cc-exp", "card", "cvc", "cvv", "iban", "kreditkarte", "kartennummer"].iter().any(|w| k.contains(w))
                .then(|| "payment details must be entered by you: Sam never types card or bank numbers".to_string())
        }
        Action::Click(id) => {
            let l = control_label(tab, id).await;
            l.trim().trim_end_matches("[submit]").trim().is_empty().then(|| "an unlabeled button (icon only): not clicked in approval mode".to_string())
        }
        Action::Goto(u) => {
            let here = tab.eval("location.host").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            let there = url::Url::parse(u).ok().and_then(|x| x.host_str().map(str::to_string)).unwrap_or_default();
            let site = |h: &str| h.trim_start_matches("www.").to_string();
            (site(&here) != site(&there)).then(|| format!("stays on {here}: leaving the site isn't allowed in approval mode"))
        }
        Action::Press(k) if k.eq_ignore_ascii_case("enter") || k.eq_ignore_ascii_case("return") => {
            let search = tab.eval("(() => { const e = document.activeElement; return !!e && (e.type === 'search' || e.getAttribute('role') === 'searchbox' || !!e.closest('[role=search]')); })()").await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
            (!search).then(|| "Enter would submit this form: use `submit <button id>` for the final button".to_string())
        }
        _ => None,
    }
}

/// What the final click will commit: its label, the filled-in fields (passwords masked), the
/// chosen options and any total on the page.
async fn submit_info(tab: &Tab, id: &str) -> anyhow::Result<Value> {
    tab.eval(&format!(
        r#"(() => {{
           const b = {FIND_JS}({id});
           const label = el => {{ let l = el.labels && el.labels[0] ? el.labels[0].innerText : (el.getAttribute('aria-label') || el.placeholder || el.name || el.id || '');
             if (el.tagName === 'SELECT') for (const o of el.options) l = l.replace(o.text, '');   // a select's label contains its options
             return l.replace(/\s+/g, ' ').trim().slice(0, 60); }};
           const vis = el => {{ const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; }};
           const fields = [];
           // The whole form the button submits (multi-step forms hide earlier pages), else what's visible.
           const scope = b && b.form ? b.form : document;
           scope.querySelectorAll('input, textarea, select').forEach(el => {{
             if ((scope === document && !vis(el)) || el.type === 'hidden' || el.type === 'submit' || el.type === 'button') return;
             if (el.type === 'radio' || el.type === 'checkbox') {{ if (el.checked) fields.push({{label: label(el), value: '✓'}}); return; }}
             const v = el.tagName === 'SELECT' ? (el.selectedOptions[0] || {{}}).text || '' : el.value;
             if (v && v.trim()) fields.push({{label: label(el), value: el.type === 'password' ? '••••' : v.slice(0, 300)}});
           }});
           const text = document.body.innerText;
           const total = (text.match(/(total|gesamt|summe|amount due)[^\n]{{0,40}}?(€|\$|£|EUR|USD)\s?[\d.,]+|(total|gesamt|summe)[^\n]{{0,20}}[\d.,]+\s?(€|EUR)/i) || [''])[0];
           const box = b && b.closest('form, section, [class*=step], [class*=review], main, article');
           const summary = box ? box.innerText.replace(/\s+/g, ' ').trim().slice(0, 400) : '';
           return {{id: {id}, summary, label: b ? (b.innerText || b.value || b.getAttribute('aria-label') || 'submit').trim().slice(0, 60) : 'submit', url: location.href, title: document.title, fields: fields.slice(0, 20), total}};
         }})()"#,
        id = json!(id)
    ))
    .await
}

/// After approval: the page must still be the drafted one with the same values; then the final
/// button is clicked once and the result read back.
pub async fn commit(app: &AppState, tab: &Tab, info: &Value) -> anyhow::Result<String> {
    let id = info["id"].as_str().context("no submit id")?;
    let now = submit_info(tab, id).await?;
    if now["url"] != info["url"] || now["fields"] != info["fields"] {
        bail!("the page changed since the draft (url or filled values differ); not submitting");
    }
    let before = tab.eval(SNAPSHOT_JS).await.unwrap_or(json!({}));
    match exec(tab, &Action::Click(id.to_string()), info["url"].as_str().unwrap_or("")).await? {
        Exec::Ok(_) | Exec::Navigated(_) => {}
        Exec::Done(..) => {}
    }
    settle(tab).await;
    let after = tab.eval(SNAPSHOT_JS).await.unwrap_or(json!({}));
    let ok = match app.judge.as_ref() {
        Some(j) => j.ask(json!({"goal": format!("{} on {}", info["label"], info["title"]), "action": format!("clicked {}", info["label"]), "before": page_brief(&before), "after": page_brief(&after)}), judge::questions(&["action_ok"]), 3000).await.and_then(|a| judge::noul(&a, "action_ok")),
        None => None,
    };
    let (bt, at) = (before["text"].as_str().unwrap_or(""), after["text"].as_str().unwrap_or(""));
    let moved = after["url"] != before["url"];
    let changed = bt != at;
    // What appeared after the click ("Thanks! Your comment has been posted.", "Order #… confirmed").
    // Sentences, not lines: page text often has no line breaks between blocks.
    let sentences = |t: &str| -> Vec<String> { regex::Regex::new(r"[.!?]\s*|\n").unwrap().split(t).map(|x| x.trim().to_string()).filter(|x| x.len() > 2).collect() };
    let old: std::collections::HashSet<String> = sentences(bt).into_iter().collect();
    let fresh_owned: Vec<String> = sentences(at).into_iter().filter(|l| !old.contains(l)).take(4).collect();
    let fresh: Vec<&str> = fresh_owned.iter().map(String::as_str).collect();
    let text: String = if fresh.is_empty() { at.chars().take(300).collect() } else { fresh.join(" · ").chars().take(400).collect() };
    if !moved && !changed {
        bail!("clicked \"{}\" but the page didn't react; it MAY not have gone through — check the site", info["label"].as_str().unwrap_or(""));
    }
    Ok(format!("clicked \"{}\"{}; the page now says: {}", info["label"].as_str().unwrap_or(""), ok.map(|p| format!(" (looks {:.0}% successful)", p * 100.0)).unwrap_or_default(), text.split_whitespace().collect::<Vec<_>>().join(" ")))
}

/// `{{password}}` etc. → Keychain value for this site, only into a field of the right kind.
fn resolve_secret(v: &str, host: &str, field_kind: &str) -> anyhow::Result<String> {
    let Some(name) = v.strip_prefix("{{").and_then(|s| s.strip_suffix("}}")) else {
        if v.contains("{{") {
            bail!("malformed placeholder in value; use exactly {{{{username}}}}, {{{{password}}}} or {{{{totp}}}}");
        }
        return Ok(v.to_string());
    };
    let k = field_kind.to_lowercase();
    let allowed = match name {
        "password" => k.starts_with("password"),
        "totp" => ["otp", "code", "2fa", "one-time", "token", "verification"].iter().any(|w| k.contains(w)) || k.starts_with("tel") || k.starts_with("number"),
        "username" => !k.starts_with("password"),
        _ => bail!("unknown placeholder {v}"),
    };
    if !allowed {
        bail!("{v} refused: field type '{}' does not match", k.split('|').next().unwrap_or(""));
    }
    secrets::get(host, name).with_context(|| format!("no {name} stored for {} (run: agent --login-add {} {name})", secrets::site(host), secrets::site(host)))
}

/// Compact page view: ranked links/buttons, all fields, goal-relevant content excerpts.
/// Signature of an element for "new since last step" marks (browser-use's `*[idx]`).
fn sig(e: &Value) -> String {
    format!("{}|{}|{}", e["kind"].as_str().unwrap_or(""), e["label"].as_str().or(e["text"].as_str()).unwrap_or(""), e["href"].as_str().unwrap_or(""))
}

/// What a batch changed on the page, as counts: `+3 button "Delete", -1 field "Search"`.
fn dom_diff(before: &Value, after: &Value) -> String {
    use std::collections::BTreeMap;
    let count = |s: &Value| {
        let mut m: BTreeMap<String, i64> = BTreeMap::new();
        for (k, kind) in [("fields", "field"), ("buttons", "button"), ("links", "link"), ("checks", "check")] {
            for e in s[k].as_array().cloned().unwrap_or_default() {
                let label = e["label"].as_str().or(e["text"].as_str()).unwrap_or("").chars().take(30).collect::<String>();
                *m.entry(format!("{kind} \"{label}\"")).or_default() += 1;
            }
        }
        m
    };
    let (a, b) = (count(before), count(after));
    let mut parts = Vec::new();
    for k in a.keys().chain(b.keys()).collect::<std::collections::BTreeSet<_>>() {
        let d = b.get(k).copied().unwrap_or(0) - a.get(k).copied().unwrap_or(0);
        if d != 0 {
            parts.push(format!("{}{} {k}", if d > 0 { "+" } else { "" }, d));
        }
    }
    // Changed field values / check states matter too (dropdowns, checkboxes).
    let vals = |s: &Value| -> Vec<String> {
        s["fields"].as_array().into_iter().flatten().map(|f| format!("{}={}", f["label"], f["value"]))
            .chain(s["checks"].as_array().into_iter().flatten().map(|c| format!("{}={}", c["label"], c["checked"]))).collect()
    };
    let (va, vb) = (vals(before), vals(after));
    let changed = vb.iter().filter(|x| !va.contains(x)).count();
    if changed > 0 && parts.is_empty() {
        parts.push(format!("{changed} value(s) changed"));
    }
    if parts.is_empty() { "no visible change".into() } else { parts.into_iter().take(6).collect::<Vec<_>>().join(", ") }
}

fn element_sigs(s: &Value) -> std::collections::HashSet<String> {
    ["fields", "buttons", "links", "checks"].iter().flat_map(|k| s[*k].as_array().cloned().unwrap_or_default()).map(|e| sig(&e)).collect()
}

/// Never let a stored credential appear in what the model reads (it may be echoed on a page).
fn mask_secrets(view: String, host: &str) -> String {
    let mut v = view;
    for f in ["username", "password"] {
        if let Some(secret) = secrets::get(host, f).filter(|s| s.len() >= 4) {
            v = v.replace(&secret, &format!("{{{{{f}}}}}"));
        }
    }
    // A one-time code just filled shows up again as the field's value (it isn't a password
    // field): mask the current and the previous code of this site.
    if let Some(seed) = secrets::get_raw(host, "totp") {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for t in [now, now.saturating_sub(30)] {
            if let Some(code) = secrets::totp_at(&seed, t) {
                v = v.replace(&code, "{{totp}}");
            }
        }
    }
    v
}

async fn render(app: &AppState, s: &Value, goal_vec: Option<&[f32]>, opt: &Options, prev: &std::collections::HashSet<String>) -> String {
    let new = |e: &Value| if !prev.is_empty() && !prev.contains(&sig(e)) { "*" } else { "" };
    let arr = |k: &str| s[k].as_array().cloned().unwrap_or_default();
    let (fields, checks) = (arr("fields"), arr("checks"));
    let mut buttons = arr("buttons");
    let mut links = arr("links");
    let rank = |items: &mut Vec<Value>, key: &str, keep: usize| {
        let texts: Vec<String> = items.iter().map(|i| format!("{} {}", i[key].as_str().unwrap_or(""), i["href"].as_str().unwrap_or(""))).collect();
        (texts, keep)
    };
    for (items, key, keep) in [(&mut links, "text", opt.max_links), (&mut buttons, "text", opt.max_buttons)] {
        if items.len() > keep {
            if let Some(gv) = goal_vec {
                let (texts, keep) = rank(items, key, keep);
                if let Ok(vs) = mind::embed(&app.http, &opt.mind_url, &texts, "doc").await {
                    let mut scored: Vec<(f32, Value)> = vs.iter().map(|v| mind::dot(gv, v)).zip(items.drain(..)).collect();
                    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                    *items = scored.into_iter().take(keep).map(|x| x.1).collect();
                    continue;
                }
            }
            items.truncate(keep);
        }
    }
    let mut out = format!("PAGE {} \"{}\"\n", short_url(s["url"].as_str().unwrap_or("")), s["title"].as_str().unwrap_or(""));
    if !fields.is_empty() {
        out.push_str("FIELDS\n");
        for f in &fields {
            let opts = f["options"].as_array().map(|o| format!(" options: {}", o.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | "))).unwrap_or_default();
            out.push_str(&format!(
                "  {}{} {} \"{}\" = \"{}\"{}{}\n",
                new(f),
                f["id"].as_str().unwrap_or(""),
                f["kind"].as_str().unwrap_or(""),
                f["label"].as_str().unwrap_or(""),
                f["value"].as_str().unwrap_or(""),
                if f["required"].as_bool() == Some(true) { " required" } else { "" },
                opts
            ));
        }
    }
    if !checks.is_empty() {
        out.push_str("CHECKS\n");
        for c in &checks {
            out.push_str(&format!("  {}{} {} \"{}\" {}\n", new(c), c["id"].as_str().unwrap_or(""), c["kind"].as_str().unwrap_or(""), c["label"].as_str().unwrap_or(""), if c["checked"].as_bool() == Some(true) { "[x]" } else { "[ ]" }));
        }
    }
    if !buttons.is_empty() {
        out.push_str("BUTTONS\n");
        for b in &buttons {
            out.push_str(&format!("  {}{} \"{}\"\n", new(b), b["id"].as_str().unwrap_or(""), b["text"].as_str().unwrap_or("")));
        }
    }
    if !links.is_empty() {
        out.push_str("LINKS\n");
        for l in &links {
            out.push_str(&format!("  {}{} \"{}\" → {}\n", new(l), l["id"].as_str().unwrap_or(""), l["text"].as_str().unwrap_or(""), l["href"].as_str().unwrap_or("")));
        }
    }
    let text = s["text"].as_str().unwrap_or("");
    if !text.is_empty() {
        let content = match goal_vec {
            Some(gv) if text.len() > opt.content_chars => {
                let chunks = extract::chunks(&text.replace(". ", ".\n\n"), 400);
                match mind::embed(&app.http, &opt.mind_url, &chunks, "doc").await {
                    Ok(vs) => {
                        let mut sc: Vec<(f32, usize)> = vs.iter().map(|v| mind::dot(gv, v)).zip(0..).collect();
                        sc.sort_by(|a, b| b.0.total_cmp(&a.0));
                        let mut pick: Vec<usize> = Vec::new();
                        let mut used = 0;
                        for (_, i) in sc {
                            if used + chunks[i].len() > opt.content_chars {
                                continue;
                            }
                            used += chunks[i].len();
                            pick.push(i);
                        }
                        pick.sort();
                        pick.iter().map(|i| chunks[*i].as_str()).collect::<Vec<_>>().join(" … ")
                    }
                    Err(_) => text.chars().take(opt.content_chars).collect(),
                }
            }
            _ => text.chars().take(opt.content_chars).collect(),
        };
        out.push_str(&format!("CONTENT\n  {content}\n"));
    }
    out
}

fn page_brief(s: &Value) -> String {
    format!("{} | {} | {}", short_url(s["url"].as_str().unwrap_or("")), s["title"].as_str().unwrap_or(""), s["text"].as_str().unwrap_or("").chars().take(200).collect::<String>())
}

/// Wait for the page to settle after actions (navigation / XHR), bounded.
async fn settle(tab: &Tab) {
    let end = Instant::now() + Duration::from_secs(6);
    tokio::time::sleep(Duration::from_millis(500)).await;
    while Instant::now() < end {
        if tab.eval("document.readyState").await.ok().and_then(|v| v.as_str().map(|s| s == "complete")).unwrap_or(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn short_url(u: &str) -> String {
    url::Url::parse(u).map(|x| format!("{}{}", x.host_str().unwrap_or(""), x.path())).unwrap_or_else(|_| u.to_string()).chars().take(80).collect()
}

fn short(a: &Action) -> String {
    format!("{a:?}").chars().take(60).collect()
}

fn step_rec(n: usize, url: &str, chars: usize, actions: &[String], outcome: &str, u: Usage, t: Instant) -> Step {
    Step { n, url: short_url(url), snapshot_chars: chars, actions: actions.to_vec(), outcome: outcome.chars().take(200).collect(), prompt_tokens: u.prompt, completion_tokens: u.completion, ms: t.elapsed().as_millis() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        let (j, _) = parse_actions("fill f1 {{username}} | fill f2 {{password}} | click b1");
        assert_eq!(j.len(), 3);
        assert!(resolve_secret("{{username}} | fill f2", "example.com", "text").is_err());
        let (a, bad) = parse_actions("1. fill f1 {{username}}\n- fill f2 \"hello world\"\nselect f3 \"Germany\"\nclick b1\npress Enter\nfrobnicate x\ndone \"logged in\"");
        assert_eq!(a[0], Action::Fill("f1".into(), "{{username}}".into()));
        assert_eq!(a[1], Action::Fill("f2".into(), "hello world".into()));
        assert_eq!(a[2], Action::Select("f3".into(), "Germany".into()));
        assert_eq!(a[3], Action::Click("b1".into()));
        assert_eq!(a.last().unwrap(), &Action::Done("logged in".into()));
        assert_eq!(bad.len(), 1);
    }

    #[test]
    fn secrets_only_into_matching_fields() {
        assert!(resolve_secret("{{password}}", "example.com", "text|username||").is_err());
        assert_eq!(resolve_secret("plain", "example.com", "text").unwrap(), "plain");
    }
}
