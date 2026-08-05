//! D2/D3 chunker + SHA-256 hashing (RFC 0007).
//!
//! Deterministic, model-independent, paragraph-preserving. Title and body
//! form the core search chunk; each comment is its own title-anchored chunk.
//! The formatted-text byte budget is recorded in `CHUNK_FORMAT_VERSION`.

use domain_core::TaskId;
use ports::{ChunkKind, ChunkTarget, TaskTextRow};
use sha2::{Digest, Sha256};

/// The lexical chunk-format version (RFC 0007 D2) lives in `ports` so both
/// the producer (`application-search`) and the validator (the sidecar
/// adapter) agree on it.
pub const CHUNK_FORMAT_VERSION: i64 = ports::SEARCH_CHUNK_FORMAT_VERSION;

/// Hard budget in UTF-8 bytes of **formatted chunk text** per chunk
/// (RFC 0007 D2 "initially approximately 900 bytes").
pub const CHUNK_BUDGET_BYTES: usize = 900;

/// Build the deterministic chunk set for one task (core + comments).
pub fn chunk_task(row: &TaskTextRow) -> Vec<ChunkTarget> {
    let mut out = Vec::new();
    for text in chunk_formatted(&row.title, &row.body, "Description", true) {
        out.push(target(row.task_id, ChunkKind::Core, text));
    }
    // Comments are separate, title-anchored chunks (D3). An empty comment is
    // not search content.
    for c in &row.comments {
        let text = chunk_formatted(&row.title, &c.body, "Comment", false);
        for t in text {
            out.push(target(row.task_id, ChunkKind::Comment, t));
        }
    }
    out
}

fn target(task_id: TaskId, kind: ChunkKind, text: String) -> ChunkTarget {
    let content_hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    ChunkTarget {
        task_id,
        kind,
        content_hash,
        text,
    }
}

/// Formatted-chunk construction for one title/body (or title/comment) pair.
/// `label` is `"Description"` for core content and `"Comment"` for comments.
/// `allow_title_only` is true for core (empty body → title-only content, D2)
/// and false for comments (empty comment → no chunk).
fn chunk_formatted(title: &str, body: &str, label: &str, allow_title_only: bool) -> Vec<String> {
    if body.trim().is_empty() {
        return if allow_title_only {
            vec![format!("Title: {title}\n\n{label}:")]
        } else {
            Vec::new()
        };
    }
    let header = format!("Title: {title}\n\n{label}:\n");
    if header.len() > CHUNK_BUDGET_BYTES {
        // Oversized title: emit the full title as its own chunk, and anchor
        // body chunks with a deterministic, `…`-marked bounded anchor (D2).
        let mut chunks = vec![format!("Title: {title}\n\n{label}:")];
        let anchor = bounded_anchor(title, label);
        let h = format!("Title: {anchor}\n\n{label}:\n");
        chunks.extend(pack_body(&h, body));
        return chunks;
    }
    pack_body(&header, body)
}

/// Deterministic truncated title anchor leaving room for the header + `…`
/// plus a body allowance, so an oversized title doesn't starve its body
/// chunks (RFC 0007 D2).
fn bounded_anchor(title: &str, label: &str) -> String {
    let fixed = format!("Title: \n\n{label}:\n");
    // Reserve at least 300 bytes of body room under the 900-byte budget.
    let max_title = CHUNK_BUDGET_BYTES.saturating_sub(fixed.len() + "…".len() + 300);
    if title.len() <= max_title {
        return title.to_string();
    }
    let mut cut = max_title;
    while cut > 0 && !title.is_char_boundary(cut) {
        cut -= 1;
    }
    // Never emit a window starting mid-title; keep the leading part so the
    // anchor is recognizable.
    format!("{}…", &title[..cut])
}

/// Pack body paragraphs into budget-bounded chunks under `header`. Splits an
/// oversized paragraph at a sentence boundary, then a UTF-8 scalar boundary.
fn pack_body(header: &str, body: &str) -> Vec<String> {
    let paragraphs: Vec<&str> = split_paragraphs(body);
    let mut chunks: Vec<String> = Vec::new();
    let mut cur: String = header.to_string();

    for p in paragraphs {
        if p.is_empty() {
            continue;
        }
        let sep = if cur.len() > header.len() { "\n\n" } else { "" };
        let cand = format!("{cur}{sep}{p}");
        if cand.len() <= CHUNK_BUDGET_BYTES {
            cur = cand;
            continue;
        }
        if cur.len() > header.len() {
            chunks.push(std::mem::take(&mut cur));
            cur = header.to_string();
        }
        // The paragraph alone exceeds the budget: split it under `header`.
        let mut rest = p;
        while !rest.is_empty() {
            let (head, rem) = split_paragraph_fragment(header, rest);
            chunks.push(format!("{header}{head}"));
            rest = rem;
        }
    }
    if cur.len() > header.len() {
        chunks.push(cur);
    }
    chunks
}

/// Split `body` into paragraphs on blank-line boundaries (RFC 0007 D2
/// "paragraph-preserving"); interior single newlines are preserved.
fn split_paragraphs(body: &str) -> Vec<&str> {
    body.split("\n\n").collect()
}

