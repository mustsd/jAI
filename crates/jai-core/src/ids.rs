//! Local session identifiers.
//!
//! Ids are 16 hex characters derived from the clock, the process id and a
//! counter. They are not secrets and not globally unique — only unique enough
//! to identify a local session in `~/.jai/jai.db`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new session id.
pub fn session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let mut state = nanos ^ (pid << 32) ^ count.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // SplitMix64 finalizer: spread the bits so ids look random and stay short.
    state ^= state >> 30;
    state = state.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^= state >> 31;
    format!("{state:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_hex_and_distinct() {
        let first = session_id();
        let second = session_id();
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn many_ids_do_not_collide() {
        let ids: std::collections::BTreeSet<String> = (0..5_000).map(|_| session_id()).collect();
        assert_eq!(ids.len(), 5_000);
    }
}
