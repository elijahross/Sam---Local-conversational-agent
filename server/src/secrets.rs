//! Credentials for the browser agent, kept in the macOS Keychain (service "sam.browser",
//! account "<domain>|<field>"). The LLM never sees them: it writes placeholders like
//! `{{username}}`, `{{password}}`, `{{totp}}` and the executor resolves them for the page's
//! own domain only, and only into a matching field type (Bops "model-blind credential fill").

use std::process::{Command, Stdio};

use hmac::{Hmac, KeyInit, Mac};

/// All Keychain access goes through Apple's signed `/usr/bin/security`. Its code signature never
/// changes, so items it creates are readable without prompts; a Keychain ACL bound to our own
/// binary would re-prompt after every rebuild (ad-hoc signature changes).
const SECURITY: &str = "/usr/bin/security";

/// Keychain service names are "sam.*"; entries written before the rename ("rowan.*") are read
/// once and moved over.
fn svc(service: &str) -> String {
    match service.strip_prefix("rowan.") {
        Some(rest) => format!("sam.{rest}"),
        None => service.to_string(),
    }
}

fn kc_read(service: &str, account: &str) -> Option<String> {
    let out = Command::new(SECURITY).args(["find-generic-password", "-s", service, "-a", account, "-w"]).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
}

pub fn kc_get(service: &str, account: &str) -> Option<String> {
    let new = svc(service);
    if let Some(v) = kc_read(&new, account) {
        return Some(v);
    }
    let old = format!("rowan.{}", new.strip_prefix("sam.").unwrap_or(&new));
    let v = kc_read(&old, account)?;
    if kc_set(&new, account, &v).is_ok() {
        let _ = Command::new(SECURITY).args(["delete-generic-password", "-s", &old, "-a", account]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    Some(v)
}

/// Values go in as hex (-X) so quoting can't break; still briefly visible to same-user `ps`,
/// which is why only small secrets (passwords, a 32-byte key) are stored this way.
pub fn kc_set(service: &str, account: &str, value: &str) -> anyhow::Result<()> {
    let hex: String = value.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let st = Command::new(SECURITY).args(["add-generic-password", "-U", "-s", &svc(service), "-a", account, "-T", SECURITY, "-X", &hex]).stderr(Stdio::null()).status()?;
    anyhow::ensure!(st.success(), "keychain write failed");
    Ok(())
}

pub fn kc_delete(service: &str, account: &str) -> anyhow::Result<()> {
    let new = svc(service);
    let old = format!("rowan.{}", new.strip_prefix("sam.").unwrap_or(&new));
    for s in [new.as_str(), old.as_str()] {
        let _ = Command::new(SECURITY).args(["delete-generic-password", "-s", s, "-a", account]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    Ok(())
}

const SERVICE: &str = "sam.browser";
pub const FIELDS: &[&str] = &["username", "password", "totp"];

pub fn set(domain: &str, field: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(FIELDS.contains(&field), "field must be one of {FIELDS:?}");
    kc_set(SERVICE, &format!("{}|{field}", site(domain)), value)
}

pub fn delete(domain: &str, field: &str) -> anyhow::Result<()> {
    kc_delete(SERVICE, &format!("{}|{field}", site(domain)))
}

/// Resolve a field for `domain` (or its parent site). `totp` returns the current 6-digit code.
pub fn get(domain: &str, field: &str) -> Option<String> {
    let v = kc_get(SERVICE, &format!("{}|{field}", site(domain)))?;
    if field == "totp" { totp_now(&v) } else { Some(v) }
}

/// The stored value as it is (for `totp`: the seed, not a code).
pub fn get_raw(domain: &str, field: &str) -> Option<String> {
    kc_get(SERVICE, &format!("{}|{field}", site(domain)))
}

pub fn has_login(domain: &str) -> bool {
    get(domain, "password").is_some()
}

/// Encrypt with the local jar key (AES-256-GCM; key created once, kept in the Keychain).
pub fn seal(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    use aes_gcm::{Aes256Gcm, Nonce, aead::Aead};
    let cipher = Aes256Gcm::new_from_slice(&jar_key()?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let nonce: [u8; 12] = rand::random();
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(&Nonce::from(nonce), plain).map_err(|_| anyhow::anyhow!("encrypt"))?);
    Ok(out)
}

pub fn open(sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
    use aes_gcm::{Aes256Gcm, Nonce, aead::Aead};
    anyhow::ensure!(sealed.len() > 12, "short");
    let cipher = Aes256Gcm::new_from_slice(&jar_key()?).map_err(|e| anyhow::anyhow!("{e}"))?;
    cipher.decrypt(&Nonce::try_from(&sealed[..12]).map_err(|_| anyhow::anyhow!("nonce"))?, &sealed[12..]).map_err(|_| anyhow::anyhow!("decrypt (key changed?)"))
}

fn jar_key() -> anyhow::Result<Vec<u8>> {
    if let Some(k) = kc_get("sam.browser.jarkey", "default").and_then(|h| data_encoding::HEXLOWER.decode(h.as_bytes()).ok()).filter(|k| k.len() == 32) {
        return Ok(k);
    }
    let k: [u8; 32] = rand::random();
    kc_set("sam.browser.jarkey", "default", &data_encoding::HEXLOWER.encode(&k))?;
    Ok(k.to_vec())
}

/// Registrable site: "accounts.google.com" → "google.com", "www.bbc.co.uk" → "bbc.co.uk".
/// Multi-tenant hosting suffixes are public suffixes: "app.herokuapp.com" stays itself, so a
/// credential never leaks to another tenant's site.
pub fn site(host: &str) -> String {
    let h = host.trim_start_matches("www.").to_lowercase();
    let parts: Vec<&str> = h.split('.').collect();
    const PUBLIC_SUFFIXES: &[&str] = &[
        "co.uk", "org.uk", "ac.uk", "com.au", "co.at", "or.at", "gv.at", "co.jp", "com.br", "co.nz", "co.za", "com.mx",
        "herokuapp.com", "github.io", "gitlab.io", "vercel.app", "netlify.app", "pages.dev", "workers.dev", "web.app", "firebaseapp.com",
        "azurewebsites.net", "cloudfront.net", "appspot.com", "blogspot.com", "wordpress.com", "onrender.com", "fly.dev", "glitch.me",
        "replit.app", "s3.amazonaws.com", "ngrok.io", "ngrok-free.app",
    ];
    let suffix_len = PUBLIC_SUFFIXES.iter().filter(|s| h == **s || h.ends_with(&format!(".{s}"))).map(|s| s.split('.').count()).max().unwrap_or(1);
    let n = (suffix_len + 1).min(parts.len());
    parts[parts.len() - n..].join(".")
}

/// RFC 6238 TOTP (SHA-1, 30 s, 6 digits) from a base32 seed.
pub fn totp_now(seed_b32: &str) -> Option<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    totp_at(seed_b32, now)
}

pub fn totp_at(seed_b32: &str, unix: u64) -> Option<String> {
    let clean: String = seed_b32.chars().filter(|c| !c.is_whitespace() && *c != '=').collect::<String>().to_uppercase();
    let key = data_encoding::BASE32_NOPAD.decode(clean.as_bytes()).ok()?;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).ok()?;
    mac.update(&(unix / 30).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let code = ((h[o] as u32 & 0x7f) << 24 | (h[o + 1] as u32) << 16 | (h[o + 2] as u32) << 8 | h[o + 3] as u32) % 1_000_000;
    Some(format!("{code:06}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_and_sites() {
        // RFC 6238 test secret "12345678901234567890" (base32 GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ) at t=59 → 287082
        assert_eq!(totp_at("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59).as_deref(), Some("287082"));
        assert_eq!(site("accounts.google.com"), "google.com");
        assert_eq!(site("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site("the-internet.herokuapp.com"), "the-internet.herokuapp.com");
        assert_eq!(site("evil.herokuapp.com"), "evil.herokuapp.com");
        assert_eq!(site("user.github.io"), "user.github.io");
        assert_eq!(site("practicetestautomation.com"), "practicetestautomation.com");
    }
}

/// Contact details out of text bound for a model: an email address keeps its first letter and
/// domain ("a…@example.com"), a phone number its last three digits ("…567"). Enough for the
/// model to say which contact it found; the full details stay on the Mac (the lookup's file,
/// the code that fills in recipients).
pub fn mask_contacts(text: &str) -> String {
    static EMAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static PHONE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let email = EMAIL.get_or_init(|| regex::Regex::new(r"\b([A-Za-z0-9])[A-Za-z0-9._%+-]*@([A-Za-z0-9.-]+\.[A-Za-z]{2,})\b").unwrap());
    let phone = PHONE.get_or_init(|| regex::Regex::new(r"(?:\+|\b00|\b0)\d[\d ()/.-]{6,}\d\b").unwrap());
    let t = email.replace_all(text, "$1…@$2").into_owned();
    phone.replace_all(&t, |c: &regex::Captures| {
        let d: String = c[0].chars().filter(|ch| ch.is_ascii_digit()).collect();
        if d.len() < 8 { c[0].to_string() } else { format!("phone …{}", &d[d.len() - 3..]) }
    }).into_owned()
}

/// Password managers and credential stores, by app name or bundle id: Sam never sends their
/// window text to a model (the helper and the app withhold it too; this is the backstop).
pub fn sensitive_app(app: &str) -> bool {
    let a = app.trim().to_lowercase();
    if a.is_empty() { return false; }
    const IDS: &[&str] = &["com.apple.keychainaccess", "com.apple.passwords", "com.agilebits.onepassword", "com.1password.", "com.bitwarden.", "com.lastpass.",
        "com.dashlane.", "org.keepassxc.", "com.enpass.", "in.sinew.enpass", "com.nordpass.", "com.protonpass.", "me.proton.pass"];
    const NAMES: &[&str] = &["passwords", "keychain access", "schlüsselbundverwaltung", "1password", "bitwarden", "lastpass", "dashlane", "keepassxc", "keepass", "enpass", "nordpass", "proton pass", "passwörter"];
    IDS.iter().any(|i| a == *i || a.starts_with(i)) || NAMES.iter().any(|n| a == *n || a.starts_with(&format!("{n} ")))
}

/// Masks credentials in text before it reaches a model, a log or speech (vault: modifier
/// pipeline `afterExecute`): API keys, tokens, private keys, password assignments (English and
/// German), one-time / door / card codes, card numbers (Luhn) and IBANs (mod 97). Every cloud
/// model request goes through it (llm.rs); `{{password}}`-style placeholders are kept, they are
/// how the browser agent asks for a stored login without seeing it.
pub fn redact(text: &str) -> String {
    type Fix = fn(&regex::Captures) -> String;
    static RES: std::sync::OnceLock<Vec<(regex::Regex, Fix)>> = std::sync::OnceLock::new();
    fn all(_: &regex::Captures) -> String { "[redacted]".into() }
    /// label + separator kept, the value masked (a `{{placeholder}}` stays).
    fn labelled(c: &regex::Captures) -> String {
        let v = c.name("v").map_or("", |m| m.as_str());
        if v.starts_with("{{") { return c[0].to_string(); }
        format!("{}{}[redacted]", &c["l"], &c["s"])
    }
    /// "my password hunter2": only when the word after the label looks like a secret (a digit
    /// or a symbol in it), so "password manager" / "Passwort vergessen" stay readable.
    fn bare_label(c: &regex::Captures) -> String {
        let v = &c["v"];
        let core = v.trim_end_matches(['.', ',', '!', '?', ':', ')']);
        let secretish = core.chars().any(|ch| ch.is_ascii_digit()) || core.chars().any(|ch| !ch.is_alphanumeric() && ch != '-');
        if v.starts_with("{{") || !secretish { c[0].to_string() } else { format!("{}{}[redacted]", &c["l"], &c["s"]) }
    }
    fn url_pw(c: &regex::Captures) -> String { format!("{}[redacted]{}", &c[1], &c[2]) }
    fn code_first(c: &regex::Captures) -> String { format!("[redacted]{}", &c["rest"]) }
    fn card(c: &regex::Captures) -> String {
        let d: Vec<u32> = c[0].chars().filter_map(|ch| ch.to_digit(10)).collect();
        let luhn = d.iter().rev().enumerate().map(|(i, &x)| if i % 2 == 1 { let y = x * 2; if y > 9 { y - 9 } else { y } } else { x }).sum::<u32>() % 10 == 0;
        if (13..=19).contains(&d.len()) && luhn { "[redacted card]".into() } else { c[0].to_string() }
    }
    fn iban(c: &regex::Captures) -> String {
        let raw: String = c[0].chars().filter(|ch| !ch.is_whitespace()).collect();
        let moved = format!("{}{}", &raw[4..], &raw[..4]);
        let mut rem = 0u64;
        for ch in moved.chars() {
            let v = if ch.is_ascii_digit() { ch as u64 - '0' as u64 } else { ch.to_ascii_uppercase() as u64 - 'A' as u64 + 10 };
            rem = if v >= 10 { (rem * 100 + v) % 97 } else { (rem * 10 + v) % 97 };
        }
        if raw.len() >= 15 && rem == 1 { "[redacted IBAN]".into() } else { c[0].to_string() }
    }
    let res = RES.get_or_init(|| {
        let v: Vec<(&str, Fix)> = vec![
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", all),
            (r"\b(sk-[A-Za-z0-9_-]{16,}|[sr]k_(?:live|test)_[A-Za-z0-9]{16,}|AIza[0-9A-Za-z_-]{30,}|ya29\.[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|glpat-[A-Za-z0-9_-]{20,}|npm_[A-Za-z0-9]{30,}|xox[abeprs]-[A-Za-z0-9-]{10,}|hf_[A-Za-z0-9]{30,}|(?:AKIA|ASIA)[0-9A-Z]{16})\b", all),
            (r#"(?i)(?P<l>\b(?:password|passwort|kennwort|passwd|pwd|passcode|zugangsdaten|secret|token|api[_-]?key|authorization)\b)(?P<s>\s*[:=]\s*|\s+(?:is|ist|lautet)\s+)(?:bearer\s+)?(?P<v>[^\s"',;]+)"#, labelled),
            (r#"(?i)(?P<l>\b(?:my |mein |the |das )?(?:wi-?fi |wlan[ -])?(?:password|passwort|kennwort|passcode)\b)(?P<s>\s+)(?P<v>[^\s"',;]{6,})"#, bare_label),
            (r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b", all),
            // .env / config style keys: AUTH0_CLIENT_SECRET="…", GITHUB_TOKEN=…, S3_SECRET_KEY: …
            (r#"(?i)(?P<l>\b[A-Za-z0-9_.-]*(?:secret|token|passw(?:or)?d|api[_-]?key|apikey|private[_-]?key|access[_-]?key|credentials?|auth[_-]?key)[A-Za-z0-9_.-]*)(?P<s>\s*[:=]\s*)"?(?P<v>[^\s"',;#]{4,})"?"#, labelled),
            // passwords inside connection URLs: postgres://user:pass@host
            (r"\b([a-z][a-z0-9+.-]*://[^:/\s@]+:)[^@\s/]+(@)", url_pw),
            // codes after a code-type word (not postcodes): verification, door, card security codes
            (r"(?i)\b((verification|security|login|one-time|confirmation|sign-in|2fa|sms|bestätigungs|sicherheits|anmelde|door|tür|gate|tor|alarm|access|zugangs)[ -]?code|your code|dein code|ihr code|otp|tan|pin|cvv|cvc)\b[^\d\n]{0,24}\d{3,8}\b", all),
            // codes before the word: "482913 is your verification code", "G-482913 is your Google code"
            (r"(?i)\b(?:G-)?\d{4,8}\b(?P<rest>\s+(?:is|ist)\s+(?:your|dein|ihr|the)\b[^\n]{0,40}?\b(?:code|otp|pin|passcode|tan)\b)", code_first),
            (r"\b(?:\d[ -]?){12,18}\d\b", card),
            (r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,3})?\b", iban),
        ];
        v.into_iter().map(|(r, f)| (regex::Regex::new(r).expect("redact regex"), f)).collect()
    });
    let mut out = text.to_string();
    for (re, f) in res {
        out = re.replace_all(&out, |c: &regex::Captures| f(c)).into_owned();
    }
    out
}

#[cfg(test)]
mod redact_tests {
    #[test]
    fn masks_env_files_and_url_passwords() {
        let env = "AUTH0_CLIENT_SECRET=\"0TLWLnqDTKpbcHUjvX\"\nS3_SECRET_KEY=\"minioadmin\"\nBACKEND_API_KEY=\"a755cc2d9b0e\" # note\nDATABASE_URL=\"postgresql://postgres:hunter2pw@localhost:5432/db\"\nTENANT=\"generic\"";
        let r = super::redact(env);
        for leak in ["0TLWLnqDTKpbcHUjvX", "minioadmin", "a755cc2d9b0e", "hunter2pw"] {
            assert!(!r.contains(leak), "{leak} leaked: {r}");
        }
        assert!(r.contains("AUTH0_CLIENT_SECRET") && r.contains("TENANT=\"generic\"") && r.contains("localhost:5432"), "{r}");
    }

    #[test]
    fn masks_secrets_keeps_text() {
        let t = "key sk-abcdefghijklmnopqrstuv and AIzaSyA1234567890abcdefghijklmnopqrstu; password: hunter2 ok. The token is abc123xyz";
        let r = super::redact(t);
        assert!(!r.contains("sk-abc") && !r.contains("AIzaSy") && !r.contains("hunter2") && !r.contains("abc123xyz"), "{r}");
        assert!(r.contains("password: [redacted]") && r.contains(" ok."));
        assert_eq!(super::redact("Dinner on Friday at 7?"), "Dinner on Friday at 7?");
    }

    #[test]
    fn masks_contact_details() {
        let r = super::mask_contacts("RELATIONSHIP: girlfriend Anna (anna.berger@example.com, +43 664 1234567), office 01 4087 2200");
        assert!(!r.contains("anna.berger") && !r.contains("1234567") && !r.contains("4087"), "{r}");
        assert!(r.contains("a…@example.com") && r.contains("phone …567") && r.contains("girlfriend Anna"), "{r}");
        assert_eq!(super::mask_contacts("It costs 1499 euros on 15.10.2026"), "It costs 1499 euros on 15.10.2026");
    }

    /// The gaps the 2026-10-15 audit found, each must be masked; the controls must survive.
    #[test]
    fn masks_audit_gaps_keeps_ordinary_text() {
        let leaks = [
            ("Passwort: Geheim123", "Geheim123"),
            ("das Kennwort ist Sommer2024!", "Sommer2024"),
            ("my password hunter2", "hunter2"),
            ("our wifi password is Bl4ckC4t", "Bl4ckC4t"),
            ("123456 is your verification code", "123456"),
            ("Your code is 482913", "482913"),
            ("Dein Code: 482913", "482913"),
            ("G-482913 is your Google verification code", "482913"),
            ("key sk_live_51HxAbCdEfGhIjKlMnOp", "sk_live_51H"),
            ("glpat-abcdefghijklmnopqrst12", "glpat-"),
            ("card 4111 1111 1111 1111 exp 12/27 cvv 123", "4111 1111"),
            ("cvv 123", "123"),
            ("IBAN DE89 3704 0044 0532 0130 00", "3704 0044"),
            ("the door code 4711", "4711"),
        ];
        for (t, secret) in leaks {
            let r = super::redact(t);
            assert!(!r.contains(secret), "{secret} leaked: {t} → {r}");
        }
        for ok in ["Use {{password}} for the login: fill f2 {{password}}", "password: {{password}}", "open the password manager", "Passwort vergessen?",
                   "Call me at +43 664 1234567", "Order 12345678 ships Friday", "Postcode 1070 Vienna", "It costs 1499 euros", "max tokens 1500",
                   "The meeting is at 10:30 in room 4.12", "the meeting id is 834 2211 9087"] {
            assert_eq!(super::redact(ok), ok, "changed: {ok}");
        }
    }
}
