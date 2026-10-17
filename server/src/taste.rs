//! What the suggestion engine learns (suggestions lab, 2026-10-13): a linear
//! Thompson-sampling scorer over a few interpretable features (place / event kind, evening,
//! weekend, near, cheap, social, novelty) plus a gate that says how much the LLM's own ranking
//! may count (its running hit rate). One reaction a day is enough: the prior (tastes from the
//! onboarding, the LLM) carries the first months, the learned part overtakes it after a few
//! months, and the gate keeps a wrong prior from steering. All state is a small JSON file.

use serde::{Deserialize, Serialize};

pub const KINDS: [&str; 10] = ["cafe", "restaurant", "concert", "exhibition", "market", "outdoor", "sport", "shop", "gift", "other"];
pub const D: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Taste {
    /// A = λI + Σ x xᵀ (row-major), b = Σ r x.
    pub a: Vec<f64>,
    pub b: Vec<f64>,
    /// Exponentially averaged hit rate of the LLM's ranking (did its top pick get a yes?).
    pub llm_hits: f64,
    pub updates: u32,
}

impl Default for Taste {
    fn default() -> Self {
        let mut a = vec![0.0; D * D];
        for i in 0..D {
            a[i * D + i] = 1.0;
        }
        Self { a, b: vec![0.0; D], llm_hits: 0.5, updates: 0 }
    }
}

/// A candidate's features: kind one-hot + evening, weekend, near (< 2 km), cheap, social, novel.
pub fn features(kind: &str, evening: bool, weekend: bool, near: bool, cheap: bool, social: bool, novel: bool) -> [f64; D] {
    let mut x = [0.0; D];
    let k = KINDS.iter().position(|k| *k == kind).unwrap_or(KINDS.len() - 1);
    x[k] = 1.0;
    for (i, v) in [evening, weekend, near, cheap, social, novel].into_iter().enumerate() {
        x[KINDS.len() + i] = v as u8 as f64;
    }
    x
}

/// Solve A y = v for a symmetric positive-definite A (Gaussian elimination, D is tiny).
fn solve(a: &[f64], v: &[f64]) -> Vec<f64> {
    let n = v.len();
    let mut m: Vec<f64> = a.to_vec();
    let mut y: Vec<f64> = v.to_vec();
    for i in 0..n {
        let p = m[i * n + i].max(1e-9);
        for j in (i + 1)..n {
            let f = m[j * n + i] / p;
            if f != 0.0 {
                for k in i..n {
                    m[j * n + k] -= f * m[i * n + k];
                }
                y[j] -= f * y[i];
            }
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut s = y[i];
        for k in (i + 1)..n {
            s -= m[i * n + k] * x[k];
        }
        x[i] = s / m[i * n + i].max(1e-9);
    }
    x
}

impl Taste {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).filter(|t: &Taste| t.a.len() == D * D && t.b.len() == D).unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) {
        let _ = std::fs::write(path, serde_json::to_string(self).unwrap_or_default());
    }

    /// Posterior mean of the taste weights.
    pub fn mean(&self) -> Vec<f64> {
        solve(&self.a, &self.b)
    }

    /// One Thompson sample: the mean plus noise shaped by the posterior (diagonal
    /// approximation of A⁻¹, enough for ranking a dozen candidates).
    pub fn sample(&self, seed: u64) -> Vec<f64> {
        let mean = self.mean();
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut gauss = || {
            // Box-Muller on a 64-bit LCG: no crate, no cryptography needed.
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u1 = ((s >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u2 = (s >> 11) as f64 / (1u64 << 53) as f64;
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        };
        (0..D).map(|i| mean[i] + 0.6 * gauss() / self.a[i * D + i].sqrt()).collect()
    }

    /// How much the LLM's own ranking counts: 0 when it has been wrong, up to 1 when right.
    pub fn gate(&self) -> f64 {
        ((self.llm_hits - 0.15) / 0.5).clamp(0.0, 1.0)
    }

    /// Score candidates: Thompson sample · x plus the gated LLM rank score (1.0 for its first
    /// pick, down to 0), minus a repeat penalty for things suggested in the last two weeks.
    pub fn score(&self, x: &[f64; D], llm_rank_score: f64, repeat: bool, seed: u64) -> f64 {
        let w = self.sample(seed);
        let dot: f64 = w.iter().zip(x.iter()).map(|(a, b)| a * b).sum();
        dot + self.gate() * 0.7 * llm_rank_score - if repeat { 2.0 } else { 0.0 }
    }

    /// What the user did with a suggestion. `elsewhere`: the features of what they chose
    /// instead (a positive observation of its own).
    pub fn learn(&mut self, x: &[f64; D], signal: &str, llm_top: bool, elsewhere: Option<&[f64; D]>) {
        let mut obs = |x: &[f64; D], r: f64, weight: f64| {
            for i in 0..D {
                for j in 0..D {
                    self.a[i * D + j] += weight * x[i] * x[j];
                }
                self.b[i] += weight * r * x[i];
            }
            self.updates += 1;
        };
        match signal {
            "accepted" => { obs(x, 1.0, 1.0); self.llm_hits = 0.9 * self.llm_hits + 0.1 * llm_top as u8 as f64; }
            // Said no: a real negative, and the LLM's pick was wrong if it was its top one.
            "declined" => { obs(x, 0.0, 1.0); self.llm_hits = 0.9 * self.llm_hits + 0.1 * (!llm_top) as u8 as f64; }
            // Took it up but nothing happened: taste was fine, the plan wasn't (a weak negative).
            "accepted_not_gone" => obs(x, 0.3, 0.6),
            // Did something else instead: that something is the positive; the suggestion a weak negative.
            "elsewhere" => { if let Some(e) = elsewhere { obs(e, 1.0, 1.0); } obs(x, 0.0, 0.5); }
            // Ignored (busy) and stayed home are context, not taste: nothing to learn here.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_a_preference_from_a_few_reactions() {
        let mut t = Taste::default();
        let cafe = features("cafe", false, true, true, true, false, false);
        let concert = features("concert", true, true, false, false, true, true);
        for _ in 0..6 {
            t.learn(&cafe, "accepted", true, None);
            t.learn(&concert, "declined", true, None);
        }
        let m = t.mean();
        let dot = |x: &[f64; D]| m.iter().zip(x.iter()).map(|(a, b)| a * b).sum::<f64>();
        assert!(dot(&cafe) > 0.6, "cafe score {}", dot(&cafe));
        assert!(dot(&concert) < 0.2, "concert score {}", dot(&concert));
        assert!(t.gate() > 0.5);
        // Scores follow: the café beats the concert with any sample.
        let (mut wins, n) = (0, 20);
        for s in 0..n {
            if t.score(&cafe, 0.5, false, s) > t.score(&concert, 1.0, false, s) { wins += 1; }
        }
        assert!(wins >= 17, "{wins}/{n}");
    }

    #[test]
    fn elsewhere_rewards_the_alternative() {
        let mut t = Taste::default();
        let hike = features("outdoor", false, true, false, true, false, true);
        let market = features("market", false, true, true, true, true, false);
        t.learn(&hike, "elsewhere", true, Some(&market));
        let m = t.mean();
        assert!(m[4] > m[5], "market {} outdoor {}", m[4], m[5]);
    }

    #[test]
    fn solve_matches_identity() {
        let t = Taste::default();
        let m = t.mean();
        assert!(m.iter().all(|v| v.abs() < 1e-9));
    }
}
