//! 登录令牌的生成与散列。

use sha2::{Digest, Sha256};

use crate::utils::random;

/// 令牌长度（字符）。
const TOKEN_LEN: usize = 48;
/// 会话 ID 长度（字符）。
const SESSION_ID_LEN: usize = 16;

/// 生成一个新令牌。
///
/// 明文只在登录响应里出现一次，之后系统只认散列。
pub fn generate() -> String {
    random::string(TOKEN_LEN)
}

/// 生成一个新会话 ID。
pub fn generate_session_id() -> String {
    random::string(SESSION_ID_LEN)
}

/// 令牌的 SHA-256 散列（十六进制）。
///
/// 令牌本身是高熵随机串，用快速散列即可，无需慢哈希。
pub fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let token = "demo-token";
        let digest = hash(token);
        assert_eq!(digest, hash(token));
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_tokens_hash_differently() {
        assert_ne!(hash("demo-token-a"), hash("demo-token-b"));
    }

    #[test]
    fn generated_tokens_are_unique() {
        assert_ne!(generate(), generate());
        assert_ne!(generate_session_id(), generate_session_id());
    }
}
