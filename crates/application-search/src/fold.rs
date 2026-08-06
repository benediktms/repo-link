//! Unicode full-case-fold → raw-byte-span mapping and the raw-text literal
//! lane (RFC 0007 D4).
//!
//! The literal lane runs over authoritative raw text (title/body/comments),
//! never over formatted chunks, so it can never be broken by a chunk
//! boundary and never false-matches injected labels. The needle and source
//! share the same full Unicode case fold; a folded match maps back to the
//! complete raw source scalars (RFC 0007 D4 partial-expansion rule):
//! excerpts are sliced with raw spans only, never folded offsets.
//!
//! Backed by `unicode-casefold` (full fold, char-granular emit), which is
//! the RFC 0007 D4 dependency decision (map ticket rpl-vru).

use unicode_casefold::{Locale, UnicodeCaseFold, Variant};

/// One folded character, tagged with the raw source scalar span that produced
/// it (RFC 0007 D4: "records the raw UTF-8 byte span that produced every
/// emitted folded byte").
#[derive(Clone, Copy, Debug)]
struct FoldChar {
    folded_start: usize,
    folded_end: usize,
    source_start: usize,
    source_end: usize,
}

/// A source string folded to its full case-fold, with each folded char's
/// raw source span.
pub struct FoldedSource {
    text: String,
    chars: Vec<FoldChar>,
}

/// Fold `s` to its full case fold, recording raw spans.
pub fn fold_source(s: &str) -> FoldedSource {
    let mut text = String::new();
    let mut chars = Vec::new();
    for (byte_off, c) in s.char_indices() {
        let src_start = byte_off;
        let src_end = byte_off + c.len_utf8();
        for fc in c
            .to_string()
            .case_fold_with(Variant::Full, Locale::default())
        {
            let start = text.len();
            text.push(fc);
            chars.push(FoldChar {
                folded_start: start,
                folded_end: text.len(),
                source_start: src_start,
                source_end: src_end,
            });
        }
    }
    FoldedSource { text, chars }
}

/// Fold `s` to its full case fold (no spans needed for the needle).
pub fn fold_plain(s: &str) -> String {
    s.case_fold_with(Variant::Full, Locale::default()).collect()
}

/// Raw byte spans in `text` where the case-folded `needle` occurs.
pub fn find_literal_spans(text: &str, needle: &str) -> Vec<std::ops::Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    let src = fold_source(text);
    let needle_folded = fold_plain(needle);
    if needle_folded.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(pos) = src.text[search_from..].find(&needle_folded) {
        let mstart = search_from + pos;
        let mend = mstart + needle_folded.len();
        if let Some(span) = map_to_raw(&src.chars, mstart, mend) {
            out.push(span);
        }
        // Advance by one folded scalar from `mstart` (not `mstart+1`, which
        // can land inside a multi-byte scalar and panic the next slice; not
        // `mend`, which would drop overlapping matches like "ana" in
        // "banana").
        let Some(c) = src.text[mstart..].chars().next() else {
            break;
        };
        search_from = mstart + c.len_utf8();
    }
    out
}

/// Map a folded match window back to the raw source span. A partially-matched
/// case-fold expansion maps back to the complete source scalar (RFC 0007 D4).
fn map_to_raw(chars: &[FoldChar], mstart: usize, mend: usize) -> Option<std::ops::Range<usize>> {
    let first = chars.iter().find(|c| c.folded_end > mstart)?;
    let last = chars
        .iter()
        .rev()
        .find(|c| c.folded_start < mend)
        .or(Some(first))?;
    Some(first.source_start..last.source_end)
}

/// Produce a bounded excerpt windowed around `span` in `raw`, sliced only
/// with raw offsets. `limit` bounds the excerpt length; the window is padded
/// around the match without ever using folded offsets.
pub fn excerpt(raw: &str, span: &std::ops::Range<usize>, limit: usize) -> String {
    let start = span.start.saturating_sub(limit / 2);
    let end = (span.end + limit / 2).min(raw.len());
    // Round to char boundaries so we never emit a partial scalar.
    let mut s = start;
    while s > 0 && !raw.is_char_boundary(s) {
        s -= 1;
    }
    let mut e = end;
    while e < raw.len() && !raw.is_char_boundary(e) {
        e += 1;
    }
    let mut out = String::new();
    if s > 0 {
        out.push('…');
    }
    out.push_str(&raw[s..e]);
    if e < raw.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_match_maps_to_raw_span() {
        let spans = find_literal_spans("The Quick Brown Fox", "quick");
        assert_eq!(spans, vec![4..9]);
    }

    #[test]
    fn full_fold_stra_e_ss_span_covers_whole_scalar() {
        // ß folds to "ss"; a partial fold match maps back to the full ß.
        let spans = find_literal_spans("GROSSE Straße", "strasse");
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(&"GROSSE Straße"[span.clone()], "Straße");
    }

    #[test]
    fn dotted_upper_i_folds() {
        // İ (U+0130) full-folds to "i̇" (i + U+0307 combining dot above). A
        // needle covering that expansion's complete fold maps back to the
        // full raw "İ" scalar (RFC 0007 D4 partial-expansion rule).
        let spans = find_literal_spans("x İzmir", "i\u{307}");
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(&"x İzmir"[span.clone()], "İ");
    }

    #[test]
    fn excerpt_never_uses_folded_offsets_or_splits_scalars() {
        let raw = "\u{1F600}and some body text around the target here";
        let spans = find_literal_spans(raw, "target");
        let span = spans[0].clone();
        let ex = excerpt(raw, &span, 12);
        assert!(ex.contains("target"));
        // Excerpt is valid UTF-8 (no partial scalars) by construction.
        assert!(std::str::from_utf8(ex.as_bytes()).is_ok());
    }

    #[test]
    fn needle_identifiers_work() {
        assert_eq!(
            find_literal_spans("error E0308: mismatched types", "e0308"),
            vec![6..11]
        );
    }

    #[test]
    fn multi_byte_matched_scalar_does_not_panic() {
        // Regression: advancing past a multi-byte folded scalar used to land
        // inside it and panic the next slice. Searching for `é` twice must be
        // safe and return both matches.
        let spans = find_literal_spans("aé...é", "é");
        assert_eq!(spans.len(), 2);
        assert_eq!(&"aé...é"[spans[0].clone()], "é");
        assert_eq!(&"aé...é"[spans[1].clone()], "é");
    }

    #[test]
    fn overlapping_matches_are_preserved() {
        // Advancing to the match end would drop the second, overlapping
        // "ana" in "banana"; advancing by one folded scalar preserves it.
        let spans = find_literal_spans("banana", "ana");
        assert_eq!(spans.len(), 2);
        assert_eq!(&"banana"[spans[0].clone()], "ana");
        assert_eq!(&"banana"[spans[1].clone()], "ana");
    }
}
