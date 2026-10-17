//! Shared configuration and application state (used by the voice server and the CLIs).

use std::{sync::Arc, time::Duration};

use crate::{judge, llm::Provider, memory, tasks};

/// Runtime knobs. Defaults are the lab winners (see best-config lab.json); every one is
/// overridable via env so experiments can sweep them without recompiling.
#[derive(Clone, Debug)]
pub struct Config {
    /// The model provider for everything (voice, thinker, memory, research): `LLM_PROVIDER` =
    /// minimax (default) | openai | anthropic | gemini | mistral | ollama | claude-code. Gemini is
    /// opt-in only (cost); ollama runs local models; claude-code uses the signed-in CLI.
    pub provider: Provider,
    pub minimax_key: Option<String>,
    pub minimax_model: String,
    /// The spoken turn itself (not the thinker): M3.1 Flash answers in ~0.5 s to the first token
    /// against M3's 1.6 s with the same persona quality (llm-cost lab, 2026-10-13).
    pub minimax_voice_model: String,
    /// `reasoning_effort` for MiniMax M3.1 models (low | medium | high | xhigh | max).
    pub minimax_reasoning: String,
    pub openai_key: Option<String>,
    pub openai_model: String,
    /// OpenAI-compatible base URL (also works for Azure / proxies).
    pub openai_url: String,
    /// `reasoning_effort` for OpenAI reasoning models ("" = the API default).
    pub openai_reasoning: String,
    pub gemini_key: Option<String>,
    pub gemini_model: String,
    pub anthropic_key: Option<String>,
    pub mistral_key: Option<String>,
    /// Ollama: the daemon (`OLLAMA_URL`), the context window sent with every call
    /// (`OLLAMA_NUM_CTX`; Ollama's own default is too small for Sam's prompts) and how long the
    /// model stays loaded between turns (`OLLAMA_KEEP_ALIVE`).
    pub ollama_url: String,
    pub ollama_ctx: u32,
    pub ollama_keep_alive: String,
    /// Let thinking models think (`OLLAMA_THINK=1`): better plans, slower first word.
    pub ollama_think: bool,
    /// The `claude` CLI for the Claude Code provider (`CLAUDE_BIN`, else PATH and the usual places).
    pub claude_bin: Option<std::path::PathBuf>,
    /// Model per provider and task tier (light / standard / heavy): data/llm.json, else defaults.
    pub models: std::collections::HashMap<String, [String; 3]>,
    pub thinking_level: String,
    /// single: `provider` voices every turn. delegate (opt-in, needs Gemini + MiniMax keys):
    /// Gemini talks and hands turns that need deep reasoning to MiniMax behind a bridge line.
    pub llm_mode: LlmMode,
    /// MiniMax must start speaking within this long after a delegation, else Gemini takes over.
    pub delegate_timeout_ms: u32,
    pub sidecar_url: String,
    pub vad_threshold: f32,
    /// Trailing silence after which the turn is checked for completeness (smart-turn v3). The turn
    /// ends here when the model says the user is done (p >= turn_threshold).
    pub vad_silence_ms: u32,
    /// smart-turn probability needed to end the turn at `vad_silence_ms`; below it we keep
    /// listening (the pause is a breath or a hesitation) until `turn_timeout_ms` of silence.
    pub turn_threshold: f32,
    /// A pause the model called "not the end" is asked again once, at this much silence, with
    /// the lower `turn_recheck_threshold` (2026-10-13: 7 of 9 real turns ran into the 3 s fallback).
    pub turn_recheck_ms: u32,
    pub turn_recheck_threshold: f32,
    /// Silence that ends the turn regardless of what the model thinks.
    pub turn_timeout_ms: u32,
    /// Speculative endpointing: the turn starts after `vad_silence_ms`, but agent audio is held
    /// until the user has been silent this long (0 = no gate). If the user resumes before any
    /// audio was heard, the turn is cancelled and its audio merged into the next utterance.
    pub commit_silence_ms: u32,
    /// If the user resumes speaking within this long after the agent's first audio, treat it
    /// as a continuation (they only paused): cut the agent off and merge both utterances.
    pub merge_window_ms: u32,
    /// Continuous speech needed before we treat it as a user turn (filters clicks/coughs).
    pub min_speech_ms: u32,
    /// Continuous speech needed to interrupt the agent while it is talking.
    pub barge_in_ms: u32,
    /// The first TTS chunk is cut at the first clause boundary once it has this many words.
    pub first_chunk_min_words: usize,
    /// Cut the first chunk at a sentence end rather than the first clause (FIRST_CUT=sentence).
    pub first_cut_sentence: bool,
    /// Rest-as-one TTS (chunker.rs): after the first piece, hold the reply and send it as one piece
    /// at its end, at a sentence end once this many words are held, or after `tts_rest_wait_ms`
    /// since the first piece. 0 = one piece per sentence (the old behaviour).
    pub tts_rest_words: usize,
    pub tts_rest_wait_ms: u64,
    /// Whole-reply TTS (chunker.rs): hold the first piece up to this long after its first clause
    /// is ready, so a quick reply is one generation (0 = off: first clause cut at once).
    pub tts_whole_wait_ms: u64,
    /// Tone tags cut the reply into pieces (only for TTS models that take a tone instruction).
    pub tts_tag_split: bool,
    /// Later chunks are cut at sentence boundaries once they have this many words.
    pub chunk_min_words: usize,
    /// Speak a short filler ("Hmm,") if no reply audio has started this long after end of turn
    /// (0 = off). Covers slow LLM responses.
    pub filler_after_ms: u32,
    /// Speak a filler if the agent goes silent mid-reply for this long while still generating.
    pub stall_filler_ms: u32,
    /// Base Qwen3-TTS style instruction; tone tags from the LLM are appended per sentence.
    pub tts_style: String,
    /// Use the LLM's [tone] tags to steer TTS emotion per sentence.
    pub tone_tags: bool,
    /// Persona / system prompt of the default character (`CHARACTER`; PERSONA_FILE overrides).
    pub persona: String,
    /// Default character + voice profile for new sessions; a session can switch (island menu).
    pub character: Character,
    /// Personas by character name (sam, sam).
    pub personas: std::collections::HashMap<String, String>,
    /// Consolidate memory every N user turns (and always at session end).
    pub consolidate_every: usize,
    /// Live-loop Laya call deadline; on timeout the turn proceeds without judgments.
    pub judge_timeout_ms: u64,
    /// Recalled items per normal turn / for questions Laya routes to memory.
    pub memory_k: usize,
    pub memory_k_deep: usize,
    /// Hold the reply's first audio this long after end of turn when Laya thinks the user
    /// stopped mid-sentence (p_done < 0.5); resumed speech merges as usual.
    pub eot_hold_ms: u64,
    /// Act on a Laya route (task / deep) only at or above this probability.
    pub route_min_p: f32,
}

