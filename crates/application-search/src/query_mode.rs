//! D4 query-mode classification (RFC 0007).
//!
//! `--exact` selects exact mode explicitly; otherwise a small deterministic
//! token classifier selects identifier mode when any token has identifier
//! shape, and natural-language mode otherwise. Shell quoting is deliberately
//! invisible to this logic (quotes are removed before the CLI receives the
//! value), so it never influences the verdict.

/// The retrieval mode selected for a query (RFC 0007 D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryMode {
    Exact,
    Identifier,
    Natural,
}

/// The identifier-shaped tokens of `query` (RFC 0007 D4 identifier-mode
/// fallback needles, when the full query never occurs verbatim).
pub fn identifier_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| is_identifier_token(t))
        .map(str::to_string)
        .collect()
}

/// Classify a query. `exact` is the explicit `--exact` flag.
pub fn classify(query: &str, exact: bool) -> QueryMode {
    if exact {
        return QueryMode::Exact;
    }
    if query.split_whitespace().any(is_identifier_token) {
        QueryMode::Identifier
    } else {
        QueryMode::Natural
    }
}

/// D4 identifier shape: contains `_`, `::`, `/`, `#`, `-`; mid-word capitals;
/// two-plus uppercase letters; or a digits-and-letters error-code shape.
fn is_identifier_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    if ['_', '/', '#', '-'].iter().any(|&c| tok.contains(c)) || tok.contains("::") {
        return true;
    }
    let chars: Vec<char> = tok.chars().collect();
    let mut uppercase = 0usize;
    let mut prev_lower = false;
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            uppercase += 1;
        }
        // Mid-word capital (camelCase boundary): lowercase immediately
        // followed by uppercase.
        if prev_lower && c.is_uppercase() {
            return true;
        }
        // Digits-and-letters error-code shape: uppercase immediately next to
        // a digit (ERR401, 3xx, A2, ...).
        if c.is_ascii_digit() {
            if i > 0 && chars[i - 1].is_uppercase() {
                return true;
            }
            if i + 1 < chars.len() && chars[i + 1].is_uppercase() {
                return true;
            }
        }
        prev_lower = c.is_lowercase();
    }
    uppercase >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_exact_wins() {
        assert_eq!(classify("search index", true), QueryMode::Exact);
    }

    #[test]
    fn identifier_shapes() {
        for q in [
            "snake_case merge",
            "dashed-name",
            "rpl-3jj",
            "Foo/bar",
            "a::b",
            "issue #273",
            "camelCaseThing",
            "HTTP500 error",
        ] {
            assert_eq!(classify(q, false), QueryMode::Identifier, "q={q}");
        }
    }

    #[test]
    fn natural_prose() {
        for q in [
            "how does retry work",
            "search the index",
            "fix the booking carousel",
            "retry safe event processing",
        ] {
            assert_eq!(classify(q, false), QueryMode::Natural, "q={q}");
        }
    }

    #[test]
    fn mixed_identifier_and_prose_is_identifier() {
        // The classifier triggers on any identifier-shaped token.
        assert_eq!(
            classify("fix the rpl-3jj deadlock", false),
            QueryMode::Identifier
        );
    }
}
