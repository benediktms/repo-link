//! Free functions for deriving repo names and prefix handles.

/// Last `/`-separated segment of a canonical URL, used as the default
/// short name when a binding is created. For
/// `github.com/benediktms/repo-link` this returns `"repo-link"`. Falls
/// back to the whole input if there's no `/` (a degenerate case for our
/// canonical form, but worth handling defensively).
pub fn derive_name(canonical_url: &str) -> String {
    canonical_url
        .rsplit('/')
        .next()
        .unwrap_or(canonical_url)
        .to_string()
}

/// Words stripped before prefix derivation — they carry no
/// distinguishing information and would waste a slot in the 3-letter
/// composite prefix (e.g. `app-packages` becomes `packages` →`pck`,
/// not `app`). Mirrors brain's noise list so prefix derivation behaves
/// consistently for engineers used to brain.
const PREFIX_NOISE_WORDS: &[&str] = &[
    "app",
    "application",
    "service",
    "svc",
    "server",
    "api",
    "the",
];

/// Derive a 3-letter lowercase prefix from a repo name.
///
/// Algorithm (ported from brain's `generate_prefix`, lowercased to match
/// the spec's `^[a-z][a-z0-9]{1,19}$` regex — the derived value is
/// always exactly 3 chars; the wider bound only applies to manual
/// overrides):
/// 1. Split on `-`, `_`, space.
/// 2. Drop pure-numeric segments (`02`, `v3` stays — only fully numeric
///    pieces are dropped).
/// 3. Drop noise words (`app`, `service`, `api`, …).
/// 4. Multi-word (3+): first letter of the first 3 unique segments.
/// 5. Two segments: first letter of each + first consonant of the longer.
/// 6. Single word: first char + next two consonants.
/// 7. Pad with `x` to exactly 3 alphabetic chars.
///
/// Deterministic and pure — collision-breaking lives at the application
/// layer (the persistence service appends `1`/`2`/… on duplicate insert).
pub fn derive_prefix(name: &str) -> String {
    let segments: Vec<&str> = name
        .split(['-', '_', ' '])
        .filter(|s| !s.is_empty())
        .filter(|s| !is_pure_numeric(s))
        .filter(|s| !PREFIX_NOISE_WORDS.contains(&s.to_ascii_lowercase().as_str()))
        .collect();

    let raw = match segments.len() {
        0 => "rlk".to_string(),
        1 => prefix_from_single_word(segments[0]),
        2 => prefix_from_two_words(segments[0], segments[1]),
        _ => prefix_from_multi_words(&segments),
    };

    let lower = raw.to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    match chars.len() {
        0 => "rlk".to_string(),
        1 => format!("{}xx", chars[0]),
        2 => format!("{}{}x", chars[0], chars[1]),
        _ => chars[..3].iter().collect(),
    }
}

fn is_pure_numeric(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_digit())
}

fn is_vowel(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u')
}

fn consonants_after_first(word: &str) -> Vec<char> {
    word.chars()
        .skip(1)
        .filter(|c| c.is_ascii_alphabetic() && !is_vowel(*c))
        .collect()
}

fn prefix_from_single_word(word: &str) -> String {
    let first = word.chars().next().unwrap_or('x');
    let mut result = vec![first];
    for c in consonants_after_first(word) {
        if result.len() >= 3 {
            break;
        }
        result.push(c);
    }
    if result.len() < 3 {
        // Pad from the remaining alphabetic chars (vowels included).
        // Duplicates are acceptable here — a 3-char prefix from a short
        // single word (e.g. "aa" → "aax") just needs to reach length 3;
        // global uniqueness is enforced separately by the suffix-on-
        // collision retry at the persistence layer.
        for c in word.chars().skip(1).filter(|c| c.is_ascii_alphabetic()) {
            result.push(c);
            if result.len() >= 3 {
                break;
            }
        }
    }
    result.into_iter().collect()
}

fn prefix_from_two_words(a: &str, b: &str) -> String {
    let first_a = a.chars().next().unwrap_or('x');
    let first_b = b.chars().next().unwrap_or('x');
    let a_is_longer = a.len() >= b.len();
    let longer = if a_is_longer { a } else { b };
    let extra = consonants_after_first(longer)
        .first()
        .copied()
        .unwrap_or_else(|| longer.chars().nth(1).unwrap_or('x'));
    if a_is_longer {
        format!("{}{}{}", first_a, extra, first_b)
    } else {
        format!("{}{}{}", first_a, first_b, extra)
    }
}