/// A character: who talks (persona) and how they sound (a voice profile in voices/<id>/).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Character {
    pub name: String,
    /// male | female for characters with two voices ("" for Sam).
    pub variant: String,
    pub voice: String,
}

impl Character {
    /// sam (+ male | female). None for an unknown name.
    pub fn new(name: &str, variant: &str) -> Option<Self> {
        match name {
            "sam" => {
                let v = if variant == "female" { "female" } else { "male" };
                Some(Self { name: "sam".into(), variant: v.into(), voice: format!("sam-{v}") })
            }
            _ => None,
        }
    }

    pub fn all() -> Vec<Self> {
        vec![Self::new("sam", "male").unwrap(), Self::new("sam", "female").unwrap()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LlmMode {
    Single,
    Delegate,
}

impl Config {
    /// The provider's standard model (voice, planning, answers).
    pub fn model_name(&self, p: Provider) -> &str {
        self.model(p, crate::llm::Tier::Standard)
    }

    /// The model for a task tier: the cheapest capable one (lab: scoring / mapping / checks on
    /// a small model, planning and answers on the standard one, long synthesis on the big one).
    pub fn model(&self, p: Provider, tier: crate::llm::Tier) -> &str {
        let i = tier as usize;
        self.models.get(p.id()).map(|t| t[i].as_str()).filter(|m| !m.is_empty()).unwrap_or(match p {
            Provider::MiniMax => &self.minimax_model,
            Provider::OpenAI => &self.openai_model,
            Provider::Gemini => &self.gemini_model,
            Provider::Anthropic | Provider::Mistral | Provider::Ollama | Provider::ClaudeCode => "",
        })
    }

    /// The model for the spoken reply: MiniMax has a dedicated fast one; others use their standard tier.
    pub fn voice_model(&self, p: Provider) -> &str {
        match p {
            Provider::MiniMax if !self.minimax_voice_model.is_empty() => &self.minimax_voice_model,
            _ => self.model(p, crate::llm::Tier::Standard),
        }
    }

    pub fn key(&self, p: Provider) -> Option<&str> {
        match p {
            Provider::MiniMax => self.minimax_key.as_deref(),
            Provider::OpenAI => self.openai_key.as_deref(),
            Provider::Gemini => self.gemini_key.as_deref(),
            Provider::Anthropic => self.anthropic_key.as_deref(),
            Provider::Mistral => self.mistral_key.as_deref(),
            Provider::Ollama | Provider::ClaudeCode => None,
        }
    }
}

/// Where the server's own files are (tools/, static/, personas, mcp.json): SAM_RESOURCES, else
/// Sam.app/Contents/Resources/server next to the running binary, else the repo (development).
pub fn resources_dir() -> std::path::PathBuf {
    if let Ok(r) = std::env::var("SAM_RESOURCES") {
        return r.into();
    }
    if let Some(r) = app_resources() {
        return r.join("server");
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Sam.app/Contents/Resources when the server runs from inside the app bundle.
pub fn app_resources() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "Contents"))?;
    Some(contents.join("Resources")).filter(|r| r.join("server").exists())
}

/// Sam's voice (onboarding: male | female): data/voice.json, else SAM_VOICE, else male.
/// "tiny" (Kyutai Pocket TTS, 100M, ~0.4 GB), "light" (Qwen3-TTS 0.6B, 1.9 GB) or "steady" (1.7B,
/// 2.4 GB, fewest glitches: voice-glitch lab); the app launches the speech sidecar with the matching model.
pub fn voice_model_choice() -> String {
    std::fs::read_to_string(crate::thinker::data_dir().join("voice.json")).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["model"].as_str().map(str::to_string)).filter(|m| m == "steady" || m == "tiny").unwrap_or_else(|| "light".into())
}

pub fn voice_choice() -> String {
    std::fs::read_to_string(crate::thinker::data_dir().join("voice.json")).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["voice"].as_str().map(str::to_string)).unwrap_or_else(|| env_or("SAM_VOICE", "male"))
}

