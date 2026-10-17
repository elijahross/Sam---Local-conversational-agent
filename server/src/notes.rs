//! Notes and journal: "save a note for later", "add to my journal / daily note", "find my notes
//! about …". Two backends, chosen in Connectors:
//! - Apple Notes (default): folder "Sam" for notes, folder "Journal" with one note per day.
//! - Obsidian (a vault folder the user picked): `Inbox/<title>.md`, `Daily/YYYY-MM-DD.md`.
//!
//! Additive only: new notes and appended journal lines. Nothing is ever deleted or overwritten,
//! and Obsidian writes stay inside the vault folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};

pub enum Backend {
    Apple,
    Obsidian(PathBuf),
}

/// The configured backend (connectors.json `notes`), Apple Notes by default.
pub fn backend() -> Backend {
    let st = crate::connectors::state();
    match (st["notes"]["backend"].as_str(), st["notes"]["vault"].as_str()) {
        (Some("obsidian"), Some(v)) if Path::new(v).is_dir() => Backend::Obsidian(PathBuf::from(v)),
        _ => Backend::Apple,
    }
}

fn slug(t: &str) -> String {
    let s: String = t.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { ' ' }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() { "Note".into() } else { s.chars().take(80).collect() }
}

/// A path inside the vault, never outside it (no `..`, no absolute escapes).
fn inside(vault: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let p = vault.join(rel);
    let parent = p.parent().context("path")?;
    std::fs::create_dir_all(parent)?;
    let real_parent = parent.canonicalize()?;
    if !real_parent.starts_with(vault.canonicalize()?) {
        bail!("refusing to write outside the vault");
    }
    Ok(real_parent.join(p.file_name().context("name")?))
}

// Apple Notes through JXA (argv only, never interpolated). HTML bodies; lines become divs.
const NOTE_CREATE: &str = r#"function run(argv) {
  const [folderName, title, html] = argv; const N = Application('Notes');
  let f = N.folders.whose({name: folderName})();
  f = f.length ? f[0] : N.make({new: 'folder', withProperties: {name: folderName}});
  const n = N.make({new: 'note', at: f, withProperties: {name: title, body: html}});
  return JSON.stringify({created: n.name(), folder: folderName, id: n.id()});
}"#;

const JOURNAL_APPEND: &str = r#"function run(argv) {
  const [folderName, title, lineHtml] = argv; const N = Application('Notes');
  let f = N.folders.whose({name: folderName})();
  f = f.length ? f[0] : N.make({new: 'folder', withProperties: {name: folderName}});
  const ex = f.notes.whose({name: title})();
  if (ex.length) { ex[0].body = ex[0].body() + lineHtml; return JSON.stringify({appended: title, id: ex[0].id()}); }
  const n = N.make({new: 'note', at: f, withProperties: {name: title, body: lineHtml}});
  return JSON.stringify({created: title, id: n.id()});
}"#;

const NOTES_FIND: &str = r#"function run(argv) {
  const [q] = argv; const N = Application('Notes'); const out = [];
  const hits = N.notes.whose({_or: [{name: {_contains: q}}, {plaintext: {_contains: q}}]})();
  for (const n of hits.slice(0, 10)) {
    const t = n.plaintext(); const i = Math.max(0, t.toLowerCase().indexOf(q.toLowerCase()) - 60);
    out.push({title: n.name(), folder: n.container().name(), modified: n.modificationDate().toISOString().slice(0, 10), snippet: t.slice(i, i + 200).replace(/\s+/g, ' ')});
  }
  return JSON.stringify(out);
}"#;

fn html(text: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    text.lines().map(|l| if l.trim().is_empty() { "<div><br></div>".to_string() } else { format!("<div>{}</div>", esc(l)) }).collect()
}

