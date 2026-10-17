//! CLI: `scrape "<task>" <start_url> field1,field2,...` → items.jsonl/items.csv + JSON report.
//! Detail pages: `scrape --details "<task>" <urls.txt | url,url,...> field1,field2,...`.
//! Env: MAX_ITEMS (200), MAX_PAGES (30), MODEL (minimax|openai|gemini; default: LLM_PROVIDER), PAGE_DELAY_MS (1500),
//! SCREEN, AUTO_LOGIN (0 = ask the user on login walls; 1 = use stored Keychain credentials).
use std::path::PathBuf;

use voice_agent::{
    AppState,
    artifacts::Catalog,
    browser::{
        cdp::{Chrome, Visibility},
        scrape,
    },
    env_num, env_or,
    llm::Provider,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let mut a: Vec<String> = std::env::args().collect();
    let details = a.get(1).is_some_and(|x| x == "--details");
    if details {
        a.remove(1);
    }
    if a.len() < 4 {
        anyhow::bail!("usage: scrape \"<task>\" <start_url> field1,field2,...");
    }
    let schema: Vec<String> = a[3].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    unsafe { std::env::set_var("MEMORY", "0") };
    let app = AppState::from_env().await?;
    let data: PathBuf = env_or("DATA_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/../data")).into();
    let screen = env_num("SCREEN", 1u16);
    let chrome = Chrome::launch(&data, screen, Visibility::Offscreen).await?;
    let tab = chrome.new_tab().await?;
    let catalog = Catalog::open(&data)?;
    let opt = scrape::Options {
        max_items: env_num("MAX_ITEMS", 200),
        max_pages: env_num("MAX_PAGES", 30),
        model: Provider::parse(&env_or("MODEL", "")).unwrap_or(app.cfg.provider),
        page_delay_ms: env_num("PAGE_DELAY_MS", 1500),
        recipes_dir: scrape::default_recipes_dir(&data),
        auto_login: env_num("AUTO_LOGIN", 0u8) == 1,
        on_user: None,
    };
    let res = if details {
        let urls: Vec<String> = match std::fs::read_to_string(&a[2]) {
            Ok(t) => t.lines().map(str::trim).filter(|l| l.starts_with("http")).map(str::to_string).collect(),
            Err(_) => a[2].split(',').map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect(),
        };
        scrape::run_details(&app, &tab, &a[1], &urls, &schema, &catalog, &opt).await
    } else {
        scrape::run(&app, &tab, &a[1], &a[2], &schema, &catalog, &opt).await
    };
    let _ = chrome.save_cookies(screen).await;
    tab.close().await;
    if env_num("KEEP_BROWSER", 0u8) == 0 {
        chrome.shutdown().await;
    }
    println!("{}", serde_json::to_string(&res?)?);
    Ok(())
}
