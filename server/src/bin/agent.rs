//! Browser agent CLI.
//!   agent "<goal>" [start-url]          run an open-ended task on screen $SCREEN (default 1)
//!   agent --login-add <domain> <field>  store username | password | totp (base32 seed) in the Keychain
//!   agent --grant <path> [note] | --revoke <path> | --grants     user-only file access grants
//!   agent --find <words>                search artifacts catalog + granted files by name
//!   agent --read <path> [offset]        read a granted document as markdown (paginated)
//!   agent --show [url]                  open the screen VISIBLY to log in / solve a captcha once;
//!                                       press Enter here when done → session saved for future runs
//! Env: AGENT_MODEL (minimax|openai|gemini; default: LLM_PROVIDER), MAX_STEPS, SCREEN, HEADLESS.
use std::io::BufRead;
use std::path::PathBuf;

use voice_agent::{
    AppState,
    browser::{
        agent,
        cdp::{Chrome, Visibility},
    },
    artifacts, env_num, env_or, files,
    llm::Provider,
    secrets,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    voice_agent::config::load_dev_env();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data: PathBuf = voice_agent::thinker::data_dir();
    let screen = env_num("SCREEN", 1u16);
    match args.first().map(String::as_str) {
        Some("--login-add") => {
            let (domain, field) = (args.get(1).expect("domain"), args.get(2).expect("field: username|password|totp"));
            let value = if field == "username" {
                eprint!("{field} for {}: ", secrets::site(domain));
                let mut l = String::new();
                std::io::stdin().lock().read_line(&mut l)?;
                l.trim().to_string()
            } else {
                rpassword::prompt_password(format!("{field} for {} (hidden): ", secrets::site(domain)))?
            };
            secrets::set(domain, field, &value)?;
            eprintln!("stored {field} for {} in the macOS Keychain (service sam.browser)", secrets::site(domain));
        }
        // Sign in on a page with the saved login (deterministic, no model); prints what happened.
        Some("--login-check") => {
            let url = args.get(1).expect("url");
            let c = voice_agent::browser::cdp::Chrome::launch(&data, screen, voice_agent::browser::cdp::Visibility::Offscreen).await?;
            let tab = c.new_tab().await?;
            tab.goto(url, std::time::Duration::from_secs(30)).await?;
            let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let r = voice_agent::browser::login::sign_in(&tab, &host).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let page = tab.eval("document.body ? document.body.innerText.slice(0, 300) : ''").await.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            println!("{}", serde_json::json!({"site": secrets::site(&host), "saved_login": secrets::has_login(&host), "result": format!("{r:?}"), "page": page}));
            tab.close().await;
            c.shutdown().await;
        }
        // Email: account config + app password into the Keychain (service sam.email).
        Some("--email-setup") => {
            let ask = |q: &str, def: &str| -> anyhow::Result<String> {
                eprint!("{q}{}: ", if def.is_empty() { String::new() } else { format!(" [{def}]") });
                let mut l = String::new();
                std::io::stdin().lock().read_line(&mut l)?;
                Ok(if l.trim().is_empty() { def.to_string() } else { l.trim().to_string() })
            };
            let user = ask("Email address (login)", "")?;
            let (ih, ip, sh, sp) = voice_agent::email::Account::guess(&user);
            let acct = voice_agent::email::Account {
                from: ask("From (e.g. Name <you@example.com>)", &user)?,
                imap_host: ask("IMAP host", &ih)?,
                imap_port: ask("IMAP port", &ip.to_string())?.parse()?,
                smtp_host: ask("SMTP host", &sh)?,
                smtp_port: ask("SMTP port (465 TLS / 587 STARTTLS)", &sp.to_string())?.parse()?,
                user,
            };
            let pw = match std::env::var("EMAIL_PASSWORD") {
                Ok(p) if acct.imap_host == "localhost" => p, // lab test server only
                _ => rpassword::prompt_password("App password (hidden): ")?,
            };
            acct.save(&pw)?;
            eprintln!("saved to the macOS Keychain (service sam.email); testing…");
            let n = voice_agent::email::search(&voice_agent::email::Query { limit: Some(3), ..Default::default() }).await?;
            eprintln!("ok: login works, {} recent mails visible", n.len());
        }
        Some("--email-forget") => {
            let _ = secrets::kc_delete("sam.email", "config");
            let _ = secrets::kc_delete("sam.email", "password");
            eprintln!("email account removed from the Keychain");
        }
        // File access: grants are a user action; find/read only work inside grants (+ artifacts).
        Some("--grant") => {
            let f = files::Files::open(&data)?;
            let p = f.grant(std::path::Path::new(args.get(1).expect("path")), args.get(2).map(String::as_str).unwrap_or(""))?;
            eprintln!("granted read access to {}", p.display());
        }
        Some("--revoke") => {
            let f = files::Files::open(&data)?;
            eprintln!("revoked: {}", f.revoke(std::path::Path::new(args.get(1).expect("path")))?);
        }
        Some("--grants") => {
            for g in files::Files::open(&data)?.grants() {
                println!("{}  {}", g.path.display(), g.note);
            }
        }
        Some("--find") => {
            let words = args[1..].join(" ");
            let catalog = artifacts::Catalog::open(&data)?;
            for e in catalog.search(&words, 10)? {
                println!("[{}] {}  ({})", e.kind, e.title, e.path);
            }
            for p in files::Files::open(&data)?.find(&words, 20) {
                println!("[file] {p}");
            }
        }
        Some("--read") => {
            let f = files::Files::open(&data)?;
            let off = args.get(2).and_then(|o| o.parse().ok()).unwrap_or(0);
            let c = f.read_doc(std::path::Path::new(args.get(1).expect("path")), off, 6000).await?;
            println!("{}", serde_json::to_string_pretty(&c)?);
        }
        Some("--show") => {
            let chrome = Chrome::launch(&data, screen, Visibility::Visible).await?;
            let tab = chrome.new_tab().await?;
            if let Some(u) = args.get(1) {
                tab.goto(u, std::time::Duration::from_secs(20)).await?;
            }
            eprintln!("Screen {screen} is open. Log in / solve the check in the window, then press Enter here.");
            let mut l = String::new();
            std::io::stdin().lock().read_line(&mut l)?;
            let n = chrome.save_cookies(screen).await?;
            eprintln!("saved {n} cookies for screen {screen}; future runs reuse this session");
            chrome.shutdown().await;
        }
        Some(goal) => {
            unsafe { std::env::set_var("MEMORY", "0") };
            let app = AppState::from_env().await?;
            let vis = if env_num("HEADLESS", 0u8) == 1 { Visibility::Headless } else { Visibility::Offscreen };
            let chrome = Chrome::launch(&data, screen, vis).await?;
            let tab = chrome.new_tab().await?;
            let _ = tab.block_heavy().await;
            let opt = agent::Options {
                max_steps: env_num("MAX_STEPS", 12),
                model: Provider::parse(&env_or("AGENT_MODEL", "")).unwrap_or(app.cfg.provider),
                mind_url: env_or("MIND_URL", "http://127.0.0.1:47802"),
                gate: env_num("GATE", 0u8) == 1,
                ..Default::default()
            };
            let out = agent::run(&app, &tab, goal, args.get(1).map(String::as_str), &opt).await?;
            println!("{}", serde_json::to_string(&out)?);
            tab.close().await;
            let _ = chrome.save_cookies(screen).await;
            if env_num("KEEP_BROWSER", 0u8) == 0 {
                chrome.shutdown().await;
            }
        }
        None => eprintln!("usage: agent \"<goal>\" [start-url] | --login-add <domain> <field> | --login-check <url> | --show [url]"),
    }
    Ok(())
}
