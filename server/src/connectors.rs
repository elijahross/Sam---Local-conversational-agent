//! Connectors: every integration Sam can use, with its status, its tools and how to set it up,
//! for the app's Connectors window (`/api/connectors`). Built-in local toolkits (Calendar,
//! Reminders, Contacts… — the Composio-style catalogue, run on this Mac), email, chat and web
//! sessions in Sam's browser, website logins (Keychain vault), file grants, and MCP servers
//! (configured ones plus one-click templates). Secrets go to the Keychain, never into files.
//! Turning a connector off hides its tools from the planner (`disabled_for`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};

fn state_file() -> PathBuf {
    crate::thinker::data_dir().join("connectors.json")
}

/// connectors.json (settings like the notes backend), for other modules.
pub fn state() -> Value {
    load_state()
}

fn load_state() -> Value {
    std::fs::read_to_string(state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}

fn save_state(v: &Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(crate::thinker::data_dir())?;
    std::fs::write(state_file(), serde_json::to_string_pretty(v)?)?;
    Ok(())
}

fn mcp_path() -> PathBuf {
    crate::config::resources_dir().join("mcp.json")
}

/// Built-in connectors: (id, name, SF Symbol, category, description, tools).
const BUILTIN: &[(&str, &str, &str, &str, &str, &[&str])] = &[
    ("calendar", "Calendar", "calendar", "This Mac", "Your macOS calendars (iCloud, Google, Exchange accounts added to the Mac): read, check conflicts, add events after your OK.", &["calendar_events", "calendar_create"]),
    ("reminders", "Reminders", "checklist", "This Mac", "macOS Reminders: to-dos and reminders — list, add and mark as done.", &["reminders_list", "reminder_create", "reminder_complete"]),
    ("contacts", "Contacts", "person.crop.circle", "This Mac", "Look up addresses in your Contacts (used to address emails).", &["contacts_lookup"]),
    ("weather", "Weather", "cloud.sun", "This Mac", "Open-Meteo forecasts, no account needed.", &["weather"]),
    ("youtube", "YouTube", "play.rectangle", "Music", "\"Play some jazz\": the first matching video opens in your browser. No account.", &["music_play"]),
    ("spotify", "Spotify", "music.note", "Music", "Plays in the Spotify app (pause, next by voice too). Paste a Spotify developer app's client id and secret once so Sam can search.", &[]),
    ("mail", "Email", "envelope", "Email & messaging", "Search, read and reply to email: IMAP/SMTP with an app password, or your Mail.app accounts for sending.", &["email_search", "email_read", "email_send"]),
    ("whatsapp", "WhatsApp", "bubble.left.and.bubble.right", "Email & messaging", "Read chats and reply in Sam's own WhatsApp Web session (link once with a QR code).", &["chat_send", "chat_read"]),
    ("telegram", "Telegram", "paperplane", "Email & messaging", "Read chats and reply in Sam's own Telegram Web session (link once with a QR code).", &[]),
    ("slack", "Slack", "number", "Email & messaging", "Read channels and DMs and reply in Sam's own Slack session (sign in once; the Slack app itself can't be read in the background).", &[]),
    ("reddit", "Reddit", "bubble.left.and.text.bubble.right", "Web", "What people say: threads and comments for pains, opinions and idea validation, read in Sam's own Reddit session (sign in once).", &[]),
    ("linkedin", "LinkedIn", "person.2", "Web", "Lead monitors on LinkedIn searches (people, jobs, companies), read in Sam's own LinkedIn session (sign in once). Read-only, a few checks a day.", &[]),
    ("gmail", "Gmail (web)", "envelope.badge", "Email & messaging", "Reply in Gmail threads in Sam's browser when the account isn't in Mail.app (sign in once).", &[]),
    ("web", "Web research & browsing", "globe", "Web", "Search, read pages, collect lists and do website tasks (comments, forms, checkout) in Sam's off-screen browser.", &["research", "scrape", "details", "web_fetch", "browse", "web_task"]),
    ("logins", "Website logins", "key", "Web", "Logins Sam may use in its browser, stored in the macOS Keychain (password and optional 2FA seed).", &[]),
    ("files", "Files", "folder", "Files", "Folders Sam may read (documents, PDFs, spreadsheets). Pointed-at files are always allowed for that one request.", &["find_files", "read_file", "list_files", "doc_review"]),
    ("papers", "Academic papers", "books.vertical", "Web", "Related-paper search on OpenAlex and arXiv, ranked locally.", &["papers_similar"]),
    ("notes", "Notes & journal", "note.text", "Files", "Notes for later and a daily journal in Apple Notes or your Obsidian vault (only adds, never deletes); generated documents go to Sam's folder.", &["note_save", "journal_add", "notes_find", "code"]),
    ("computer", "Computer use", "cursorarrow.motionlines", "This Mac", "Let Sam operate Mac apps in the background (Calculator, TextEdit, Pages…) through the Sam Computer Use helper. Every goal asks you first; Terminal and Keychain are off-limits.", &["computer"]),
];

/// One-click MCP integrations (Composio-like toolkits): (id, name, icon, description, command,
/// args, required env vars).
const MCP_TEMPLATES: &[(&str, &str, &str, &str, &str, &[&str], &[&str])] = &[
    ("github", "GitHub", "chevron.left.forwardslash.chevron.right", "Issues, pull requests and repositories.", "npx", &["-y", "@modelcontextprotocol/server-github"], &["GITHUB_PERSONAL_ACCESS_TOKEN"]),
    ("notion", "Notion", "doc.richtext", "Search and edit pages and databases in your workspace.", "npx", &["-y", "@notionhq/notion-mcp-server"], &["NOTION_TOKEN"]),
    ("slack", "Slack", "number", "Read channels and post messages (posting asks first).", "npx", &["-y", "@modelcontextprotocol/server-slack"], &["SLACK_BOT_TOKEN", "SLACK_TEAM_ID"]),
    ("linear", "Linear", "line.3.horizontal.decrease.circle", "Issues and projects (signs in through your browser).", "npx", &["-y", "mcp-remote", "https://mcp.linear.app/sse"], &[]),
    ("brave-search", "Brave Search", "magnifyingglass", "Web search with your Brave Search API key.", "npx", &["-y", "@modelcontextprotocol/server-brave-search"], &["BRAVE_API_KEY"]),
    ("fetch", "Fetch", "arrow.down.doc", "Fetch any URL as markdown.", "uvx", &["mcp-server-fetch"], &[]),
];

/// The connector a disabled tool belongs to (Registry marks those tools unavailable).
pub fn disabled_for(tool: &str) -> Option<&'static str> {
    if tool == "computer" && !crate::computer::enabled() {
        return Some("Computer use");
    }
    let st = load_state();
    let off: Vec<&str> = st["disabled"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    BUILTIN.iter().find(|c| off.contains(&c.0) && c.5.contains(&tool)).map(|c| c.1)
}

/// A browser session was established (QR link / sign-in done): remembered for the status.
/// The user's own phone number (for "message myself"), set in Connectors or learned from their
/// pointed-at self-chat.
pub fn set_chat_via(site: &str, via: &str) {
    let mut st = load_state();
    st["chat_via"][site] = json!(via);
    let _ = save_state(&st);
}

pub fn set_my_phone(digits: &str) {
    let mut st = load_state();
    st["my_phone"] = json!(digits);
    let _ = save_state(&st);
}

pub fn mark_linked(id: &str) {
    let mut st = load_state();
    st["linked"][id] = json!(chrono::Local::now().to_rfc3339());
    let _ = save_state(&st);
}

fn mcp_cfg() -> Value {
    std::fs::read_to_string(mcp_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({"servers": {}}))
}

fn mcp_tools_count(name: &str) -> Option<usize> {
    let p = crate::thinker::data_dir().join("mcp").join(format!("{name}.json"));
    serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(p).ok()?).ok().map(|v| v.len())
}