/// Model tiers per provider when data/llm.json doesn't say otherwise ([light, standard, heavy]):
/// compiled defaults, then the model manifest's tiers, then single settings (environment > manifest).
pub fn default_models() -> std::collections::HashMap<String, [String; 3]> {
    use crate::manifest::pick;
    let t = |a: &str, b: &str, c: &str| [a.to_string(), b.to_string(), c.to_string()];
    // Lab 2026-10-13 (llm-cost lab): M3 answers a voice turn in 1.6 s to first token and plans
    // JSON in 3.9 s; M3.1 Flash Preview at low effort in 0.5 s / 1.5 s with the same persona
    // quality, so it serves the light tier.
    let mut m: std::collections::HashMap<String, [String; 3]> = [
        ("minimax", t("MiniMax-M3.1-Flash-Preview", "MiniMax-M3", "MiniMax-M3")), ("openai", t("gpt-5-nano", "gpt-6-luna", "gpt-5")),
        ("gemini", t("gemini-3.5-flash-lite", "gemini-3.5-flash", "gemini-3.5-pro")),
        ("anthropic", t("claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5")), ("mistral", t("mistral-small-latest", "mistral-medium-latest", "mistral-large-latest")),
        // One local model for every tier: switching models between a light and a standard call
        // would reload weights (seconds) on each turn. The CLI resolves its aliases itself.
        ("ollama", t("qwen3.5:9b", "qwen3.5:9b", "qwen3.5:9b")), ("claude-code", t("haiku", "sonnet", "opus"))]
        .into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    for (k, v) in &crate::manifest::get().models {
        m.insert(k.clone(), v.clone());
    }
    let mut set = |provider: &str, tiers: &[usize], key: &str| {
        if let (Some(v), Some(row)) = (pick(key), m.get_mut(provider)) {
            for &i in tiers { row[i] = v.clone(); }
        }
    };
    set("minimax", &[0], "MINIMAX_LIGHT_MODEL");
    set("minimax", &[1, 2], "MINIMAX_MODEL");
    set("openai", &[1], "OPENAI_MODEL");
    set("gemini", &[0], "GEMINI_MODEL");
    m
}

