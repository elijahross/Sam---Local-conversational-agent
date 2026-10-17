//! Email over IMAP (read) and SMTP (send) with an app password from the macOS Keychain.
//!
//! - Account config: Keychain service `sam.email`, account `config` (JSON, no secret) and
//!   `password` (the app password). Set up with `agent --email-setup`; never shown to a model.
//! - TLS: IMAPS (993) and SMTPS (465) / STARTTLS (587). Plaintext only to localhost (lab
//!   test server).
//! - Reads use EXAMINE (read-only): searching never marks mail as seen.
//! - Models only see capped snippets / bodies; mail text is untrusted data (see thinker).
//! - Sending is a `write` tool: drafted first, sent only after the user's confirmation.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskCx, Poll},
    time::Duration,
};

use anyhow::{Context, bail};
use futures_util::TryStreamExt;
use mail_parser::MimeHeaders;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

const SERVICE: &str = "sam.email";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub user: String,
    pub from: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
}

impl Account {
    pub fn load() -> Option<(Self, String)> {
        let cfg: Self = serde_json::from_str(&crate::secrets::kc_get(SERVICE, "config")?).ok()?;
        let pw = crate::secrets::kc_get(SERVICE, "password")?;
        Some((cfg, pw))
    }

    pub fn save(&self, password: &str) -> anyhow::Result<()> {
        crate::secrets::kc_set(SERVICE, "config", &serde_json::to_string(self)?)?;
        crate::secrets::kc_set(SERVICE, "password", password)
    }

    /// Well-known hosts for common domains (the user can override in setup).
    pub fn guess(user: &str) -> (String, u16, String, u16) {
        let domain = user.rsplit('@').next().unwrap_or("").to_lowercase();
        match domain.as_str() {
            "gmail.com" | "googlemail.com" => ("imap.gmail.com".into(), 993, "smtp.gmail.com".into(), 465),
            "outlook.com" | "hotmail.com" | "live.com" => ("outlook.office365.com".into(), 993, "smtp.office365.com".into(), 587),
            "icloud.com" | "me.com" => ("imap.mail.me.com".into(), 993, "smtp.mail.me.com".into(), 587),
            "gmx.at" | "gmx.net" | "gmx.de" => ("imap.gmx.net".into(), 993, "mail.gmx.net".into(), 465),
            d => (format!("imap.{d}"), 993, format!("smtp.{d}"), 465),
        }
    }

    fn local(host: &str) -> bool {
        host == "localhost" || host == "127.0.0.1"
    }
}

/// Plain TCP (localhost only) or TLS.
enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Conn::Plain(_) => "Plain",
            Conn::Tls(_) => "Tls",
        })
    }
}

impl AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskCx<'_>, b: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, b),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, b),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

type Session = async_imap::Session<Conn>;

async fn session(a: &Account, pw: &str) -> anyhow::Result<Session> {
    let tcp = tokio::time::timeout(TIMEOUT, TcpStream::connect((a.imap_host.as_str(), a.imap_port))).await.context("IMAP connect timed out")??;
    let conn = if Account::local(&a.imap_host) {
        Conn::Plain(tcp)
    } else {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        for c in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(c);
        }
        let cfg = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(a.imap_host.clone())?;
        Conn::Tls(Box::new(tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(name, tcp).await.context("IMAP TLS")?))
    };
    let mut client = async_imap::Client::new(conn);
    client.read_response().await?.context("no IMAP greeting")?;
    let s = tokio::time::timeout(TIMEOUT, client.login(&a.user, pw)).await.context("IMAP login timed out")?.map_err(|(e, _)| anyhow::anyhow!("IMAP login failed: {e}"))?;
    Ok(s)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub since_days: Option<u32>,
    #[serde(default)]
    pub unread: Option<bool>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub uid: u32,
    pub date: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub uid: u32,
    pub date: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub text: String,
    pub attachments: Vec<String>,
    pub truncated: bool,
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace(['"', '\\', '\r', '\n'], " "))
}

