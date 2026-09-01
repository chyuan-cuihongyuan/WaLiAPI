pub mod claude;
pub mod custom;
pub mod deepseek;
pub mod gemini;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Connect-timeout (10 s) shared by all clients.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Build a reqwest client for **non-streaming** requests: the total request
/// duration (connect + send + receive) is capped at `timeout_secs`.
pub fn blocking_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Build a reqwest client for **streaming** (SSE) requests: only the TCP
/// connection establishment is capped at [`CONNECT_TIMEOUT_SECS`]; the
/// response body is allowed to stream indefinitely so long LLM generations
/// are not cut off by a premature total-timeout.
pub fn streaming_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
    pub model_mapping: serde_json::Value,
    pub extra: serde_json::Value,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    pub model: String,
    pub body: serde_json::Value,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// 缓存命中读取的输入 token；`None` = 上游未上报（估算路径/无该字段）。
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// 缓存写入的输入 token（Anthropic cache_creation 等）；`None` = 未上报。
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
}

/// 从 OpenAI 兼容 usage 对象解析 TokenUsage（含缓存归一化字段）。
/// 缓存读取依次尝试：OpenAI `prompt_tokens_details.cached_tokens`、
/// DeepSeek `prompt_cache_hit_tokens`、Anthropic 兼容层的 `cache_read_input_tokens`；
/// 缓存写入读取 `cache_creation_input_tokens`。字段缺失时对应项为 None。
pub fn parse_openai_compatible_usage(u: &serde_json::Value) -> Option<TokenUsage> {
    let prompt = u.get("prompt_tokens")?.as_u64()?;
    let completion = u
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = u
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt + completion);
    let cache_read = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .or_else(|| u.get("cache_read_input_tokens").and_then(|v| v.as_u64()));
    let cache_creation = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());
    Some(TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
    })
}

/// 从 Anthropic 原生 usage 对象解析 TokenUsage（input_tokens 保持窄值，
/// cache_read / cache_creation 单独落列，不再求和混入 prompt）。
pub fn parse_anthropic_usage(u: &serde_json::Value) -> Option<TokenUsage> {
    let input = u.get("input_tokens")?.as_u64()?;
    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = u.get("cache_read_input_tokens").and_then(|v| v.as_u64());
    let cache_creation = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());
    Some(TokenUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
    })
}

/// 从 Gemini usageMetadata 解析 TokenUsage（cachedContentTokenCount 是
/// promptTokenCount 的子集，归一化为缓存读取）。
pub fn parse_gemini_usage(gemini_json: &serde_json::Value) -> Option<TokenUsage> {
    let meta = gemini_json.get("usageMetadata")?;
    let prompt = meta.get("promptTokenCount")?.as_u64()?;
    let completion = meta
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = meta.get("cachedContentTokenCount").and_then(|v| v.as_u64());
    Some(TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cache_read_tokens: cache_read,
        cache_creation_tokens: None,
    })
}

#[async_trait]
pub trait Adaptor: Send + Sync {
    #[allow(dead_code)]
    fn channel_type(&self) -> &'static str;
    #[allow(dead_code)]
    fn default_models(&self) -> Vec<&'static str>;
    #[allow(dead_code)]
    fn default_base_url(&self) -> &str;

    async fn test(&self, config: &ChannelConfig) -> Result<TestResult, anyhow::Error>;

    async fn forward(
        &self,
        request: &ProxyRequest,
        config: &ChannelConfig,
    ) -> Result<(u16, serde_json::Value, Option<TokenUsage>), anyhow::Error>;

    async fn forward_stream(
        &self,
        request: &ProxyRequest,
        config: &ChannelConfig,
    ) -> Result<reqwest::Response, anyhow::Error>;
}

pub fn get_adaptor(channel_type: &str) -> Box<dyn Adaptor> {
    match channel_type {
        "openai" => Box::new(openai::OpenAIAdaptor),
        "deepseek" => Box::new(deepseek::DeepSeekAdaptor),
        "claude" => Box::new(claude::ClaudeAdaptor),
        "gemini" => Box::new(gemini::GeminiAdaptor),
        "custom" => Box::new(custom::CustomAdaptor),
        _ => Box::new(custom::CustomAdaptor),
    }
}

#[cfg(test)]
mod cache_usage_tests {
    use super::*;

    #[test]
    fn openai_compatible_normalizes_cached_tokens_details() {
        let usage = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_tokens_details": { "cached_tokens": 64 }
        });
        let parsed = parse_openai_compatible_usage(&usage).unwrap();
        assert_eq!(parsed.prompt_tokens, 100);
        assert_eq!(parsed.cache_read_tokens, Some(64));
        assert_eq!(parsed.cache_creation_tokens, None);
    }

    #[test]
    fn openai_compatible_deepseek_bare_hit_tokens_fallback() {
        let usage = serde_json::json!({
            "prompt_tokens": 348,
            "completion_tokens": 53,
            "total_tokens": 401,
            "prompt_cache_hit_tokens": 256,
            "prompt_cache_miss_tokens": 92
        });
        let parsed = parse_openai_compatible_usage(&usage).unwrap();
        assert_eq!(parsed.cache_read_tokens, Some(256));
        // miss 不落列（= prompt - read，统计时可推导）。
        assert_eq!(parsed.cache_creation_tokens, None);
    }

    #[test]
    fn openai_compatible_without_cache_reports_none() {
        let usage =
            serde_json::json!({ "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 });
        let parsed = parse_openai_compatible_usage(&usage).unwrap();
        assert_eq!(parsed.cache_read_tokens, None);
        assert_eq!(parsed.cache_creation_tokens, None);
    }

    #[test]
    fn anthropic_usage_keeps_input_narrow_and_splits_cache() {
        let usage = serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 4,
            "cache_creation_input_tokens": 2,
            "cache_read_input_tokens": 3
        });
        let parsed = parse_anthropic_usage(&usage).unwrap();
        // 窄值：不再把 cache 求和混入 prompt（配额口径与 codec 路径统一）。
        assert_eq!(parsed.prompt_tokens, 12);
        assert_eq!(parsed.completion_tokens, 4);
        assert_eq!(parsed.total_tokens, 16);
        assert_eq!(parsed.cache_read_tokens, Some(3));
        assert_eq!(parsed.cache_creation_tokens, Some(2));
    }

    #[test]
    fn gemini_cached_content_token_count_becomes_cache_read() {
        let body = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 1000,
                "candidatesTokenCount": 50,
                "cachedContentTokenCount": 700
            }
        });
        let parsed = parse_gemini_usage(&body).unwrap();
        assert_eq!(parsed.prompt_tokens, 1000);
        assert_eq!(parsed.cache_read_tokens, Some(700));
        assert_eq!(parsed.cache_creation_tokens, None);
    }
}
