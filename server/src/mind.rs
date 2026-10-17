//! Client for the mind sidecar's embeddings (Qwen3-Embedding-0.6B); judgments live in judge.rs.

use std::time::Duration;

use serde_json::{Value, json};

pub async fn embed(http: &reqwest::Client, url: &str, texts: &[String], kind: &str) -> anyhow::Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let r: Value = http
        .post(format!("{url}/embed"))
        .json(&json!({"texts": texts, "kind": kind}))
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(serde_json::from_value(r["vectors"].clone())?)
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
