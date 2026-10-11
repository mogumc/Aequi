//! 随机串生成。

/// 字符集去掉了容易看错的 `0`、`O`、`1`、`l`、`I`。
///
/// 初始账号密码需要人工抄写，可读性比字符集大小更重要。
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 生成指定长度的随机串。
pub fn string(len: usize) -> String {
    (0..len)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_length_and_alphabet() {
        let value = string(32);
        assert_eq!(value.len(), 32);
        assert!(value.bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn produces_different_values() {
        assert_ne!(string(24), string(24));
    }
}
