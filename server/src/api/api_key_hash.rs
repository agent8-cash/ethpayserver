//! SHA-256 hashing for API keys.
//!
//! Shared between `extractors::validate_api_key` (auth path) and
//! `users::build_api_key` (creation path) so both paths can never drift.

/// Hash an API key with SHA-256 and return hex-encoded output.
pub(super) fn hash_api_key(raw_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    hex::encode(hasher.finalize())
}

/// True iff `s` is a 0x-prefixed hex string encoding exactly `n_bytes` bytes.
#[must_use]
pub fn is_hex_bytes(s: &str, n_bytes: usize) -> bool {
    let Some(hex) = s.strip_prefix("0x") else {
        return false;
    };
    n_bytes.checked_mul(2) == Some(hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_deterministic() {
        let a = hash_api_key("ak_abcd_1234567890");
        let b = hash_api_key("ak_abcd_1234567890");
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_produce_different_hashes() {
        let a = hash_api_key("ak_aaa");
        let b = hash_api_key("ak_bbb");
        assert_ne!(a, b);
    }

    #[test]
    fn output_is_64_hex_chars() {
        let h = hash_api_key("ak_test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn is_hex_bytes_accepts_lowercase() {
        assert!(is_hex_bytes("0xdeadbeef", 4));
        assert!(is_hex_bytes(&format!("0x{}", "ab".repeat(32)), 32));
    }

    #[test]
    fn is_hex_bytes_accepts_upper_and_mixed_case() {
        assert!(is_hex_bytes("0xDEADBEEF", 4));
        assert!(is_hex_bytes("0xDeadBeEf", 4));
    }

    #[test]
    fn is_hex_bytes_rejects_too_short() {
        assert!(!is_hex_bytes("0xdeadbe", 4));
    }

    #[test]
    fn is_hex_bytes_rejects_too_long() {
        assert!(!is_hex_bytes("0xdeadbeef00", 4));
    }

    #[test]
    fn is_hex_bytes_rejects_missing_prefix() {
        assert!(!is_hex_bytes("deadbeef", 4));
        assert!(!is_hex_bytes("0Xdeadbeef", 4));
        assert!(!is_hex_bytes("xxdeadbeef", 4));
    }

    #[test]
    fn is_hex_bytes_rejects_non_hex_char() {
        assert!(!is_hex_bytes("0xdeadbeeg", 4));
        assert!(!is_hex_bytes("0xdead-eef", 4));
    }

    #[test]
    fn is_hex_bytes_zero_bytes_needs_bare_prefix() {
        assert!(is_hex_bytes("0x", 0));
        assert!(!is_hex_bytes("0x00", 0));
    }

    #[test]
    fn is_hex_bytes_rejects_empty_string() {
        assert!(!is_hex_bytes("", 0));
        assert!(!is_hex_bytes("", 1));
    }

    #[test]
    fn is_hex_bytes_rejects_multibyte_without_panicking() {
        assert!(!is_hex_bytes("0xé", 1));
        assert!(!is_hex_bytes("0xdeadbeé", 4));
        assert!(!is_hex_bytes("0x日本", 3));
    }

    #[test]
    fn is_hex_bytes_huge_n_bytes_does_not_overflow() {
        assert!(!is_hex_bytes("0x00", usize::MAX));
    }
}