/// Newest matching messages first, as short summaries (no bodies).
pub async fn search(q: &Query) -> anyhow::Result<Vec<Summary>> {
    let (a, pw) = Account::load().context("no email account set up: run `agent --email-setup`")?;
    let mut s = session(&a, &pw).await?;
    s.examine(q.folder.as_deref().unwrap_or("INBOX")).await?;
    let mut crit = Vec::new();
    if let Some(d) = q.since_days {
        crit.push(format!("SINCE {}", (chrono::Local::now() - chrono::Duration::days(d as i64)).format("%d-%b-%Y")));
    }
    if let Some(f) = q.from.as_deref().filter(|f| !f.is_empty()) {
        crit.push(format!("FROM {}", quote(f)));
    }
    // Each word is its own TEXT condition (IMAP TEXT matches a substring, not words).
    for w in q.text.as_deref().unwrap_or("").split_whitespace().filter(|w| w.len() > 1).take(5) {
        crit.push(format!("TEXT {}", quote(w)));
    }
    if q.unread == Some(true) {
        crit.push("UNSEEN".into());
    }
    let query = if crit.is_empty() { "ALL".to_string() } else { crit.join(" ") };
    let mut uids: Vec<u32> = s.uid_search(&query).await?.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    uids.truncate(q.limit.unwrap_or(10).min(25));
    let mut out = Vec::new();
    if !uids.is_empty() {
        let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let fetches: Vec<_> = s.uid_fetch(&set, "(UID INTERNALDATE RFC822.SIZE BODY.PEEK[])").await?.try_collect().await?;
        for f in &fetches {
            let Some(raw) = f.body() else { continue };
            let Some(m) = mail_parser::MessageParser::default().parse(raw) else { continue };
            out.push(Summary {
                uid: f.uid.unwrap_or(0),
                date: m.date().map(|d| d.to_rfc3339()).or_else(|| f.internal_date().map(|d| d.to_rfc3339())).unwrap_or_default(),
                from: addr(m.from()),
                subject: m.subject().unwrap_or("").to_string(),
                snippet: clip(&body_text(&m), 200),
            });
        }
        out.sort_by(|a, b| b.uid.cmp(&a.uid));
    }
    let _ = s.logout().await;
    Ok(out)
}

/// One message as text (capped), attachments by name only.
pub async fn read(uid: u32, folder: Option<&str>, max_chars: usize) -> anyhow::Result<Message> {
    let (a, pw) = Account::load().context("no email account set up: run `agent --email-setup`")?;
    let mut s = session(&a, &pw).await?;
    s.examine(folder.unwrap_or("INBOX")).await?;
    let fetches: Vec<_> = s.uid_fetch(uid.to_string(), "(UID INTERNALDATE BODY.PEEK[])").await?.try_collect().await?;
    let _ = s.logout().await;
    let f = fetches.first().context("message not found")?;
    let raw = f.body().context("message not found")?;
    let m = mail_parser::MessageParser::default().parse(raw).context("unparsable message")?;
    let text = body_text(&m);
    Ok(Message {
        uid,
        date: m.date().map(|d| d.to_rfc3339()).or_else(|| f.internal_date().map(|d| d.to_rfc3339())).unwrap_or_default(),
        from: addr(m.from()),
        to: addr(m.to()),
        subject: m.subject().unwrap_or("").to_string(),
        message_id: m.message_id().map(str::to_string),
        truncated: text.chars().count() > max_chars,
        text: text.chars().take(max_chars).collect(),
        attachments: m.attachments().map(|p| format!("{} ({} KB)", p.attachment_name().unwrap_or("unnamed"), p.len() / 1024)).collect(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// Message-ID of the mail being answered (threading headers).
    #[serde(default)]
    pub in_reply_to: Option<String>,
}

pub async fn send(d: &Draft) -> anyhow::Result<()> {
    use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor, message::header::ContentType, transport::smtp::authentication::Credentials};
    let (a, pw) = Account::load().context("no email account set up: run `agent --email-setup`")?;
    if d.to.is_empty() {
        bail!("no recipient");
    }
    let mut b = lettre::Message::builder().from(a.from.parse().context("from address")?).subject(&d.subject).header(ContentType::TEXT_PLAIN);
    for t in &d.to {
        b = b.to(t.parse().with_context(|| format!("bad address {t}"))?);
    }
    for c in &d.cc {
        b = b.cc(c.parse().with_context(|| format!("bad address {c}"))?);
    }
    if let Some(id) = &d.in_reply_to {
        b = b.in_reply_to(id.clone()).references(id.clone());
    }
    let msg = b.body(d.body.clone())?;
    let creds = Credentials::new(a.user.clone(), pw);
    let mailer = if Account::local(&a.smtp_host) {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&a.smtp_host).port(a.smtp_port).credentials(creds).build()
    } else if a.smtp_port == 587 {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&a.smtp_host)?.port(587).credentials(creds).build()
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&a.smtp_host)?.port(a.smtp_port).credentials(creds).build()
    };
    tokio::time::timeout(TIMEOUT, mailer.send(msg)).await.context("SMTP timed out")??;
    Ok(())
}

fn addr(a: Option<&mail_parser::Address>) -> String {
    a.map(|a| {
        a.iter()
            .map(|x| match (x.name(), x.address()) {
                (Some(n), Some(e)) => format!("{n} <{e}>"),
                (None, Some(e)) => e.to_string(),
                (Some(n), None) => n.to_string(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

/// Plain text body (HTML converted), quoted history and signatures trimmed.
fn body_text(m: &mail_parser::Message) -> String {
    let t = m.body_text(0).map(|c| c.to_string()).unwrap_or_default();
    let mut out = Vec::new();
    for l in t.lines() {
        let lt = l.trim();
        if lt.starts_with('>') {
            continue;
        }
        if lt == "--" || (lt.starts_with("On ") && lt.ends_with("wrote:")) || (lt.starts_with("Am ") && lt.ends_with("schrieb:")) {
            break;
        }
        out.push(lt);
    }
    out.join("\n").split("\n\n\n").collect::<Vec<_>>().join("\n\n").trim().to_string()
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}
