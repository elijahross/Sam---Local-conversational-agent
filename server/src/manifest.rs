//! The model manifest: model ids that change without an app update (a provider retires a model,
//! a better default ships). Sam.app downloads `manifest.json` from the latest GitHub release,
//! checks its Ed25519 signature (the same key as app updates) and writes the verified copy to
//! `data/manifest.json`; the server reads it at start. Schema: `release/manifest.json`.
//!
//! Precedence for every value: environment > manifest > compiled default. The manifest can only
//! name models (an allowlist of settings, plain ids); endpoints, keys and safety settings ship
//! with app updates only.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Settings the manifest may override: model ids and reasoning levels only.
const SETTINGS: &[&str] = &[
    "MINIMAX_MODEL", "MINIMAX_LIGHT_MODEL", "MINIMAX_VOICE_MODEL", "MINIMAX_REASONING",
    "OPENAI_MODEL", "OPENAI_REASONING", "GEMINI_MODEL", "GEMINI_THINKING",
];

/// Retirements known at build time (MiniMax M2.x retires 2026-11-18).
const BUILTIN_RETIRED: &[(&str, &str)] = &[("MiniMax-M2*", "default")];

#[derive(Default, Debug)]
pub struct Manifest {
    pub version: u64,
    settings: HashMap<String, String>,
    /// Per-provider tiers [light, standard, heavy].
    pub models: HashMap<String, [String; 3]>,
    /// Retired model id (or "prefix*") → its successor, or "default" for the tier's current default.
    replace: Vec<(String, String)>,
}

/// A model id: letters, digits and `._:/-`, at most 100 characters.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

impl Manifest {
    pub fn parse(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut m = Manifest { version: v["version"].as_u64()?, ..Default::default() };
        if let Some(o) = v["settings"].as_object() {
            for (k, val) in o {
                if let Some(s) = val.as_str().filter(|s| SETTINGS.contains(&k.as_str()) && plain(s)) {
                    m.settings.insert(k.clone(), s.to_string());
                }
            }
        }
        if let Some(o) = v["models"].as_object() {
            for (k, val) in o {
                let tiers: Vec<&str> = val.as_array().into_iter().flatten().filter_map(|t| t.as_str()).filter(|t| plain(t)).collect();
                if tiers.len() == 3 && plain(k) {
                    m.models.insert(k.clone(), [tiers[0].into(), tiers[1].into(), tiers[2].into()]);
                }
            }
        }
        if let Some(o) = v["replace"].as_object() {
            for (k, val) in o {
                if let Some(s) = val.as_str().filter(|s| plain(k.trim_end_matches('*')) && plain(s)) {
                    m.replace.push((k.clone(), s.to_string()));
                }
            }
        }
        Some(m)
    }

    /// The successor of a retired id: Some(new id) or Some("default"); None when it isn't retired.
    /// The manifest's list first, then the retirements this build already knows about.
    pub fn successor(&self, id: &str) -> Option<&str> {
        let hit = |k: &str| match k.strip_suffix('*') {
            Some(prefix) => id.starts_with(prefix),
            None => id == k,
        };
        self.replace.iter().find(|(k, _)| hit(k)).map(|(_, v)| v.as_str())
            .or_else(|| BUILTIN_RETIRED.iter().find(|(k, _)| hit(k)).map(|(_, v)| *v))
    }
}

pub fn get() -> &'static Manifest {
    static M: OnceLock<Manifest> = OnceLock::new();
    M.get_or_init(|| {
        let path = crate::thinker::data_dir().join("manifest.json");
        let m = std::fs::read_to_string(&path).ok().and_then(|t| Manifest::parse(&t)).unwrap_or_default();
        if m.version > 0 {
            tracing::info!(version = m.version, "model manifest loaded");
        }
        m
    })
}

/// A setting from the environment, else the manifest; None when neither sets it.
pub fn pick(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty()).or_else(|| get().settings.get(key).cloned())
}

/// A setting: the environment, else the manifest, else the compiled default.
pub fn setting(key: &str, default: &str) -> String {
    pick(key).unwrap_or_else(|| default.to_string())
}

/// `id`, or its successor when the manifest lists it as retired (`fallback` stands for "default").
pub fn current(id: &str, fallback: &str) -> String {
    match get().successor(id) {
        Some("default") => fallback.to_string(),
        Some(new) => new.to_string(),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_allowlisted_values_and_retirements() {
        let m = Manifest::parse(r#"{"version": 4,
            "settings": {"MINIMAX_VOICE_MODEL": "MiniMax-M4-Flash", "VOICE_CONFIRM": "0", "OPENAI_MODEL": "bad id; rm -rf"},
            "models": {"anthropic": ["a", "b", "c"], "mistral": ["only-two", "x"]},
            "replace": {"MiniMax-M2*": "default", "gpt-6-luna": "gpt-7-luna"}}"#).unwrap();
        assert_eq!(m.version, 4);
        assert_eq!(m.settings.get("MINIMAX_VOICE_MODEL").map(String::as_str), Some("MiniMax-M4-Flash"));
        assert!(!m.settings.contains_key("VOICE_CONFIRM"), "safety settings are not the manifest's");
        assert!(!m.settings.contains_key("OPENAI_MODEL"), "ids are plain");
        assert_eq!(m.models["anthropic"], ["a", "b", "c"].map(String::from));
        assert!(!m.models.contains_key("mistral"));
        assert_eq!(m.successor("MiniMax-M2.7"), Some("default"));
        assert_eq!(m.successor("gpt-6-luna"), Some("gpt-7-luna"));
        assert_eq!(m.successor("MiniMax-M3"), None);
        assert!(Manifest::parse(r#"{"settings": {}}"#).is_none(), "a manifest needs a version");
    }
}
