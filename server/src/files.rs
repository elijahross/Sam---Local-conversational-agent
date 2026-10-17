//! Read-only file access for the agent, only where the user granted it.
//!
//! Safety model:
//! - Grants are added/removed ONLY by the user (CLI `agent --grant/--revoke`, later a UI
//!   confirmation). No agent tool can create a grant.
//! - Every path is canonicalized (symlinks and `..` resolved) before the check, so a link or
//!   traversal inside a granted folder cannot point outside it.
//! - A hard denylist (secrets, keys, keychains, browser profiles, .env) applies even inside
//!   grants.
//! - Read-only, size-capped, paginated reads; every access (allowed or denied) is audited.
//! - The agent's own artifacts dir is always readable.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const DENY: &[&str] = &[
    "/.ssh/", "/.aws/", "/.gnupg/", "/.kube/", "/.docker/", "/.config/gcloud/", "/Library/Keychains/", "/Library/Cookies/",
    "/Library/Application Support/Google/Chrome/", "/data/browser/profiles/", "/.password-store/", "/.netrc",
];
const DENY_NAMES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "credentials", ".npmrc", ".pypirc", "secrets.json", "keychain"];
const DENY_EXT: &[&str] = &["pem", "key", "p12", "pfx", "keychain-db", "kdbx", "sqlite-wal"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub path: PathBuf,
    pub granted_at: i64,
    pub note: String,
}

pub struct Files {
    grants_file: PathBuf,
    audit_file: PathBuf,
    own_root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    pub path: String,
    pub text: String,
    pub offset: usize,
    pub total_chars: usize,
    pub next_offset: Option<usize>,
}

impl Files {
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir.join("artifacts"))?;
        Ok(Self { grants_file: data_dir.join("file-grants.json"), audit_file: data_dir.join("file-access.log.jsonl"), own_root: data_dir.join("artifacts").canonicalize()? })
    }

    pub fn grants(&self) -> Vec<Grant> {
        std::fs::read_to_string(&self.grants_file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// User action only (CLI / UI confirmation). Never exposed as an agent tool.
    pub fn grant(&self, path: &Path, note: &str) -> anyhow::Result<PathBuf> {
        let p = path.canonicalize().with_context(|| format!("no such path: {}", path.display()))?;
        if denied(&p) {
            bail!("refusing to grant a protected location: {}", p.display());
        }
        let mut g = self.grants();
        g.retain(|x| x.path != p);
        g.push(Grant { path: p.clone(), granted_at: chrono::Local::now().timestamp(), note: note.into() });
        std::fs::write(&self.grants_file, serde_json::to_string_pretty(&g)?)?;
        Ok(p)
    }

    pub fn revoke(&self, path: &Path) -> anyhow::Result<bool> {
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut g = self.grants();
        let before = g.len();
        g.retain(|x| x.path != p);
        std::fs::write(&self.grants_file, serde_json::to_string_pretty(&g)?)?;
        Ok(g.len() != before)
    }

    /// The single gate every agent file operation goes through.
    pub fn check(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let res = (|| {
            let p = path.canonicalize().with_context(|| format!("not found: {}", path.display()))?;
            if denied(&p) {
                bail!("protected location");
            }
            let allowed = p.starts_with(&self.own_root) || self.grants().iter().any(|g| p.starts_with(&g.path));
            if !allowed {
                bail!("no access granted; ask the user to run: agent --grant \"{}\"", p.parent().unwrap_or(&p).display());
            }
            Ok(p)
        })();
        self.audit(path, res.as_ref().map(|_| "allowed").unwrap_or("denied"), res.as_ref().err().map(|e| e.to_string()));
        res
    }

    /// List a granted directory (names, sizes; no contents).
    pub fn list(&self, dir: &Path, limit: usize) -> anyhow::Result<Vec<(String, u64, bool)>> {
        let d = self.check(dir)?;
        let mut out = Vec::new();
        for e in std::fs::read_dir(&d)?.flatten().take(limit) {
            let p = e.path();
            if denied(&p) || e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let m = e.metadata()?;
            out.push((p.display().to_string(), m.len(), m.is_dir()));
        }
        Ok(out)
    }

    /// Find files by name inside granted roots (and own artifacts), bounded walk. Ranked by how
    /// many of the words appear in the name; generic words ("file", "spreadsheet", "my") are
    /// ignored, and at least one specific word must match.
    pub fn find(&self, words: &str, limit: usize) -> Vec<String> {
        const GENERIC: &[&str] = &["my", "the", "a", "file", "files", "document", "doc", "spreadsheet", "sheet", "pdf", "folder", "find", "in", "of"];
        let terms: Vec<String> = words.split(|c: char| !c.is_alphanumeric()).map(|w| w.to_lowercase()).filter(|w| w.len() > 1 && !GENERIC.contains(&w.as_str())).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(usize, String)> = Vec::new();
        let mut roots: Vec<PathBuf> = self.grants().into_iter().map(|g| g.path).collect();
        roots.push(self.own_root.clone());
        let mut stack: Vec<(PathBuf, usize)> = roots.into_iter().map(|r| (r, 0)).collect();
        let mut seen = 0;
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                seen += 1;
                if seen > 50_000 {
                    break;
                }
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_lowercase();
                if name.starts_with('.') || denied(&p) {
                    continue;
                }
                if p.is_dir() {
                    if depth < 8 && !p.is_symlink() {
                        stack.push((p, depth + 1));
                    }
                } else {
                    let score = terms.iter().filter(|t| name.contains(t.as_str())).count();
                    if score > 0 {
                        hits.push((score, p.display().to_string()));
                    }
                }
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
        hits.into_iter().take(limit).map(|h| h.1).collect()
    }

    /// Read a granted document (PDF/Office/text) as markdown, paginated by chars.
    pub async fn read_doc(&self, path: &Path, offset: usize, max_chars: usize) -> anyhow::Result<Chunk> {
        let p = self.check(path)?;
        let text = crate::parse::to_markdown(&p).await?;
        self.read(&p, offset, max_chars, |_| Ok(text.clone()))
    }

    /// Read a granted file as text via `parse`, paginated by chars.
    pub fn read(&self, path: &Path, offset: usize, max_chars: usize, parse: impl Fn(&Path) -> anyhow::Result<String>) -> anyhow::Result<Chunk> {
        let p = self.check(path)?;
        let meta = std::fs::metadata(&p)?;
        if meta.is_dir() {
            bail!("is a directory; use list");
        }
        if meta.len() > MAX_FILE_BYTES {
            bail!("file too large ({} MB)", meta.len() / 1_000_000);
        }
        let text = parse(&p)?;
        let total = text.chars().count();
        let piece: String = text.chars().skip(offset).take(max_chars).collect();
        let end = offset + piece.chars().count();
        Ok(Chunk { path: p.display().to_string(), text: piece, offset, total_chars: total, next_offset: (end < total).then_some(end) })
    }

    /// A read of the file the user pointed at (allowed for that request without a grant).
    pub fn audit_pointed(&self, path: &Path) {
        self.audit(path, "allowed", Some("pointed at by the user".into()));
    }

    fn audit(&self, path: &Path, verdict: &str, reason: Option<String>) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.audit_file) {
            let _ = writeln!(f, "{}", serde_json::json!({"ts": chrono::Local::now().to_rfc3339(), "path": path.display().to_string(), "verdict": verdict, "reason": reason}));
        }
    }
}

