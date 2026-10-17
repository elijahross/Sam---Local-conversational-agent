//! CLI / lab: run the background thinking agent on one request: `think "<request>"`.
//! Env: THINKER_MODEL (minimax|openai|gemini; default: LLM_PROVIDER), BG_SCREEN, SANDBOX,
//! CONFIRM_AFTER=approve|cancel (lab: then decide every drafted action in the same process, so
//! parked browser tabs survive). Approve only applies to CONFIRM_TOOLS (default web_task,chat_send,
//! which act on fixture pages); everything else is cancelled.
use voice_agent::{AppState, thinker};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    voice_agent::config::load_dev_env();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let req = std::env::args().nth(1).expect("usage: think \"<request>\"");
    unsafe { std::env::set_var("MEMORY", "0") };
    if std::env::var("SAM_REAL_APPS").is_err() {
        unsafe { std::env::set_var("SAM_NO_REAL_APPS", "1") };
    }
    let app = AppState::from_env().await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            eprintln!("\n>>> {m}");
        }
    });
    let o = thinker::run(&app, &req, std::env::var("KIND").ok().as_deref(), Some(tx.clone())).await?;
    println!("{}", serde_json::to_string(&o)?);
    let _ = &o;
    if let Ok(decision) = std::env::var("CONFIRM_AFTER") {
        let pending = voice_agent::tools::Pending::new(&thinker::data_dir());
        // Only tools that act on lab fixture pages may be auto-approved; anything touching the
        // user's real apps (calendar, reminders, mail) is always cancelled here.
        let safe = std::env::var("CONFIRM_TOOLS").unwrap_or_else(|_| "web_task,chat_send".into());
        for a in pending.open() {
            let ok = decision == "approve" && safe.split(',').any(|t| t.trim() == a.tool);
            if decision == "approve" && !ok {
                eprintln!("\n>>> not auto-approving {} ({}): cancelled", a.tool, a.id);
            }
            let text = if ok { format!("yes, go ahead {}", a.id) } else { format!("no, cancel {}", a.id) };
            let c = thinker::run(&app, &text, None, Some(tx.clone())).await?;
            println!("{}", serde_json::to_string(&c)?);
        }
    }
    voice_agent::sandbox::stop_if_idle().await;
    Ok(())
}
