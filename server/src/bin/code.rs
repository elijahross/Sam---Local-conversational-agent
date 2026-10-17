//! CLI: `code "<task>" <input files...>` → the model writes one Python script, it runs in the
//! sandbox VM (sandbox lab), and the JSON answer + outputs dir are printed.
//! Inputs must be granted (`agent --grant <dir>`) or live under data/artifacts.
//! Env: MODEL (minimax|openai|gemini; default: LLM_PROVIDER), SANDBOX (container|docker), MAX_REPAIRS, TIMEOUT_S.
use std::path::PathBuf;

use voice_agent::{AppState, codegen, env_num, env_or, files::Files, llm::Provider, sandbox::Sandbox};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    voice_agent::config::load_dev_env();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        anyhow::bail!("usage: code \"<task>\" <input files...>");
    }
    unsafe { std::env::set_var("MEMORY", "0") };
    let app = AppState::from_env().await?;
    let data: PathBuf = voice_agent::thinker::data_dir();
    let files = Files::open(&data)?;
    let sb = Sandbox::new(&data);
    let inputs: Vec<PathBuf> = a[2..].iter().map(PathBuf::from).collect();
    let opt = codegen::Options {
        model: Provider::parse(&env_or("MODEL", "")).unwrap_or(app.cfg.provider),
        max_repairs: env_num("MAX_REPAIRS", 2),
        timeout: std::time::Duration::from_secs(env_num("TIMEOUT_S", 90)),
        ..Default::default()
    };
    let sol = codegen::solve(&app, &sb, &files, &a[1], &inputs, &opt).await?;
    println!("{}", serde_json::to_string(&sol)?);
    Ok(())
}