/// Saves a note for later. Returns a short human result.
pub async fn save(title: &str, text: &str) -> anyhow::Result<String> {
    let title = slug(title);
    match backend() {
        Backend::Apple => {
            crate::apps::real_writes_ok()?;
            let v = crate::apps::jxa_pub(NOTE_CREATE, &["Sam".into(), title.clone(), html(text)]).await?;
            Ok(format!("note \"{}\" saved in Apple Notes › Sam", v["created"].as_str().unwrap_or(&title)))
        }
        Backend::Obsidian(vault) => {
            // A new file every time (a clash gets " 2", " 3"…): never overwrite.
            let mut p = inside(&vault, &format!("Inbox/{title}.md"))?;
            let mut n = 2;
            while p.exists() {
                p = inside(&vault, &format!("Inbox/{title} {n}.md"))?;
                n += 1;
            }
            let now = chrono::Local::now();
            std::fs::write(&p, format!("---\ncreated: {}\nsource: sam\n---\n# {title}\n\n{}\n", now.format("%Y-%m-%d %H:%M"), text.trim()))?;
            Ok(format!("note saved: {}", p.display()))
        }
    }
}

/// Adds a timestamped line to the daily note (`date` = YYYY-MM-DD, default today).
pub async fn journal(text: &str, date: Option<chrono::NaiveDate>) -> anyhow::Result<String> {
    let now = chrono::Local::now();
    let day = date.unwrap_or_else(|| now.date_naive());
    let line = text.trim().replace('\n', " ");
    if line.is_empty() {
        bail!("nothing to add to the journal");
    }
    let stamp = if day == now.date_naive() { now.format("%H:%M").to_string() } else { "added later".into() };
    match backend() {
        Backend::Apple => {
            crate::apps::real_writes_ok()?;
            let title = format!("Journal {}", day.format("%Y-%m-%d (%a)"));
            let v = crate::apps::jxa_pub(JOURNAL_APPEND, &["Journal".into(), title.clone(), format!("<div>• {stamp} — {}</div>", html(&line).trim_start_matches("<div>").trim_end_matches("</div>"))]).await?;
            Ok(format!("{} journal note \"{title}\" in Apple Notes › Journal", if v.get("created").is_some() { "started" } else { "added to" }))
        }
        Backend::Obsidian(vault) => {
            let p = inside(&vault, &format!("Daily/{}.md", day.format("%Y-%m-%d")))?;
            use std::io::Write;
            let new = !p.exists();
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;   // append-only
            if new {
                writeln!(f, "# {}\n", day.format("%A, %-d %B %Y"))?;
            }
            writeln!(f, "- {stamp} {line}")?;
            Ok(format!("added to {}", p.display()))
        }
    }
}

