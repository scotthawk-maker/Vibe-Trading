//! Shared input-validation helpers used across the trading bot.
//!
//! Provides:
//! - [`is_valid_solana_pubkey`]: base58 / 32-byte check used before passing
//!   mint strings to `gmgn-cli`, SQLite, or the actor registry (H5, H7).
//! - [`constant_time_eq`]: constant-time byte comparison used by the webhook
//!   auth gate to prevent timing attacks on the auth token (C2).

/// Returns `true` iff `s` decodes from base58 to exactly 32 bytes — the length
/// of a Solana `Pubkey`.  This is the minimum sanity check applied to every
/// mint received from an untrusted source (webhook payloads, gmgn-cli output).
pub fn is_valid_solana_pubkey(s: &str) -> bool {
    bs58::decode(s)
        .into_vec()
        .map(|v| v.len() == 32)
        .unwrap_or(false)
}

/// Compare two byte slices in constant time.
///
/// Returns `false` immediately if the lengths differ (length is not a secret
/// here — the expected token length is fixed by the configured value).  The
/// per-byte loop runs for the full length regardless of where the first
/// mismatch occurs, preventing timing-based token recovery.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_pubkey() {
        // A real Solana mint (USDC) — 32-byte base58.
        assert!(is_valid_solana_pubkey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"));
    }

    #[test]
    fn invalid_pubkey_rejected() {
        assert!(!is_valid_solana_pubkey(""));
        assert!(!is_valid_solana_pubkey("not-a-pubkey"));
        assert!(!is_valid_solana_pubkey("short"));
    }

    #[test]
    fn cte_matches() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}