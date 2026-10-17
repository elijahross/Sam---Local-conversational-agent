//! Local access control for Sam's localhost services (server, computer-use helper, test API).
//! - Host must be 127.0.0.1 / localhost (defeats DNS rebinding: a rebound site sends its own Host).
//! - A browser Origin is refused unless it is the server's own page (http://127.0.0.1:PORT),
//!   so websites can't drive the WebSocket (no CORS for WebSockets) or the API.
//! - Native clients (Sam.app, tests) send `X-Sam-Token`: a random secret in a 0600 file
//!   (~/Library/Application Support/Sam/local-token) that web pages can't read.

use std::{io::Write, path::PathBuf};

pub fn token_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Sam/local-token")
}

/// The token, created once (0600, exclusive create so concurrent starters agree).
pub fn token() -> String {
    let p = token_path();
    if let Ok(t) = std::fs::read_to_string(&p) {
        if t.trim().len() >= 32 {
            return t.trim().to_string();
        }
    }
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let t: String = (0..32).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
        Ok(mut f) => {
            let _ = f.write_all(t.as_bytes());
            t
        }
        Err(_) => std::fs::read_to_string(&p).map(|s| s.trim().to_string()).unwrap_or(t),
    }
}

/// Checks one request's headers. `own_origins` = the service's own page(s), allowed without token.
pub fn check(host: Option<&str>, origin: Option<&str>, tok: Option<&str>, port: u16) -> Result<(), &'static str> {
    let host_ok = host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return Err("bad host");
    }
    match origin {
        Some(o) if o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}") => Ok(()),
        Some(_) => Err("foreign origin"),
        None => {
            if tok.is_some_and(|t| constant_eq(t.as_bytes(), token().as_bytes())) {
                Ok(())
            } else {
                Err("missing token")
            }
        }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_checks() {
        let t = super::token();
        assert!(super::check(Some("127.0.0.1:47800"), None, Some(&t), 47800).is_ok());
        assert!(super::check(Some("127.0.0.1:47800"), None, None, 47800).is_err(), "token required for native clients");
        assert!(super::check(Some("evil.example:3000"), None, Some(&t), 47800).is_err(), "DNS rebinding");
        assert!(super::check(Some("127.0.0.1:47800"), Some("https://evil.example"), Some(&t), 47800).is_err(), "foreign page");
        assert!(super::check(Some("127.0.0.1:47800"), Some("http://127.0.0.1:47800"), None, 47800).is_ok(), "own page");
    }
}
