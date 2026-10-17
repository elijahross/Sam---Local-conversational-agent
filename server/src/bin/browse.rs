//! Lab / CLI entry: `browse "<research request>"` → artifacts dir + JSON report on stdout.
//! Options via env: N, RELEVANCE (laya|embed|both), STATIC_FIRST (1|0), SYNTH_BUDGET, PLANNER
//! (minimax|openai|gemini; default: LLM_PROVIDER), SYNTHESIZER (minimax|gemini), SCREEN (Chrome profile number), HEADLESS (0 = off-screen headful, default), RENDER_CONC, STATIC_CONC.
use std::path::PathBuf;

use voice_agent::{
    AppState,
    artifacts::{self, Catalog},
    browser::{
        cdp::{Chrome, Visibility},
        research,
    },
    env_num, env_or,
    llm::Provider,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let request = std::env::args().nth(1).expect("usage: browse \"<research request>\"");
    // Background agent context: no voice-session memory needed.
    unsafe { std::env::set_var("MEMORY", "0") };
    let app = AppState::from_env().await?;
    let data: PathBuf = env_or("DATA_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/../data")).into();
    let screen = env_num("SCREEN", 1u16);
    let vis = if env_num("HEADLESS", 0u8) == 1 { Visibility::Headless } else { Visibility::Offscreen };
    let chrome = Chrome::launch(&data, screen, vis).await?;
    let opt = research::Options {
        n: env_num("N", 10),
        static_first: env_num("STATIC_FIRST", 1u8) == 1,
        static_concurrency: env_num("STATIC_CONC", 6),
        render_concurrency: env_num("RENDER_CONC", 2),
        // Lab winners (browser lab): embeddings ≥ 0.55, 3k-char brief context.
        relevance: env_or("RELEVANCE", "embed"),
        laya_threshold: env_num("LAYA_T", 0.5),
        embed_threshold: env_num("EMBED_T", 0.55),
        synth_budget: env_num("SYNTH_BUDGET", 3_000),
        planner: Provider::parse(&env_or("PLANNER", "")).unwrap_or(app.cfg.provider),
        synthesizer: Provider::parse(&env_or("SYNTHESIZER", "")).unwrap_or(app.cfg.provider),
        mind_url: env_or("MIND_URL", "http://127.0.0.1:47802"),
        keep_rejected: env_num("KEEP_REJECTED", 0u8) == 1,
    };
    let catalog = Catalog::open(&data)?;
    let out = artifacts::run_dir(&catalog.root, "research", &request);
    let report = research::run(&app, &chrome, &request, &out, Some(&catalog), &opt).await?;
    println!("{}", serde_json::to_string(&report)?);
    let _ = chrome.save_cookies(screen).await;
    if env_num("KEEP_BROWSER", 0u8) == 0 {
        chrome.shutdown().await;
    }
    Ok(())
}
