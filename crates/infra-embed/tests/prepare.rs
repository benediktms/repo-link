use infra_embed::prepare::prepare;
use infra_embed::profiles::profile;

#[test]
fn prepares_pinned_profile_and_rejects_tamper() {
    let root = std::env::temp_dir().join(format!("rl-embed-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cache = root.join("models");

    let dir = prepare(&profile(), &cache).expect("prepare should install the pinned profile");
    assert_eq!(dir, cache.join(profile().profile_id));
    for a in &profile().artifacts {
        assert!(dir.join(&a.filename).exists(), "missing {}", a.filename);
    }

    // tamper: a wrong digest must be rejected on a fresh profile id
    let mut bad = profile();
    bad.profile_id = "deadbeef".to_string();
    bad.artifacts[0].sha256 = "0".repeat(64);
    let err = prepare(&bad, &cache).unwrap_err();
    assert!(err.to_string().contains("digest mismatch"), "err: {err}");

    let _ = std::fs::remove_dir_all(&root);
}