/// The denylist (keys, keychains, .env…), for other modules.
pub fn is_denied(p: &Path) -> bool {
    denied(p)
}

pub(crate) fn denied(p: &Path) -> bool {
    let s = format!("{}/", p.display());
    let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    DENY.iter().any(|d| s.contains(d)) || DENY_NAMES.iter().any(|n| name == *n || name.starts_with(&format!("{n}."))) || DENY_EXT.contains(&ext.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_symlinks_and_denylist() {
        let base = std::env::temp_dir().join(format!("files-test-{}", std::process::id()));
        let data = base.join("data");
        let docs = base.join("docs");
        let secret = base.join("outside");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(docs.join("notes.txt"), "hello").unwrap();
        std::fs::write(docs.join(".env"), "KEY=1").unwrap();
        std::fs::write(secret.join("private.txt"), "nope").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(secret.join("private.txt"), docs.join("link.txt")).unwrap();
        let f = Files::open(&data).unwrap();
        let read = |p: &Path| f.read(p, 0, 100, |x| Ok(std::fs::read_to_string(x)?));
        assert!(read(&docs.join("notes.txt")).is_err(), "no grant yet");
        f.grant(&docs, "test").unwrap();
        assert_eq!(read(&docs.join("notes.txt")).unwrap().text, "hello");
        assert!(read(&docs.join(".env")).is_err(), "denylist inside a grant");
        assert!(read(&docs.join("link.txt")).is_err(), "symlink escaping the grant");
        assert!(read(&docs.join("../outside/private.txt")).is_err(), "traversal");
        assert!(f.grant(&base.join("data/../docs/.env"), "x").is_err());
        f.revoke(&docs).unwrap();
        assert!(read(&docs.join("notes.txt")).is_err(), "revoked");
        let log = std::fs::read_to_string(data.join("file-access.log.jsonl")).unwrap();
        assert!(log.contains("denied") && log.contains("allowed"));
        let _ = std::fs::remove_dir_all(base);
    }
}