/// Finds notes by words (titles and text). Read-only.
pub async fn find(query: &str) -> anyhow::Result<Value> {
    let q = query.trim();
    if q.len() < 2 {
        bail!("what should I look for?");
    }
    match backend() {
        Backend::Apple => crate::apps::jxa_pub(NOTES_FIND, &[q.to_string()]).await,
        Backend::Obsidian(vault) => {
            let ql = q.to_lowercase();
            let mut out = Vec::new();
            let mut stack = vec![vault.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let p = e.path();
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if name.starts_with('.') {
                        continue;
                    }
                    if p.is_dir() {
                        stack.push(p);
                    } else if name.ends_with(".md") {
                        let t = std::fs::read_to_string(&p).unwrap_or_default();
                        let tl = t.to_lowercase();
                        if name.to_lowercase().contains(&ql) || tl.contains(&ql) {
                            let i = tl.find(&ql).unwrap_or(0).saturating_sub(60);
                            let snip: String = t.chars().skip(t[..i.min(t.len())].chars().count()).take(200).collect();
                            out.push(json!({"title": name.trim_end_matches(".md"), "path": p.display().to_string(), "snippet": snip.split_whitespace().collect::<Vec<_>>().join(" ")}));
                            if out.len() >= 10 {
                                return Ok(Value::Array(out));
                            }
                        }
                    }
                }
            }
            Ok(Value::Array(out))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn vault_confinement() {
        let v = std::env::temp_dir().join(format!("vault-{}", std::process::id()));
        std::fs::create_dir_all(&v).unwrap();
        assert!(super::inside(&v, "Inbox/a.md").is_ok());
        assert!(super::inside(&v, "../escape.md").is_err());
        assert!(super::inside(&v, "Inbox/../../escape.md").is_err());
        let _ = std::fs::remove_dir_all(&v);
    }
}

/// Live check against the real Apple Notes / Reminders (run explicitly:
/// `cargo test --release --lib live_apple -- --ignored`). Creates items marked "(delete me)" and
/// deletes exactly those afterwards; folders only if this test created them.
#[cfg(test)]
mod live_apple {
    const SNAP: &str = r#"function run(){const N=Application('Notes');return JSON.stringify({sam:N.folders.whose({name:'Sam'})().length,journal:N.folders.whose({name:'Journal'})().length})}"#;
    const CLEAN: &str = r#"function run(argv){const [dropSam,dropJournal]=argv;const N=Application('Notes');let n=0;
      for(const f of N.folders()){for(const x of f.notes()){if(x.plaintext().includes('(delete me)')&&x.plaintext().includes('sam-live-test')){N.delete(x);n++}}}
      for(const [name,drop] of [['Sam',dropSam],['Journal',dropJournal]]){if(drop==='1'){const f=N.folders.whose({name})();if(f.length&&f[0].notes().length===0)N.delete(f[0])}}
      const R=Application('Reminders');let r=0;for(const l of R.lists()){for(const x of l.reminders.whose({name:{_contains:'(delete me) sam-live-test'}})()){R.delete(x);r++}}
      return JSON.stringify({notes:n,reminders:r})}"#;
    const READ: &str = r#"function run(){const N=Application('Notes');const out=[];for(const f of N.folders()){for(const x of f.notes()){if(x.plaintext().includes('sam-live-test'))out.push({folder:f.name(),name:x.name(),text:x.plaintext()})}}
      const R=Application('Reminders');const rs=[];for(const l of R.lists()){for(const x of l.reminders.whose({name:{_contains:'sam-live-test'}})())rs.push({name:x.name(),done:x.completed()})}return JSON.stringify({notes:out,rem:rs})}"#;

    #[tokio::test]
    #[ignore]
    async fn live_apple() {
        let before = crate::apps::jxa_pub(SNAP, &[]).await.unwrap();
        let r = async {
            let a = super::save("Sam test (delete me)", "sam-live-test (delete me)\nsecond line").await?;
            let b = super::journal("sam-live-test (delete me) first entry", None).await?;
            let c = super::journal("sam-live-test (delete me) second entry", None).await?;
            let f = super::find("sam-live-test").await?;
            let d = crate::apps::reminder_create("(delete me) sam-live-test buy oat milk", "", "").await?;
            let e = crate::apps::reminder_complete("(delete me) sam-live-test buy oat milk", "").await?;
            let state = crate::apps::jxa_pub(READ, &[]).await?;
            anyhow::Ok((a, b, c, f, d, e, state))
        }
        .await;
        let drop = |k: &str| if before[k].as_i64() == Some(0) { "1".to_string() } else { "0".to_string() };
        let cleaned = crate::apps::jxa_pub(CLEAN, &[drop("sam"), drop("journal")]).await;
        let after = crate::apps::jxa_pub(SNAP, &[]).await.unwrap();
        println!("before {before}\nresult {r:#?}\ncleaned {cleaned:?}\nafter {after}");
        let (_, _, _, found, _, _, state) = r.unwrap();
        assert!(found.as_array().is_some_and(|a| !a.is_empty()));
        let notes = state["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2, "one note + one journal note");
        let j = notes.iter().find(|n| n["folder"] == "Journal").unwrap();
        assert!(j["text"].as_str().unwrap().contains("first entry") && j["text"].as_str().unwrap().contains("second entry"));
        assert_eq!(state["rem"][0]["done"], true);
        assert_eq!(before, after, "folders back to how they were");
    }
}
