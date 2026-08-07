use infra_embed::prepare::{PrepareError, prepare};
use infra_embed::profiles::profile;

fn temp_root() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

#[test]
fn prepares_pinned_profile_and_rejects_tamper() {
    if std::env::var("REPO_LINK_E2E").is_err() {
        eprintln!("skipping prepare test (set REPO_LINK_E2E to run, needs network)");
        return;
    }
    let root = temp_root();
    let cache = root.path().join("models");

    let dir = prepare(&profile(), &cache).expect("prepare should install the pinned profile");
    assert_eq!(dir, cache.join(profile().profile_id));
    for a in &profile().artifacts {
        assert!(dir.join(&a.filename).exists(), "missing {}", a.filename);
    }

    match prepare(&profile(), &cache) {
        Err(PrepareError::AlreadyPrepared { profile_id, path }) => {
            assert_eq!(profile_id, profile().profile_id);
            assert_eq!(path, dir);
        }
        other => panic!("expected AlreadyPrepared, got {other:?}"),
    }

    // tamper: a wrong digest must be rejected on a fresh profile id
    let mut bad = profile();
    bad.profile_id = "deadbeef".to_string();
    bad.artifacts[0].sha256 = "0".repeat(64);
    let err = prepare(&bad, &cache).unwrap_err();
    assert!(err.to_string().contains("digest mismatch"), "err: {err}");
}