/// `^[a-z][a-z0-9]{1,19}$` — 2-20 chars, starts with a letter,
/// lowercase alnum. Auto-derived prefixes are always 3 chars (see
/// [`derive_prefix`]); the wider ceiling exists only for manual
/// overrides via `set_prefix` / `repo attach --prefix`, where a user
/// may legitimately want a longer, more descriptive handle. The 20-char
/// cap is an arbitrary safety bound to keep composite IDs typeable.
/// Empty strings are rejected; an unset prefix lives as `""` in storage
/// but never reaches this check.
pub fn is_valid_prefix(p: &str) -> bool {
    let len = p.chars().count();
    if !(2..=20).contains(&len) {
        return false;
    }
    let mut chars = p.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn prefix_from_multi_words(segments: &[&str]) -> String {
    let mut result = Vec::new();
    let mut seen = Vec::new();
    for seg in segments {
        if result.len() >= 3 {
            break;
        }
        if let Some(c) = seg.chars().next() {
            let lower = c.to_ascii_lowercase();
            if !seen.contains(&lower) {
                result.push(c);
                seen.push(lower);
            }
        }
    }
    if result.len() < 3 {
        for seg in segments {
            if result.len() >= 3 {
                break;
            }
            for c in consonants_after_first(seg) {
                let lower = c.to_ascii_lowercase();
                if !seen.contains(&lower) {
                    result.push(c);
                    seen.push(lower);
                    break;
                }
            }
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_name_from_canonical() {
        // Use this project's own canonical URL as the primary case so the
        // test doubles as a sanity check on the format we actually store.
        assert_eq!(derive_name("github.com/benediktms/repo-link"), "repo-link");
        // Deeper paths still take only the last segment.
        assert_eq!(derive_name("gitlab.com/group/sub/project"), "project");
        // Degenerate single-segment input falls through to the input itself.
        assert_eq!(derive_name("just-a-name"), "just-a-name");
    }

    #[test]
    fn derive_prefix_repo_link() {
        // `repo-link` is two segments of equal length; the `a_is_longer`
        // tie-breaker uses >= so `repo` wins. Format is `first_a + extra
        // + first_b` where `extra` is the first consonant of the longer:
        // 'r' + 'p' (from "repo") + 'l' = "rpl". (The spec body wrote
        // `rlk` as a stylised example; the deterministic algorithm
        // produces `rpl` — explicit overrides via `set_prefix` or the
        // upcoming `--prefix` flag let users pick a different value.)
        assert_eq!(derive_prefix("repo-link"), "rpl");
    }

    #[test]
    fn derive_prefix_strips_noise_words() {
        // `service` is noise; remaining single-word `auth` → a + th = ath.
        assert_eq!(derive_prefix("auth-service"), "ath");
        // `app` stripped → single-word `packages` → p + ck = pck.
        assert_eq!(derive_prefix("app-packages"), "pck");
    }

    #[test]
    fn derive_prefix_pads_short_input() {
        // Single letter pads to 3.
        assert_eq!(derive_prefix("a"), "axx");
        // Empty falls back to "rlk".
        assert_eq!(derive_prefix(""), "rlk");
        // Pure-numeric segments dropped → empty → fallback.
        assert_eq!(derive_prefix("123-456"), "rlk");
    }

    #[test]
    fn derive_prefix_multi_word_dedup() {
        // 3 unique first letters.
        assert_eq!(derive_prefix("my-cool-project"), "mcp");
        // Underscores work the same as hyphens.
        assert_eq!(derive_prefix("my_cool_project"), "mcp");
    }

    #[test]
    fn is_valid_prefix_accepts_spec_examples() {
        assert!(is_valid_prefix("rlk"));
        assert!(is_valid_prefix("ath"));
        // Longer manual prefixes are allowed (derived ones are always 3).
        assert!(is_valid_prefix("abcdefgh"));
        assert!(is_valid_prefix("mylongprefix")); // 12 chars
        assert!(is_valid_prefix("abcdefghijklmnopqrst")); // exactly 20
        // Digits allowed after the first char.
        assert!(is_valid_prefix("rl1"));
    }

    #[test]
    fn is_valid_prefix_rejects_bad_shapes() {
        assert!(!is_valid_prefix("")); // too short
        assert!(!is_valid_prefix("a")); // too short
        assert!(!is_valid_prefix("abcdefghijklmnopqrstu")); // 21 chars, over cap
        assert!(!is_valid_prefix("RLK")); // uppercase
        assert!(!is_valid_prefix("1rl")); // leading digit
        assert!(!is_valid_prefix("rl-k")); // hyphen
    }
}
