//! Byte-level SSE framing helpers, shared by both directions.
//!
//! Handles arbitrary network fragmentation: a TCP chunk may split a UTF-8
//! codepoint, an SSE field, or the CRLF/LF record delimiter.  This module is
//! stateless; callers keep their own pending buffer per request.

use crate::protocol::codec::error::UnsupportedFeatures;

/// SSE 解码器 pending 缓冲上限（FIX-11）：故障/恶意上游持续发送无终止符
/// 字节时在上限处报错换渠道，而非无界累积。合法单条 SSE 记录为 KB 量级，
/// 32MB 提供了极大的合法余量。
pub const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

/// pending 缓冲是否已超限（调用方在 `extend_from_slice` 后检查并报错）。
pub fn pending_exceeded(pending: &[u8]) -> bool {
    pending.len() > MAX_PENDING_BYTES
}

/// 超限错误文案（各解码器映射为自己的错误类型时共用）。
pub fn pending_overflow_message() -> String {
    format!(
        "upstream SSE pending buffer exceeded {} MB without a record terminator",
        MAX_PENDING_BYTES / (1024 * 1024)
    )
}

/// Locate the terminating sequence of the next full SSE record.
/// Returns the byte length (including the terminator) of the first complete
/// record, or `None` if a record is not yet complete.
///
/// Both `\r\n\r\n` (CRLF) and `\n\n` (LF) are recognized; a CRLF record
/// spanning several chunks is only emitted once its CRLF terminator arrives,
/// while an LF record is emitted at `\n\n`.
pub fn record_end(input: &[u8]) -> Option<usize> {
    let crlf = input
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4);
    let lf = input.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(c), Some(l)) => Some(c.min(l)),
        (Some(c), None) => Some(c),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    }
}

/// Parse one raw SSE record (already isolated by `record_end`) into its
/// `data:` payload.  `data:` lines are joined with `\n` per the SSE spec.
/// `[DONE]` is returned verbatim so the caller can detect termination.
pub fn parse_data_payload(record: &[u8]) -> Result<String, UnsupportedFeatures> {
    let text = std::str::from_utf8(record).map_err(|_| {
        UnsupportedFeatures::single(
            crate::protocol::codec::error::FeatureKind::UnknownEvent,
            "/",
            "upstream SSE record was not valid UTF-8",
        )
    })?;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    Ok(data.join("\n"))
}

/// Format one downstream SSE event as `event: <name>\ndata: <json>\n\n`.
pub fn event(name: &str, value: serde_json::Value) -> String {
    format!("event: {name}\ndata: {value}\n\n")
}

/// Format a `data:`-only SSE frame (used for OpenAI-style `data: {json}`).
pub fn data_frame(value: serde_json::Value) -> String {
    format!("data: {value}\n\n")
}
