//! Response decoder ports exposed by the codec facade.

use super::error::DecodeError;
use super::report::Usage;
use serde_json::Value;
use std::ops::Deref;

/// A fully decoded upstream response and the usage observed while decoding it.
///
/// Returning usage with the body prevents consumers from reparsing the raw
/// provider response through protocol-specific side channels.
#[derive(Debug, Clone)]
pub struct DecodedResponse {
    pub body: Value,
    pub usage: Option<Usage>,
}

/// Temporary source compatibility for callers that consumed a decoder result
/// as raw JSON. New consumers should access `.body` explicitly.
impl Deref for DecodedResponse {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

/// Factory-produced decoder for one non-stream upstream response.
pub trait NonStreamDecoder: Send + Sync {
    fn decode(&self, body: &Value) -> Result<DecodedResponse, DecodeError>;
}

/// Factory-produced, stateful SSE decoder for one upstream stream.
pub trait StreamDecoder: Send + Sync {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, DecodeError>;
    fn finish(&mut self) -> Result<Vec<String>, DecodeError>;
    fn usage(&self) -> Option<Usage>;

    /// 上游协议终止标记（[DONE]/message_stop/response.completed）是否已消费。
    /// 转换方向会把终止事件推迟到 finish() 合成，输出扫描看不到——执行器
    /// 依赖本方法在 push 阶段判断「协议层已完成」，不等上游 TCP EOF（#57）。
    /// 默认 false：identity 直通方向的终止标记会原样出现在输出里，
    /// 由泵的输出扫描覆盖。
    fn saw_terminal(&self) -> bool {
        false
    }
}
