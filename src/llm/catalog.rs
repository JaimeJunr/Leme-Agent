//! OpenRouter model catalog (context sizes, pricing, capabilities), cached on disk.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const CACHE_TTL: Duration = Duration::from_secs(12 * 3600);
const EFFORT_ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_length: u64,
    pub max_output: u64,
    /// $ per token
    pub price_in: f64,
    pub price_out: f64,
    pub price_cache_read: f64,
    pub price_cache_write: f64,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    pub reasoning_mandatory: bool,
    pub reasoning_efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub input_images: bool,
    pub created: u64,
}

impl ModelInfo {
    pub fn from_json(v: &Value) -> Option<ModelInfo> {
        let id = v.get("id")?.as_str()?.to_string();
        let price = |k: &str| -> f64 {
            v.pointer(&format!("/pricing/{k}"))
                .and_then(|p| {
                    p.as_str()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| p.as_f64())
                })
                .unwrap_or(0.0)
                .max(0.0)
        };
        let params: Vec<&str> = v
            .get("supported_parameters")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let reasoning = v.get("reasoning").filter(|r| r.is_object());
        let efforts: Vec<String> = reasoning
            .and_then(|r| r.get("supported_efforts"))
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let modalities: Vec<&str> = v
            .pointer("/architecture/input_modalities")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let context_length = v
            .get("context_length")
            .and_then(|c| c.as_u64())
            .or_else(|| {
                v.pointer("/top_provider/context_length")
                    .and_then(|c| c.as_u64())
            })
            .unwrap_or(128_000);
        Some(ModelInfo {
            name: v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(&id)
                .to_string(),
            context_length,
            max_output: v
                .pointer("/top_provider/max_completion_tokens")
                .and_then(|c| c.as_u64())
                .unwrap_or(0),
            price_in: price("prompt"),
            price_out: price("completion"),
            price_cache_read: price("input_cache_read"),
            price_cache_write: price("input_cache_write"),
            supports_tools: params.contains(&"tools"),
            supports_reasoning: params.contains(&"reasoning") || reasoning.is_some(),
            reasoning_mandatory: reasoning
                .and_then(|r| r.get("mandatory"))
                .and_then(|m| m.as_bool())
                .unwrap_or(false),
            default_effort: reasoning
                .and_then(|r| r.get("default_effort"))
                .and_then(|m| m.as_str())
                .map(String::from),
            reasoning_efforts: efforts,
            input_images: modalities.contains(&"image"),
            created: v.get("created").and_then(|c| c.as_u64()).unwrap_or(0),
            id,
        })
    }

    /// Map a requested effort onto the closest one this model supports.
    pub fn closest_effort(&self, want: &str) -> String {
        if self.reasoning_efforts.is_empty() || self.reasoning_efforts.iter().any(|e| e == want) {
            return want.to_string();
        }
        let rank = |e: &str| EFFORT_ORDER.iter().position(|x| *x == e).unwrap_or(4) as i32;
        let w = rank(want);
        self.reasoning_efforts
            .iter()
            .min_by_key(|e| {
                let d = rank(e) - w;
                // prefer rounding up on ties
                (d.abs() * 2) - if d > 0 { 1 } else { 0 }
            })
            .cloned()
            .unwrap_or_else(|| want.to_string())
    }

    /// Estimated cost of a call, used when the API does not report `cost`.
    pub fn estimate_cost(
        &self,
        prompt: u64,
        cached: u64,
        cache_write: u64,
        completion: u64,
    ) -> f64 {
        let uncached = prompt.saturating_sub(cached + cache_write);
        let read_price = if self.price_cache_read > 0.0 {
            self.price_cache_read
        } else {
            self.price_in
        };
        let write_price = if self.price_cache_write > 0.0 {
            self.price_cache_write
        } else {
            self.price_in
        };
        uncached as f64 * self.price_in
            + cached as f64 * read_price
            + cache_write as f64 * write_price
            + completion as f64 * self.price_out
    }

    /// Anthropic/Qwen (and Gemini explicit caching) need `cache_control`
    /// breakpoints; OpenAI, DeepSeek, Grok, Moonshot, … cache automatically.
    pub fn wants_cache_control(id: &str) -> bool {
        let id = id.trim_start_matches('~');
        id.starts_with("anthropic/") || id.starts_with("qwen/") || id.starts_with("google/gemini")
    }
}

