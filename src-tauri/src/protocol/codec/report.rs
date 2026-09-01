//! Conversion report produced by every codec direction.

use super::types::CodecId;
use serde::Serialize;
use serde_json::Value;

/// 从 OpenAI 兼容 usage JSON 提取缓存注解（issue #51 缓存命中统计）。
/// 归一化口径：`prompt_tokens_details.cached_tokens`（OpenAI）、
/// `prompt_cache_hit_tokens`（DeepSeek）、`cache_read_input_tokens`
/// （Anthropic 兼容层）都视为缓存读取；`cache_creation_input_tokens`
/// 视为缓存写入。读取/写入均以 >0 视为已上报（0 与缺失对统计等价）。
pub fn cache_fields_from_openai_usage(u: &Value) -> (Option<i64>, Option<i64>) {
    let read = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_i64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_i64))
        .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_i64))
        .filter(|v| *v > 0);
    let creation = u
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0);
    (read, creation)
}

/// What the codec did to the request/response it converted.
/// Token usage observed from a real upstream response.
///
/// `usage_unknown` is set when the upstream did not report a value; callers
/// must never treat `0` as an exact count when this flag is set.  Only the
/// protocol-mandated field (e.g. Anthropic `usage`) emits a compatible `0`,
/// and even then the gateway logs `usage_unknown=true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache tokens are only a billing annotation on the Anthropic side and
    /// are surfaced into OpenAI `usage_details` without double-counting.
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub usage_unknown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversionReport {
    /// Fields rejected (codes + pointers); empty on success.
    pub rejected: Vec<RejectedReportEntry>,
    /// Fields that were normalized (kept semantically, changed representation).
    pub normalized: Vec<String>,
    /// The exact directed codec selected for this conversion.
    pub codec_id: CodecId,
}

#[derive(Debug, Clone, Serialize)]
pub struct RejectedReportEntry {
    pub code: String,
    pub pointer: String,
}

impl ConversionReport {
    pub fn for_codec(
        codec_id: CodecId,
        rejected: Vec<RejectedReportEntry>,
        normalized: Vec<String>,
    ) -> Self {
        Self {
            rejected,
            normalized,
            codec_id,
        }
    }
}

/// Context handed to the response decoder so the response can be expressed in
/// the downstream protocol (message ids, mapped upstream model, stream flag).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConversionContext {
    /// Downstream request id (message id for the messages side, `chatcmpl-` for chat).
    pub request_id: String,
    /// The mapped upstream model as passed into the codec (never re-mapped
    /// inside the codec).
    pub upstream_model: String,
    pub stream: bool,
    /// JSON pointers (e.g. `/container`) of fields the encoder dropped or
    /// transformed in a fail-open way during request encoding.  Populated by
    /// the encoder and surfaced through the [`ConversionReport`].
    pub normalized: Vec<String>,
}

impl ConversionContext {
    pub fn new(
        request_id: impl Into<String>,
        upstream_model: impl Into<String>,
        stream: bool,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            upstream_model: upstream_model.into(),
            stream,
            normalized: Vec::new(),
        }
    }
}