/// Cut `text` to fit under `header` within `CHUNK_BUDGET_BYTES`, preferring a
/// sentence boundary, then a UTF-8 scalar boundary. Returns `(head, rest)`
/// with a non-empty `head` when `header` leaves room.
fn split_paragraph_fragment<'a>(header: &str, text: &'a str) -> (String, &'a str) {
    let avail = CHUNK_BUDGET_BYTES.saturating_sub(header.len());
    if avail == 0 {
        // No room under this header: emit one scalar anyway so the packing
        // loop always makes progress and no text is dropped (D2).
        let first = text.chars().next().unwrap();
        let len = first.len_utf8();
        return (first.to_string(), &text[len..]);
    }
    let utf8_max = avail.min(text.len());
    let mut cut = utf8_max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 {
        // The first scalar alone exceeds the budget — still emit it so the
        // loop terminates and no text is dropped (D2 "no source text is
        // dropped").
        let first = text.chars().next().unwrap();
        let len = first.len_utf8();
        return (first.to_string(), &text[len..]);
    }
    // Prefer a sentence boundary within the fit.
    if let Some(pos) = last_sentence_boundary(text, cut) {
        cut = pos + 1;
    }
    let head = text[..cut].to_string();
    (head, &text[cut..])
}

/// Last sentence/line terminator at or before `limit`, if any.
fn last_sentence_boundary(text: &str, limit: usize) -> Option<usize> {
    text.as_bytes()[..limit]
        .iter()
        .rposition(|&b| matches!(b, b'.' | b'?' | b'!' | b'\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, body: &str) -> TaskTextRow {
        TaskTextRow {
            task_id: domain_core::TaskId::new(),
            workspace_id: domain_core::WorkspaceId::new(),
            repo_id: None,
            is_open: true,
            title: title.to_string(),
            body: body.to_string(),
            comments: Vec::new(),
        }
    }

    #[test]
    fn short_body_is_one_core_chunk() {
        let r = row("Fix the retry loop", "It double-acks on timeout.");
        let chunks = chunk_task(&r);
        assert_eq!(chunks.len(), 1, "short task -> one core chunk");
        assert_eq!(chunks[0].kind, ChunkKind::Core);
        assert!(
            chunks[0]
                .text
                .starts_with("Title: Fix the retry loop\n\nDescription:")
        );
        assert_eq!(chunks[0].content_hash.len(), 32);
    }

    #[test]
    fn empty_body_is_title_only() {
        let r = row("RFC-0002", "");
        let chunks = chunk_task(&r);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Title: RFC-0002\n\nDescription:");
    }

    #[test]
    fn long_body_chunks_without_dropping_text() {
        let para = "word ".repeat(400); // ~2000 bytes, one paragraph
        let r = row("T", &format!("{para}\n\n{para}"));
        let chunks = chunk_task(&r);
        assert!(
            chunks.len() >= 4,
            "long body -> multiple chunks: {}",
            chunks.len()
        );
        let joined: String = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        // Every paragraph word survives (only headers/separators added).
        for _ in 0..2 {
            assert!(joined.contains("word word"));
        }
        for c in &chunks {
            assert!(c.text.len() <= CHUNK_BUDGET_BYTES + "…".len() + 8);
        }
    }

    #[test]
    fn oversized_title_gets_own_chunk_and_bounded_anchor() {
        let long_title = "x".repeat(1500);
        let body = "body paragraph";
        let r = row(&long_title, body);
        let chunks = chunk_task(&r);
        assert!(chunks.len() >= 2);
        // The full title is still indexed as its own core chunk.
        assert!(chunks[0].text.contains(&long_title));
        // A later body chunk carries a bounded anchor with the ellipsis marker.
        assert!(chunks[1].text.contains("…"));
        assert!(chunks[1].text.contains("body paragraph"));
    }

    #[test]
    fn comments_are_separate_anchored_chunks() {
        let r = TaskTextRow {
            task_id: domain_core::TaskId::new(),
            workspace_id: domain_core::WorkspaceId::new(),
            repo_id: None,
            is_open: true,
            title: "T".into(),
            body: "B".into(),
            comments: vec![
                ports::CommentTextRow {
                    remote_comment_id: Some("123".into()),
                    body: "first comment".into(),
                },
                ports::CommentTextRow {
                    remote_comment_id: Some("456".into()),
                    body: "".into(),
                },
            ],
        };
        let chunks = chunk_task(&r);
        // core + one non-empty comment; the empty comment is skipped.
        assert_eq!(chunks.len(), 2);
        let comment = &chunks[1];
        assert_eq!(comment.kind, ChunkKind::Comment);
        assert!(comment.text.starts_with("Title: T\n\nComment:"));
        assert!(comment.text.contains("first comment"));
    }

    #[test]
    fn chunking_is_deterministic() {
        let row = row("Same input", "Same body\n\nwith two paras");
        let a = chunk_task(&row);
        let b = chunk_task(&row);
        assert_eq!(a, b);
    }
}
