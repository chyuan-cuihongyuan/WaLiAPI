pub mod claude;
pub mod custom;
pub mod deepseek;
pub mod gemini;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Connect-timeout (10 s) shared by all clients.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Blocking-client 分桶上限：超时值来自渠道配置，实际取值集合很小；
/// 上限只防病态配置撑爆缓存。
const BLOCKING_CLIENT_MAX_BUCKETS: usize = 32;

/// FIX-21：流式客户端进程内单例——每个连接池/线程池只建一次，
/// 高流量下不再每请求重建（reqwest::Client clone 是廉价的 Arc 复制）。
static STREAMING_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// FIX-21：阻塞客户端按总超时分桶复用（同超时共享连接池）。
static BLOCKING_CLIENTS: OnceLock<Mutex<HashMap<u64, reqwest::Client>>> = OnceLock::new();

fn blocking_client_map() -> &'static Mutex<HashMap<u64, reqwest::Client>> {
    BLOCKING_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build a reqwest client for **non-streaming** requests: the total request
/// duration (connect + send + receive) is capped at `timeout_secs`.
///
/// FIX-21：按 `timeout_secs` 分桶缓存复用；构建失败降级为无总超时的
/// 兜底客户端并打日志（与旧行为一致，但只告警一次桶）。
pub fn blocking_client(timeout_secs: u64) -> reqwest::Client {
    let key = timeout_secs.max(1);
    let mut buckets = blocking_client_map().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(client) = buckets.get(&key) {
        return client.clone();
    }
    match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(key))
        .build()
    {
        Ok(client) => {
            if buckets.len() >= BLOCKING_CLIENT_MAX_BUCKETS {
                // 病态配置兜底：不缓存，直接返回（每次构建，但不会内存膨胀）。
                return client;
            }
            buckets.insert(key, client.clone());
            client
        }
        Err(e) => {
            tracing::warn!("blocking client build failed (timeout={key}s), falling back: {e}");
            reqwest::Client::new()
        }
    }
}

/// Build a reqwest client for **streaming** (SSE) requests: only the TCP
/// connection establishment is capped at [`CONNECT_TIMEOUT_SECS`]; the
/// response body is allowed to stream indefinitely so long LLM generations
/// are not cut off by a premature total-timeout.
///
/// FIX-21：进程内单例；构建失败降级默认客户端并打日志。
pub fn streaming_client() -> reqwest::Client {
    STREAMING_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("streaming client build failed, falling back: {e}");
                    reqwest::Client::new()
                })
        })
        .clone()
}

#[cfg(test)]
mod client_reuse_tests {
    use super::*;

    /// 同超时桶只建一个客户端；不同超时分桶。键取不常见值并在锁内做
    /// 相对增长断言，避免与其他并行测试共享全局映射造成偶发失败。
    #[test]
    fn blocking_clients_are_bucketed_by_timeout() {
        let before = {
            let map = blocking_client_map().lock().unwrap_or_else(|e| e.into_inner());
            map.len()
        };
        let _a = blocking_client(12345);
        let _b = blocking_client(12345);
        {
            let map = blocking_client_map().lock().unwrap_or_else(|e| e.into_inner());
            assert!(map.contains_key(&12345), "bucket must be cached");
            assert_eq!(map.len(), before + 1, "same timeout must share one bucket");
        }
        let _c = blocking_client(12346);
        {
            let map = blocking_client_map().lock().unwrap_or_else(|e| e.into_inner());
            assert!(map.contains_key(&12346));
            assert_eq!(map.len(), before + 2, "different timeout gets its own bucket");
        }
    }

    /// 零/越界超时归一到 1s 桶（超时语义与旧实现一致）。
    #[test]
    fn blocking_client_normalizes_timeout_key() {
        let _a = blocking_client(0);
        let map = blocking_client_map().lock().unwrap_or_else(|e| e.into_inner());
        assert!(map.contains_key(&1), "timeout 0 must normalize to the 1s bucket");
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Prompt tokens served from upstream cache.
    pub cached_tokens: u64,
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
