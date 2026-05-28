//! Friendly task-id hash: minting + validation.

/// Minimum minted hash length. Minting always starts here and only
/// grows on repeated collisions.
pub const MIN_HASH_LEN: usize = 3;

/// Maximum minted hash length. Both mint paths (runtime `task create`
/// and the `open_db` backfill) cap growth here, and [`is_valid_hash`]
/// accepts up to this length — keeping the validator and the minters
/// in lockstep so a grown hash can never become unresolvable. 16 chars
/// of base32 is 32^16 ≈ 10^24 values; reaching this length is
/// effectively impossible in practice. A single UUID yields up to 25
/// base32 chars of entropy, so one draw covers it.
pub const MAX_HASH_LEN: usize = 16;

/// `^[a-z2-7]{MIN..=MAX}$` — the shape of a minted hash. Used by the
/// resolver to reject obviously-malformed input (wrong case, illegal
/// chars, a truncated UUID's trailing group) with a clear "bad id"
/// error rather than a misleading "task hash not found". The bounds
/// match the minters so any hash the system can persist also resolves.
pub fn is_valid_hash(s: &str) -> bool {
    let len = s.chars().count();
    (MIN_HASH_LEN..=MAX_HASH_LEN).contains(&len)
        && s.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7'))
}

/// Generate a random lowercase RFC 4648 base32 string of the given
/// length. Backs the friendly task ID minting: the persistence layer
/// retries with new randomness on `UNIQUE` index collisions and grows
/// the requested length once the failure rate at a given length climbs.
///
/// Uses a fresh UUID's bytes as the entropy source — keeps the
/// dependency tree small (no extra `rand` crate) and reuses the
/// randomness primitive that already mints `TaskId`s. One UUID supplies
/// up to 25 base32 chars of entropy (16 bytes × 8 / 5), which covers
/// the full `MIN_HASH_LEN..=MAX_HASH_LEN` range callers consume.
pub fn random_lowercase_base32(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(length);
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    // One UUID yields 128 bits ≈ 25 base32 chars. Draw additional UUIDs
    // as needed so the function always returns exactly `length` chars,
    // even past 25 — otherwise it would silently underfill and break
    // the length-growth collision strategy.
    while out.len() < length {
        for &b in uuid::Uuid::new_v4().as_bytes() {
            acc = (acc << 8) | (b as u64);
            bits += 8;
            while bits >= 5 && out.len() < length {
                bits -= 5;
                let idx = ((acc >> bits) & 0b11111) as usize;
                out.push(ALPHABET[idx] as char);
            }
            if out.len() >= length {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_hash_accepts_minted_shapes_rejects_junk() {
        assert!(is_valid_hash("ev6"));
        assert!(is_valid_hash("ak7"));
        // Grown hashes (up to MAX_HASH_LEN) must still validate, or a
        // collision-grown hash would become unresolvable.
        assert!(is_valid_hash("abcdefgh")); // 8
        assert!(is_valid_hash("abcdefghijklmnop")); // exactly MAX_HASH_LEN (16)
        // Wrong case, illegal base32 digits (0/1/8/9), wrong length.
        assert!(!is_valid_hash("EV6"));
        assert!(!is_valid_hash("ab")); // below MIN_HASH_LEN
        assert!(!is_valid_hash("abcdefghijklmnopq")); // 17, over MAX_HASH_LEN
        assert!(!is_valid_hash("ev0")); // 0 not in RFC 4648 base32
        assert!(!is_valid_hash("ev1")); // 1 not in RFC 4648 base32
        assert!(!is_valid_hash("ev-")); // hyphen
        assert!(!is_valid_hash(""));
    }

    #[test]
    fn minted_hashes_are_always_valid_hash_shaped() {
        for &length in &[3usize, 4, 5, 8] {
            let s = random_lowercase_base32(length);
            assert!(is_valid_hash(&s), "minted {s:?} failed is_valid_hash");
        }
    }

    #[test]
    fn random_lowercase_base32_fills_past_single_uuid_entropy() {
        // 30 > the ~25 chars a single UUID supplies — the function must
        // draw more entropy rather than underfilling.
        let s = random_lowercase_base32(30);
        assert_eq!(s.chars().count(), 30);
        assert!(s.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
    }

    #[test]
    fn random_lowercase_base32_length_and_alphabet() {
        for &length in &[3usize, 4, 5, 7] {
            let s = random_lowercase_base32(length);
            assert_eq!(
                s.chars().count(),
                length,
                "expected {length} chars, got {s:?}"
            );
            for c in s.chars() {
                assert!(
                    matches!(c, 'a'..='z' | '2'..='7'),
                    "char {c:?} is outside RFC 4648 lowercase base32"
                );
            }
        }
    }

    /// Smoke test: ten draws at length 3 produce more than one distinct
    /// value. (3-char base32 has 32^3 = 32 768 possible values; collisions
    /// across 10 draws would be astronomical bad luck.)
    #[test]
    fn random_lowercase_base32_is_actually_random() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10 {
            seen.insert(random_lowercase_base32(3));
        }
        assert!(seen.len() > 1, "10 length-3 draws produced one value");
    }
}
