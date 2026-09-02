/// 按 UTF-8 字节上限截断字符串，并保证返回值始终落在字符边界上。
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_cjk_at_utf8_boundary() {
        let text = "界".repeat(100);
        let truncated = truncate_utf8(&text, 37);

        assert_eq!(truncated.len(), 36);
        assert!(text.is_char_boundary(truncated.len()));
    }

    #[test]
    fn truncates_emoji_at_utf8_boundary() {
        let text = "a".repeat(15) + "😀尾";
        let truncated = truncate_utf8(&text, 17);

        assert_eq!(truncated, "a".repeat(15));
        assert!(text.is_char_boundary(truncated.len()));
    }

    #[test]
    fn preserves_text_within_limit() {
        assert_eq!(truncate_utf8("WaLiAPI", 7), "WaLiAPI");
        assert_eq!(truncate_utf8("WaLiAPI", 16), "WaLiAPI");
    }
}
