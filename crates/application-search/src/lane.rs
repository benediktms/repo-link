//! RRF fusion + deterministic ordering (RFC 0007 D4).
//!
//! Reciprocal-rank fusion combines independent lane rankings (BM25 / cosine /
//! literal occurrence) without pretending their scores share a calibrated
//! scale. The constant `k` is the literature default (RFC 0007 D4, "initially
//! k = 60"); Stage 2 may revise it before it becomes final.

use std::collections::HashMap;

use domain_core::TaskId;

/// RRF constant (RFC 0007 D4 initial value).
pub const RRF_K: f64 = 60.0;

/// Fuse one or more per-task 1-based rankings into an RRF score per task.
pub fn rrf_fuse(rankings: &[&HashMap<TaskId, usize>]) -> HashMap<TaskId, f64> {
    let mut scores: HashMap<TaskId, f64> = HashMap::new();
    for ranking in rankings {
        for (task, rank) in ranking.iter() {
            let e = scores.entry(*task).or_default();
            *e += 1.0 / (RRF_K + *rank as f64);
        }
    }
    scores
}

/// Order fused scores descending; ties resolve by task ID ascending (RFC 0007
/// D4 "Ties resolve by task ID in all modes").
pub fn order_fused(scores: &HashMap<TaskId, f64>) -> Vec<TaskId> {
    let mut v: Vec<(TaskId, f64)> = scores.iter().map(|(t, s)| (*t, *s)).collect();
    v.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.as_uuid().cmp(&b.0.as_uuid()))
    });
    v.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn tid(hex_last: u32) -> TaskId {
        TaskId::from_str(&format!(
            "00000000-0000-0000-0000-{:012x}",
            u64::from(hex_last)
        ))
        .unwrap()
    }

    #[test]
    fn fusion_ranks_present_in_all_lanes_first() {
        let a: HashMap<TaskId, usize> = [(tid(1), 1), (tid(2), 2)].into_iter().collect();
        let b: HashMap<TaskId, usize> = [(tid(1), 2), (tid(2), 1)].into_iter().collect();
        let fused = rrf_fuse(&[&a, &b]);
        let ordered = order_fused(&fused);
        // Both appear in both lanes; a and b tie exactly (1/61+1/62 each) so
        // the tie-break orders by task id ascending.
        assert_eq!(ordered.len(), 2);
        assert!(ordered.contains(&tid(1)) && ordered.contains(&tid(2)));
    }

    #[test]
    fn single_lane_uses_rrf_of_its_ranks() {
        let a: HashMap<TaskId, usize> = [(tid(1), 1), (tid(2), 2)].into_iter().collect();
        let fused = rrf_fuse(&[&a]);
        assert_eq!(fused[&tid(1)], 1.0 / (RRF_K + 1.0), "a must score 1/(k+1)");
        assert_eq!(fused[&tid(2)], 1.0 / (RRF_K + 2.0), "b must score 1/(k+2)");
        assert_eq!(order_fused(&fused), vec![tid(1), tid(2)]);
    }

    #[test]
    fn ties_break_by_task_id() {
        let a: HashMap<TaskId, usize> = [(tid(11), 1), (tid(1), 1)].into_iter().collect();
        let fused = rrf_fuse(&[&a]);
        let ordered = order_fused(&fused);
        // Equal scores: lower uuid wins.
        assert_eq!(ordered, vec![tid(1), tid(11)]);
    }
}
