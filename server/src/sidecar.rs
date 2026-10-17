//! Client for the Python MLX speech sidecar (STT + streaming TTS). See sidecar/speech_server.py
//! for the wire protocol.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Context, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{Bytes, Message};

pub enum Event {
    Audio { id: u32, pcm: Bytes },
    TtsDone { id: u32, ttfa_ms: u64 },
    Closed,
}

pub struct Sidecar {
    tx: mpsc::UnboundedSender<Message>,
    pending_stt: Arc<Mutex<HashMap<u32, oneshot::Sender<(String, u64)>>>>,
    pending_turn: Arc<Mutex<HashMap<u32, oneshot::Sender<f32>>>>,
    next_id: AtomicU32,
    pub sample_rate: u32,
    /// Verified filler takes per voice (clone model). Empty = no list, any filler may be spoken.
    pub fillers: HashMap<String, Vec<String>>,
}

impl Sidecar {
    pub async fn connect(url: &str, events: mpsc::UnboundedSender<Event>) -> anyhow::Result<Arc<Self>> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.context("connect sidecar")?;
        let (mut sink, mut stream) = ws.split();

        let ready: Value = match stream.next().await {
            Some(Ok(Message::Text(t))) => serde_json::from_str(&t)?,
            other => return Err(anyhow!("sidecar handshake failed: {other:?}")),
        };
        let sample_rate = ready["sample_rate"].as_u64().unwrap_or(24000) as u32;
        let fillers: HashMap<String, Vec<String>> = serde_json::from_value(ready["fillers"].clone()).unwrap_or_default();

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            // The session ended (Sidecar dropped): close the socket properly, otherwise the
            // read half keeps the connection open and the sidecar never unloads its models.
            let _ = sink.close().await;
        });
        // Models load on demand: a voice session starting is the moment to warm STT + TTS, so
        // loading (≈1–3 s) overlaps the user's first words.
        let _ = tx.send(Message::Text(json!({"op": "warm"}).to_string().into()));

        let pending_stt: Arc<Mutex<HashMap<u32, oneshot::Sender<(String, u64)>>>> = Default::default();
        let pending = pending_stt.clone();
        let pending_turn: Arc<Mutex<HashMap<u32, oneshot::Sender<f32>>>> = Default::default();
        let pending_t = pending_turn.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    Message::Binary(b) if b.len() > 5 && b[0] == 2 => {
                        let id = u32::from_le_bytes(b[1..5].try_into().unwrap());
                        let _ = events.send(Event::Audio { id, pcm: b.slice(5..) });
                    }
                    Message::Text(t) => {
                        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                        let id = v["id"].as_u64().unwrap_or(0) as u32;
                        match v["op"].as_str() {
                            Some("stt") => {
                                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                    let text = v["text"].as_str().unwrap_or_default().to_string();
                                    let _ = tx.send((text, v["ms"].as_u64().unwrap_or(0)));
                                }
                            }
                            Some("turn") => {
                                if let Some(tx) = pending_t.lock().unwrap().remove(&id) {
                                    let _ = tx.send(v["p"].as_f64().unwrap_or(1.0) as f32);
                                }
                            }
                            Some("tts_done") => {
                                let _ = events.send(Event::TtsDone { id, ttfa_ms: v["ttfa_ms"].as_u64().unwrap_or(0) });
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            let _ = events.send(Event::Closed);
        });

        Ok(Arc::new(Self { tx, pending_stt, pending_turn, next_id: AtomicU32::new(1), sample_rate, fillers }))
    }

    fn id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Transcribe a 16 kHz mono utterance. Returns (text, sidecar ms).
    pub async fn stt(&self, pcm: &[i16]) -> anyhow::Result<(String, u64)> {
        let id = self.id();
        let (tx, rx) = oneshot::channel();
        self.pending_stt.lock().unwrap().insert(id, tx);
        let mut frame = Vec::with_capacity(5 + pcm.len() * 2);
        frame.push(1u8);
        frame.extend_from_slice(&id.to_le_bytes());
        for s in pcm {
            frame.extend_from_slice(&s.to_le_bytes());
        }
        self.tx.send(Message::Binary(frame.into()))?;
        Ok(rx.await?)
    }

    /// smart-turn v3: probability that the user has finished speaking, from the turn's audio so far
    /// (16 kHz, include the trailing silence). ~5 ms on the sidecar.
    pub async fn turn(&self, pcm: &[i16]) -> anyhow::Result<f32> {
        let id = self.id();
        let (tx, rx) = oneshot::channel();
        self.pending_turn.lock().unwrap().insert(id, tx);
        let mut frame = Vec::with_capacity(5 + pcm.len() * 2);
        frame.push(3u8);
        frame.extend_from_slice(&id.to_le_bytes());
        for s in pcm {
            frame.extend_from_slice(&s.to_le_bytes());
        }
        self.tx.send(Message::Binary(frame.into()))?;
        Ok(rx.await?)
    }

    /// Queue a TTS job; audio arrives as `Event::Audio` with the returned id.
    /// `voice` is a profile in voices/<id>/ (cloned, consistent across pieces); `instruct` is the
    /// style instruction, which only CustomVoice models use.
    pub fn tts(&self, text: &str, voice: &str, instruct: Option<&str>, first: bool) -> u32 {
        let id = self.id();
        // `first`: the opening piece of a reply; the sidecar streams it for latency where it
        // would otherwise make a short piece whole (voice-glitch lab).
        let mut m = json!({"op": "tts", "id": id, "text": text, "voice": voice, "first": first});
        if let Some(i) = instruct {
            m["instruct"] = json!(i);
        }
        let _ = self.tx.send(Message::Text(m.to_string().into()));
        id
    }

    /// The user started speaking: a short throwaway take in the reply's voice, so the reply's
    /// first piece meets a warm model. The sidecar skips it when the voice spoke recently and
    /// cuts it short when the turn's STT or a real piece arrives.
    pub fn prewarm(&self, voice: &str) {
        let _ = self.tx.send(Message::Text(json!({"op": "prewarm", "voice": voice}).to_string().into()));
    }

    pub fn cancel(&self, id: u32) {
        let _ = self.tx.send(Message::Text(json!({"op": "cancel", "id": id}).to_string().into()));
    }
}