/// The user's provider choice (onboarding / settings): data/llm.json {"provider", "models"}.
/// Saved choices of retired models (model manifest, plus the retirements this build knows)
/// move to their successor or the tier's current default.
pub fn llm_settings() -> serde_json::Value {
    let mut v: serde_json::Value = std::fs::read_to_string(crate::thinker::data_dir().join("llm.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let defaults = default_models();
    if let Some(o) = v["models"].as_object_mut() {
        for (provider, tiers) in o.iter_mut() {
            let Some(fresh) = defaults.get(provider) else { continue };
            for (i, m) in tiers.as_array_mut().into_iter().flatten().enumerate().take(3) {
                if let Some(id) = m.as_str() {
                    let now = crate::manifest::current(id, &fresh[i]);
                    if now != id {
                        *m = serde_json::Value::String(now);
                    }
                }
            }
        }
    }
    v
}

/// An API key: the environment first, else the Keychain (stored by the onboarding).
pub fn api_key(p: Provider) -> Option<String> {
    let env = match p {
        Provider::MiniMax => "MINIMAX_API_KEY",
        Provider::OpenAI => "OPENAI_API_KEY",
        Provider::Gemini => "GEMINI_API_KEY",
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::Mistral => "MISTRAL_API_KEY",
        Provider::Ollama | Provider::ClaudeCode => return None,
    };
    std::env::var(env).ok().filter(|v| !v.trim().is_empty()).or_else(|| crate::secrets::kc_get("sam.llm", p.id()))
}

/// A model id setting (environment > manifest > default), moved on when it has been retired.
fn model_setting(key: &str, default: &str) -> String {
    crate::manifest::current(&crate::manifest::setting(key, default), default)
}

pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// None when the embedding sidecar is unavailable or MEMORY=0: the agent still talks.
    /// Late-bound: the embedding sidecar starts after the server (the app launches it second), so
    /// the store is opened as soon as the sidecar answers, not only at startup (2026-10-13: a 3 s
    /// startup probe failed on every launch and the owner's memory stayed empty for days).
    pub memory: std::sync::OnceLock<memory::Memory>,
    /// Laya micro-judgments (mind sidecar). None = judge unavailable; every caller fails open.
    pub judge: Option<judge::Judge>,
    pub tasks: std::sync::OnceLock<tasks::Tasks>,
}

const TTS_STYLE: &str = "Speak in a natural, relaxed, conversational way at a lively everyday pace, like chatting with a friend on the phone";


impl AppState {
    /// Builds the shared state from the environment (.env is loaded by the caller).
    pub async fn from_env() -> anyhow::Result<Arc<Self>> {
        let cfg = Config {
            // Legacy LLM_MODE=gemini|minimax still picks the provider.
            // The onboarding's choice (data/llm.json) wins over the environment.
            provider: llm_settings()["provider"].as_str().and_then(Provider::parse)
                .or_else(|| Provider::parse(&env_or("LLM_PROVIDER", &env_or("LLM_MODE", "minimax")))).unwrap_or(Provider::MiniMax),
            minimax_key: api_key(Provider::MiniMax),
            minimax_model: model_setting("MINIMAX_MODEL", "MiniMax-M3"),
            minimax_reasoning: crate::manifest::setting("MINIMAX_REASONING", "low"),
            minimax_voice_model: model_setting("MINIMAX_VOICE_MODEL", "MiniMax-M3.1-Flash-Preview"),
            openai_key: api_key(Provider::OpenAI),
            // Lab winner (llm-cost lab): cheapest model that passed voice + point-and-ask.
            openai_model: model_setting("OPENAI_MODEL", "gpt-6-luna"),
            openai_url: env_or("OPENAI_BASE_URL", "https://api.openai.com/v1"),
            openai_reasoning: crate::manifest::setting("OPENAI_REASONING", "none"),
            gemini_key: api_key(Provider::Gemini),
            gemini_model: model_setting("GEMINI_MODEL", "gemini-3.5-flash-lite"),
            anthropic_key: api_key(Provider::Anthropic),
            mistral_key: api_key(Provider::Mistral),
            ollama_url: env_or("OLLAMA_URL", "http://127.0.0.1:11434"),
            ollama_ctx: env_num("OLLAMA_NUM_CTX", 16384),
            ollama_keep_alive: env_or("OLLAMA_KEEP_ALIVE", "30m"),
            ollama_think: env_or("OLLAMA_THINK", "0") == "1",
            claude_bin: crate::claude_cli::find_bin(),
            models: {
                let mut m = default_models();
                if let Some(o) = llm_settings()["models"].as_object() {
                    for (k, v) in o {
                        if let Some(a) = v.as_array().filter(|a| a.len() == 3) {
                            m.insert(k.clone(), [0, 1, 2].map(|i| a[i].as_str().unwrap_or("").to_string()));
                        }
                    }
                }
                m
            },
            thinking_level: crate::manifest::setting("GEMINI_THINKING", "minimal"),
            llm_mode: if env_or("LLM_MODE", "") == "delegate" { LlmMode::Delegate } else { LlmMode::Single },
            delegate_timeout_ms: env_num("DELEGATE_TIMEOUT_MS", 4000),
            // Items injected on a normal turn (memory questions get all `memory_k_deep`). Was read
            // from MEMORY_K_DEEP by mistake (= 10); k 6 / 8 / 10 answered 64.8 / 65.7 / 66.7 % in
            // memlab (noise), so 8 keeps the answers and drops ~100 tokens a turn.
            memory_k: env_num("MEMORY_K", 8),
            memory_k_deep: env_num("MEMORY_K_DEEP", 10),
            sidecar_url: env_or("SIDECAR_URL", "ws://127.0.0.1:47801"),
            vad_threshold: env_num("VAD_THRESHOLD", 0.5),
            vad_silence_ms: env_num("VAD_SILENCE_MS", 250),
            turn_threshold: env_num("TURN_THRESHOLD", 0.9),
            turn_recheck_ms: env_num("TURN_RECHECK_MS", 1000),
            turn_recheck_threshold: env_num("TURN_RECHECK_THRESHOLD", 0.7),
            turn_timeout_ms: env_num("TURN_TIMEOUT_MS", 1500),
            commit_silence_ms: env_num("COMMIT_SILENCE_MS", 0),
            merge_window_ms: env_num("MERGE_WINDOW_MS", 1000),
            min_speech_ms: env_num("MIN_SPEECH_MS", 160),
            barge_in_ms: env_num("BARGE_IN_MS", 300),
            first_chunk_min_words: env_num("FIRST_CHUNK_MIN_WORDS", 3),
            chunk_min_words: env_num("CHUNK_MIN_WORDS", 6),
            first_cut_sentence: env_or("FIRST_CUT", "clause") == "sentence",
            tts_rest_words: env_num("TTS_REST_WORDS", 45),
            tts_rest_wait_ms: env_num("TTS_REST_WAIT_MS", 700),
            tts_whole_wait_ms: env_num("TTS_WHOLE_WAIT_MS", 450),
            tts_tag_split: env_num("TTS_TAG_SPLIT", 0u8) == 1,
            filler_after_ms: env_num("FILLER_AFTER_MS", 1400),
            stall_filler_ms: env_num("STALL_FILLER_MS", 800),
            tts_style: env_or("TTS_STYLE", TTS_STYLE),
            tone_tags: env_num("TONE_TAGS", 1u8) == 1,
            persona: String::new(),
            character: Character::new("sam", &voice_choice()).unwrap_or_else(|| Character::new("sam", "male").unwrap()),
            personas: [("sam", "persona_sam.md")]
                .into_iter()
                .map(|(n, f)| (n.to_string(), std::fs::read_to_string(resources_dir().join(f)).expect("persona file")))
                .collect(),
            consolidate_every: env_num("CONSOLIDATE_EVERY", 10),
            judge_timeout_ms: env_num("JUDGE_TIMEOUT_MS", 250),
            eot_hold_ms: env_num("EOT_HOLD_MS", 900),
            route_min_p: env_num("ROUTE_MIN_P", 0.8),
        };
        let mut cfg = cfg;
        cfg.persona = match std::env::var("PERSONA_FILE") {
            Ok(f) => std::fs::read_to_string(f).expect("PERSONA_FILE"),
            Err(_) => cfg.personas[&cfg.character.name].clone(),
        };
        let has_key = |p: Provider| cfg.key(p).is_some();
        if cfg.provider.needs_key() && !has_key(cfg.provider) {
            // The server still starts: the onboarding collects the key (AI provider step).
            tracing::warn!("{:?} has no API key yet: set it in Sam's onboarding (AI provider)", cfg.provider);
        }
        if cfg.llm_mode == LlmMode::Delegate && !(has_key(Provider::Gemini) && has_key(Provider::MiniMax)) {
            anyhow::bail!("LLM_MODE=delegate needs GEMINI_API_KEY and MINIMAX_API_KEY");
        }
        tracing::info!(?cfg.provider, model = cfg.model_name(cfg.provider), ?cfg.llm_mode, cfg.vad_silence_ms, "starting");

        // One pooled client: keeps the TLS/HTTP2 connection to the provider warm across turns.
        let http = reqwest::Client::builder()
            .pool_idle_timeout(None)
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        let memory = std::sync::OnceLock::new();
        let want_memory = env_num("MEMORY", 1u8) == 1;
        let mcfg = || memory::MemoryConfig {
            data_dir: crate::thinker::data_dir(),
            embed_url: env_or("MIND_URL", "http://127.0.0.1:47802"),
            k: env_num("MEMORY_K", 6),
            decay_weight: env_num("DECAY_W", 0.3),
            half_life_days: env_num("HALF_LIFE_DAYS", 14.0),
            timeout_ms: env_num("MEMORY_TIMEOUT_MS", 300),
            reinforce_weight: env_num("MEMORY_REINFORCE_W", 0.0),
        };
        let mut memory_pending = false;
        if want_memory {
            match memory::Memory::open(mcfg(), http.clone()).await {
                Ok(m) => {
                    let _ = memory.set(m);
                }
                Err(e) => {
                    tracing::warn!("long-term memory not up yet: {e:#}; retrying in the background");
                    memory_pending = true;
                }
            }
        }
        let judge = (env_num("JUDGE", 1u8) == 1).then(|| judge::Judge::new(http.clone(), env_or("MIND_URL", "http://127.0.0.1:47802")));
        let state = Arc::new(AppState { cfg, http, memory, judge, tasks: Default::default() });
        let _ = state.tasks.set(tasks::Tasks::spawn(state.clone()));
        if memory_pending {
            // The sidecar loads Python and a model: keep trying for a while, then give up quietly.
            let app = state.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                while started.elapsed() < Duration::from_secs(600) {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    match memory::Memory::open(mcfg(), app.http.clone()).await {
                        Ok(m) => {
                            if app.memory.set(m).is_ok() {
                                tracing::info!(after_s = started.elapsed().as_secs(), "long-term memory on");
                            }
                            return;
                        }
                        Err(e) if started.elapsed() > Duration::from_secs(60) => tracing::debug!("long-term memory still off: {e:#}"),
                        Err(_) => {}
                    }
                }
                tracing::warn!("long-term memory disabled: embedding sidecar never answered");
            });
        }
        Ok(state)
    }
}
