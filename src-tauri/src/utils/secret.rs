//! 密钥脱敏（FIX-13/FIX-23 共享实现）。

/// 掩码密钥：保留前 4 与后 4 个**字符**（按字符而非字节——多字节密钥按
/// 字节切片会在字符边界 panic，与 issue #55 同类；此前 commands/channel.rs
/// 的 `&key[..4]` 是全仓最后一个可 panic 的 String 字节切片）。
/// 短密钥（≤8 字符）整体打码。
pub fn mask_secret(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}...{}", head, tail)
}

#[cfg(test)]
mod tests {
    use super::mask_secret;

    #[test]
    fn masks_ascii_secret_keeping_head_and_tail() {
        assert_eq!(mask_secret("sk-waliapi-abcdef1234567890"), "sk-w...7890");
    }

    /// 回归（FIX-23/#55 同类）：多字节字符不再按字节切片 panic。
    #[test]
    fn masks_multibyte_secret_without_panic() {
        let masked = mask_secret("密钥密钥密钥密钥密钥abcd密钥efgh");
        assert_eq!(masked, "密钥密钥...efgh");
    }

    #[test]
    fn short_secrets_are_fully_masked() {
        assert_eq!(mask_secret(""), "****");
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("12345678"), "****");
        assert_eq!(mask_secret("123456789"), "1234...6789");
    }
}