/// Everything for the Connectors window.
pub fn list(files: &crate::files::Files) -> Value {
    let st = load_state();
    let off: Vec<String> = st["disabled"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let mut out = Vec::new();
    for (id, name, icon, cat, desc, tools) in BUILTIN {
        let disabled = off.iter().any(|o| o == id);
        let (status, detail, actions): (&str, String, Vec<&str>) = match *id {
            "mail" => match crate::email::Account::load() {
                Some((a, _)) => ("connected", format!("IMAP: {}", a.user), vec!["disconnect"]),
                None if crate::apps::mail_app_ready() => ("connected", "Mail.app accounts (sending); add IMAP to search and read".into(), vec!["setup"]),
                None => ("setup", "Not set up".into(), vec!["setup"]),
            },
            "whatsapp" | "telegram" | "slack" if crate::native_chat::mode(id) == "app" => {
                ("connected", "Using your installed app".into(), vec!["chat_via"])
            }
            "whatsapp" | "telegram" | "slack" | "gmail" | "reddit" | "linkedin" => {
                let via = if matches!(*id, "gmail" | "reddit" | "linkedin") { vec![] } else { vec!["chat_via"] };
                let dom = match *id { "reddit" => "reddit.com", "linkedin" => "linkedin.com", "gmail" => "google.com", "whatsapp" => "whatsapp.com", "telegram" => "telegram.org", "slack" => "slack.com", _ => "" };
                let hint = if !dom.is_empty() && crate::secrets::has_login(dom) { " · saved login" } else { "" };
                match st["linked"][id].as_str() {
                    Some(t) => ("connected", format!("Sam's browser · linked {}{hint}", t.get(..10).unwrap_or(t)), [via, vec!["link"]].concat()),
                    None => ("setup", format!("Sam's browser · not linked yet{hint}"), [via, vec!["link"]].concat()),
                }
            }
            "logins" => {
                let n = st["logins"].as_array().map(|a| a.len()).unwrap_or(0);
                (if n > 0 { "connected" } else { "available" }, format!("{n} saved"), vec!["add_login", "browser_login"])
            }
            "files" => {
                let g = files.grants();
                (if g.is_empty() { "available" } else { "connected" }, format!("{} folder(s)", g.len()), vec!["add_folder"])
            }
            "notes" => match crate::notes::backend() {
                crate::notes::Backend::Apple => ("connected", "Apple Notes (folders Sam, Journal)".into(), vec!["notes_backend"]),
                crate::notes::Backend::Obsidian(v) => ("connected", format!("Obsidian: {}", v.display()), vec!["notes_backend"]),
            },
            "computer" => if crate::computer::enabled() { ("connected", "On — asks before every goal".into(), vec!["computer_permissions"]) } else { ("off", "Off".into(), vec!["computer_permissions"]) },
            _ => ("connected", "Built in".into(), vec![]),
        };
        let extra = match *id {
            "files" => json!(files.grants().iter().map(|g| json!({"label": g.path.display().to_string(), "remove": "revoke"})).collect::<Vec<_>>()),
            "logins" => json!(st["logins"].as_array().cloned().unwrap_or_default().iter().map(|d| json!({"label": d, "remove": "remove_login"})).collect::<Vec<_>>()),
            _ => json!([]),
        };
        let app_option = match *id {
            "whatsapp" | "telegram" | "slack" => json!({"available": crate::native_chat::app_for(id).is_some(), "via": crate::native_chat::mode(id),
                "note": match *id { "whatsapp" => "", "telegram" => "The Telegram app can't be operated in the background (it exposes nothing to Accessibility).", _ => "The Slack app can't be operated in the background (it exposes nothing to Accessibility)." }}),
            _ => Value::Null,
        };
        out.push(json!({"id": id, "name": name, "icon": icon, "category": cat, "description": desc, "tools": tools, "app_option": app_option,
            "status": if disabled { "off" } else { status }, "detail": detail, "actions": actions, "items": extra, "can_disable": !tools.is_empty(), "kind": "builtin"}));
    }
    let cfg = mcp_cfg();
    for (name, sc) in cfg["servers"].as_object().into_iter().flatten() {
        let enabled = sc["enabled"].as_bool().unwrap_or(true);
        let tpl = MCP_TEMPLATES.iter().find(|t| t.0 == name);
        out.push(json!({"id": format!("mcp:{name}"), "name": tpl.map(|t| t.1.to_string()).unwrap_or_else(|| name.clone()), "icon": tpl.map(|t| t.2).unwrap_or("puzzlepiece.extension"),
            "category": "Integrations (MCP)", "description": tpl.map(|t| t.3.to_string()).unwrap_or_else(|| format!("{} {}", sc["command"].as_str().unwrap_or(""), sc["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(" "))),
            "tools": [], "status": if enabled { "connected" } else { "off" },
            "detail": mcp_tools_count(name).map(|n| format!("{n} tools")).unwrap_or_else(|| "tools load on first use".into()),
            "actions": ["remove"], "items": [], "can_disable": true, "kind": "mcp"}));
    }
    let catalog: Vec<Value> = MCP_TEMPLATES.iter().filter(|t| cfg["servers"].get(t.0).is_none()).map(|(id, name, icon, desc, cmd, args, env)| {
        json!({"id": id, "name": name, "icon": icon, "description": desc, "command": cmd, "args": args, "env": env})
    }).collect();
    json!({"connectors": out, "catalog": catalog})
}

fn write_mcp(cfg: &Value) -> anyhow::Result<()> {
    std::fs::write(mcp_path(), serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Stores env secrets in the Keychain and returns the mcp.json env map with references.
fn secret_env(server: &str, values: &Value, names: &[String]) -> anyhow::Result<Value> {
    let mut env = serde_json::Map::new();
    for n in names {
        let v = values[n].as_str().unwrap_or("").trim();
        if v.is_empty() {
            bail!("{n} is required");
        }
        let account = format!("{server}/{n}");
        crate::secrets::kc_set("sam.mcp", &account, v)?;
        env.insert(n.clone(), json!(format!("keychain:sam.mcp/{account}")));
    }
    Ok(Value::Object(env))
}

/// Runs a connector action from the app. Returns a short message for the UI.
pub async fn action(app: &crate::AppState, files: &crate::files::Files, id: &str, action: &str, v: &Value) -> anyhow::Result<String> {
    let mut st = load_state();
    match action {
        "enable" | "disable" if id == "computer" => {
            crate::computer::set_enabled(action == "enable")?;
            Ok(format!("{}d", action))
        }
        "my_phone" => {
            let d = crate::native_chat::digits(v["phone"].as_str().unwrap_or("")).context("enter your number with country code, e.g. +43 660 1234567")?;
            set_my_phone(&d);
            Ok("saved".into())
        }
        "chat_via" => {
            let via = v["via"].as_str().unwrap_or("app");
            crate::native_chat::set_mode(id, via)?;
            Ok(if via == "app" { "using your installed app".into() } else { "using Sam's browser — link it with Link now".to_string() })
        }
        "computer_permissions" => {
            let r = app.http.post("http://127.0.0.1:47803/request_access").header("X-Sam-Token", crate::localauth::token()).json(&json!({})).timeout(std::time::Duration::from_secs(10)).send().await;
            match r {
                Ok(r) => Ok(format!("helper: {}", r.text().await.unwrap_or_default())),
                Err(_) => bail!("the Sam Computer Use helper isn't running"),
            }
        }
        "notes_backend" => {
            match v["backend"].as_str().unwrap_or("apple") {
                "obsidian" => {
                    let vault = v["vault"].as_str().unwrap_or("").trim().to_string();
                    let p = std::path::Path::new(&vault);
                    if !p.is_dir() {
                        bail!("choose your Obsidian vault folder");
                    }
                    let real = p.canonicalize()?;
                    let home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default();
                    if crate::files::is_denied(&real) || real == std::path::Path::new("/") || real == home.canonicalize().unwrap_or_default() {
                        bail!("that folder is protected; choose the vault folder itself");
                    }
                    st["notes"] = json!({"backend": "obsidian", "vault": vault});
                }
                _ => st["notes"] = json!({"backend": "apple"}),
            }
            save_state(&st)?;
            Ok("saved".into())
        }
        "enable" | "disable" => {
            if let Some(name) = id.strip_prefix("mcp:") {
                let mut cfg = mcp_cfg();
                let s = cfg["servers"].get_mut(name).context("no such integration")?;
                s["enabled"] = json!(action == "enable");
                write_mcp(&cfg)?;
            } else {
                let mut off: Vec<String> = st["disabled"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
                off.retain(|o| o != id);
                if action == "disable" {
                    off.push(id.to_string());
                }
                st["disabled"] = json!(off);
                save_state(&st)?;
            }
            Ok(format!("{}d", action))
        }
        "remove" => {
            let name = id.strip_prefix("mcp:").context("only integrations can be removed")?;
            let mut cfg = mcp_cfg();
            if let Some(s) = cfg["servers"].as_object_mut().and_then(|m| m.remove(name)) {
                for (_, val) in s["env"].as_object().into_iter().flatten() {
                    if let Some((svc, acct)) = val.as_str().and_then(|x| x.strip_prefix("keychain:")).and_then(|x| x.split_once('/')) {
                        let _ = crate::secrets::kc_delete(svc, acct);
                    }
                }
            }
            write_mcp(&cfg)?;
            let _ = std::fs::remove_file(crate::thinker::data_dir().join("mcp").join(format!("{name}.json")));
            Ok("removed".into())
        }
        "add_mcp" => {
            // From a template (id = template id) or custom (values: name, command, args, env names).
            let (name, cmd, args, env_names): (String, String, Vec<String>, Vec<String>) = match MCP_TEMPLATES.iter().find(|t| t.0 == id) {
                Some(t) => (t.0.into(), t.4.into(), t.5.iter().map(|s| s.to_string()).collect(), t.6.iter().map(|s| s.to_string()).collect()),
                None => {
                    let name = v["name"].as_str().unwrap_or("").trim().to_lowercase().replace(' ', "-");
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                        bail!("give the integration a short name (letters, digits, -)");
                    }
                    let cmd = v["command"].as_str().unwrap_or("").trim().to_string();
                    if cmd.is_empty() {
                        bail!("command is required (e.g. npx)");
                    }
                    let args = v["args"].as_str().unwrap_or("").split_whitespace().map(str::to_string).collect();
                    let env = v["env_names"].as_str().unwrap_or("").split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_string).collect();
                    (name, cmd, args, env)
                }
            };
            let env = secret_env(&name, &v["env"], &env_names)?;
            let mut cfg = mcp_cfg();
            // Every tool of a new integration asks before it runs, until marked read-only.
            cfg["servers"][&name] = json!({"command": cmd, "args": args, "env": env, "read_tools": [], "enabled": true});
            write_mcp(&cfg)?;
            // List its tools now so problems show up here, not in the middle of a task.
            let sc: crate::mcp::ServerCfg = serde_json::from_value(cfg["servers"][&name].clone())?;
            match crate::tools::list_server(&name, &sc).await {
                Ok(tools) => {
                    let dir = crate::thinker::data_dir().join("mcp");
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&tools)?)?;
                    Ok(format!("added, {} tools", tools.len()))
                }
                Err(e) => Ok(format!("added, but it didn't start yet: {}", format!("{e:#}").chars().take(160).collect::<String>())),
            }
        }
        "setup" if id == "mail" => {
            let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
            let user = s("user");
            if !user.contains('@') || s("password").is_empty() {
                bail!("email address and app password are required");
            }
            let domain = user.split('@').nth(1).unwrap_or("");
            let guess = |p: &str| format!("{p}.{domain}");
            let a = crate::email::Account {
                user: user.clone(),
                from: user.clone(),
                imap_host: if s("imap_host").is_empty() { guess("imap") } else { s("imap_host") },
                imap_port: 993,
                smtp_host: if s("smtp_host").is_empty() { guess("smtp") } else { s("smtp_host") },
                smtp_port: v["smtp_port"].as_u64().unwrap_or(465) as u16,
            };
            a.save(&s("password"))?;
            // Check the login right away.
            match crate::email::search(&crate::email::Query { limit: Some(1), ..Default::default() }).await {
                Ok(_) => Ok("connected".into()),
                Err(e) => Ok(format!("saved, but the login check failed: {}", format!("{e:#}").chars().take(160).collect::<String>())),
            }
        }
        "disconnect" if id == "mail" => {
            let _ = crate::secrets::kc_delete("sam.email", "config");
            let _ = crate::secrets::kc_delete("sam.email", "password");
            Ok("disconnected".into())
        }
        "link" => {
            // Show Sam's browser at the site; the user links / signs in; status flips when ready.
            let site = id.to_string();
            let url = match id {
                "gmail" => crate::env_or("GMAIL_URL", "https://mail.google.com/mail/u/0/"),
                "reddit" => crate::browser::reddit::HOME.to_string(),
                "linkedin" => crate::browser::linkedin::HOME.to_string(),
                s => crate::browser::chat::site_url(s).context("unknown site")?,
            };
            let data = crate::thinker::data_dir();
            let screen = crate::thinker::accounts_screen();
            let c = crate::browser::cdp::Chrome::launch(&data, screen, crate::browser::cdp::Visibility::Offscreen).await?;
            let tab = c.new_tab().await?;
            tab.goto(&url, std::time::Duration::from_secs(30)).await?;
            let _ = app;
            let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let saved = crate::secrets::has_login(&host);
            let site2 = site.clone();
            // The check runs on; the answer below waits a bounded time for it. Sites with a
            // saved login (Website logins) are signed in without the user; otherwise Sam's
            // browser window shows the sign-in and the status flips once it's done.
            let mut handle = tokio::spawn(async move {
                let r: anyhow::Result<String> = match site2.as_str() {
                    "gmail" => crate::browser::gmail::ready(&tab, None).await.map(|_| "linked".to_string()),
                    "reddit" => crate::browser::reddit::ready(&tab, None).await.map(|u| format!("u/{u}")),
                    "linkedin" => crate::browser::linkedin::ready(&tab, None).await.map(|_| "linked".to_string()),
                    _ => crate::browser::chat::ready(&tab, &site2, None).await.map(|_| "linked".to_string()),
                };
                if r.is_ok() {
                    mark_linked(&site2);
                }
                let _ = c.save_cookies(screen).await;
                tab.close().await;
                c.shutdown().await;
                r
            });
            match tokio::time::timeout(std::time::Duration::from_secs(25), &mut handle).await {
                Ok(Ok(Ok(who))) => Ok(format!("Linked ({who}){}", if saved { " — signed in with your saved login" } else { "" })),
                Ok(Ok(Err(e))) => Ok(format!("Not linked: {}", format!("{e:#}").chars().take(160).collect::<String>())),
                Ok(Err(_)) => Ok("Not linked: the check stopped unexpectedly".into()),
                Err(_) => Ok(if saved {
                    "Signing in with your saved login… if the site asks for more, Sam's browser window shows it. The status flips when linked.".into()
                } else {
                    "Not signed in yet: sign in in Sam's browser window (or save a login for this site under Website logins). The status flips when linked.".into()
                }),
            }
        }
        "add_login" => {
            let d = v["domain"].as_str().unwrap_or("").trim().to_lowercase();
            let host = d.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or("").to_string();
            if host.is_empty() || v["password"].as_str().unwrap_or("").is_empty() {
                bail!("website and password are required");
            }
            for f in ["username", "password", "totp"] {
                if let Some(val) = v[f].as_str().filter(|x| !x.trim().is_empty()) {
                    crate::secrets::set(&host, f, val.trim())?;
                }
            }
            let mut logins: Vec<String> = st["logins"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
            let site = crate::secrets::site(&host);
            if !logins.contains(&site) {
                logins.push(site);
            }
            st["logins"] = json!(logins);
            save_state(&st)?;
            Ok("saved to the Keychain".into())
        }
        "remove_login" => {
            let d = v["item"].as_str().unwrap_or("");
            for f in ["username", "password", "totp"] {
                let _ = crate::secrets::delete(d, f);
            }
            let logins: Vec<Value> = st["logins"].as_array().cloned().unwrap_or_default().into_iter().filter(|x| x.as_str() != Some(d)).collect();
            st["logins"] = json!(logins);
            save_state(&st)?;
            Ok("removed".into())
        }
        "browser_login" => {
            // "example.com" or a pasted URL; only web pages.
            let raw = v["url"].as_str().unwrap_or("").trim();
            let url = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
            match url::Url::parse(&url) {
                Ok(u) if (u.scheme() == "https" || u.scheme() == "http") && u.host_str().is_some_and(|h| h.contains('.') || h == "localhost") => {}
                _ => bail!("enter the site's address, e.g. example.com"),
            }
            let data = crate::thinker::data_dir();
            let screen = crate::env_num("BG_SCREEN", 1u16);
            let c = crate::browser::cdp::Chrome::launch(&data, screen, crate::browser::cdp::Visibility::Offscreen).await?;
            let tab = c.new_tab().await?;
            tab.goto(&url, std::time::Duration::from_secs(30)).await?;
            tab.show_window(true).await;
            // When the user closes the window (or after 20 min) the session is saved and this
            // browser released, so it never lingers in the background.
            tokio::spawn(async move {
                let end = std::time::Instant::now() + std::time::Duration::from_secs(20 * 60);
                while std::time::Instant::now() < end && tab.eval("1").await.is_ok() {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                let _ = c.save_cookies(screen).await;
                tab.close().await;
                c.shutdown().await;
            });
            Ok("Log in in Sam's browser window, then close it; Sam keeps the session".into())
        }
        // Folder grants: the path comes from the app's folder picker (the user's own choice).
        "add_folder" => {
            let p = v["path"].as_str().unwrap_or("");
            files.grant(Path::new(p), "added in Connectors")?;
            Ok("folder added".into())
        }
        "revoke" => {
            files.revoke(Path::new(v["item"].as_str().unwrap_or("")))?;
            Ok("removed".into())
        }
        _ => bail!("unknown action {action} for {id}"),
    }
}