#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub models: HashMap<String, ModelInfo>,
}

impl Catalog {
    fn cache_path() -> PathBuf {
        crate::config::cache_dir().join("models.json")
    }

    pub fn from_value(v: &Value) -> Catalog {
        let mut models = HashMap::new();
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for m in arr {
                if let Some(info) = ModelInfo::from_json(m) {
                    models.insert(info.id.clone(), info);
                }
            }
        }
        Catalog { models }
    }

    /// Load from disk cache; returns (catalog, is_stale).
    pub fn load_cached() -> (Catalog, bool) {
        let p = Self::cache_path();
        let stale = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > CACHE_TTL)
            .unwrap_or(true);
        match std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            Some(v) => (Self::from_value(&v), stale),
            None => (Catalog::default(), true),
        }
    }

    pub async fn fetch(http: &reqwest::Client, base_url: &str) -> Result<Catalog> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let v: Value = http
            .get(url)
            .timeout(Duration::from_secs(20))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let p = Self::cache_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, v.to_string());
        Ok(Self::from_value(&v))
    }

    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        if let Some(m) = self.models.get(id) {
            return Some(m);
        }
        // Variants like `:nitro`, `:floor`, `:online`, `:exacto` share base info.
        let base = id.split(':').next().unwrap_or(id);
        self.models.get(base)
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Fuzzy search by id/name; all whitespace-separated terms must match.
    pub fn search(&self, query: &str) -> Vec<&ModelInfo> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let mut v: Vec<&ModelInfo> = self
            .models
            .values()
            .filter(|m| !m.id.ends_with(":batch"))
            .filter(|m| {
                let hay = format!("{} {}", m.id, m.name).to_lowercase();
                terms.iter().all(|t| hay.contains(t.as_str()))
            })
            .collect();
        v.sort_by(|a, b| {
            let q = query.to_lowercase();
            let ea = (a.id.to_lowercase() == q) as u8;
            let eb = (b.id.to_lowercase() == q) as u8;
            eb.cmp(&ea)
                .then(b.created.cmp(&a.created))
                .then(a.id.cmp(&b.id))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({"data":[{
            "id":"anthropic/claude-sonnet-5.5","name":"Claude Sonnet 5.5","context_length":1000000,
            "pricing":{"prompt":"0.000002","completion":"0.00001","input_cache_read":"0.0000002","input_cache_write":"0.0000025"},
            "supported_parameters":["tools","reasoning"],
            "reasoning":{"mandatory":true,"supported_efforts":["max","xhigh","high","medium","low"],"default_effort":"high"},
            "architecture":{"input_modalities":["text","image"]},
            "top_provider":{"max_completion_tokens":128000}
        }]})
    }

    #[test]
    fn parses_model() {
        let c = Catalog::from_value(&sample());
        let m = c.get("anthropic/claude-sonnet-5.5:nitro").unwrap();
        assert_eq!(m.context_length, 1_000_000);
        assert!(
            m.supports_tools && m.supports_reasoning && m.input_images && m.reasoning_mandatory
        );
        assert!((m.price_in - 2e-6).abs() < 1e-12);
        assert_eq!(m.max_output, 128000);
        assert_eq!(m.closest_effort("minimal"), "low");
        assert_eq!(m.closest_effort("high"), "high");
        let cost = m.estimate_cost(1_000_000, 0, 0, 0);
        assert!((cost - 2.0).abs() < 1e-9);
    }

    #[test]
    fn search_matches_terms() {
        let c = Catalog::from_value(&sample());
        assert_eq!(c.search("sonnet 5.5").len(), 1);
        assert!(c.search("gpt").is_empty());
    }
}
