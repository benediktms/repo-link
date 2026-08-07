//! Pinned embedding profile manifest (RFC 0007 D7).
//!
//! The trust root is this source tree: every artifact digest below is authored
//! from the Stage 2 candidate evaluation (eval/rfc0007/) and compiled into the
//! binary. `prepare-model` verifies every downloaded byte against these
//! digests — never trust-on-first-use.

use crate::prepare::{ManifestArtifact, ProfileManifest};

/// Canonical manifest JSON (sorted keys, no whitespace) whose SHA-256 is the
/// `embedding_profile_id`. See the `profile()` doc for the invariant.
const PROFILE_ID: &str = "6962ee9ee1d2cd50f284bd3eefbadd5b99227c1ea80938b138aa0e936e370f3f";

/// The single shipped profile (RFC 0007 §9 Stage 2 winner).
///
/// `embedding_profile_id` is the SHA-256 of the canonical manifest:
/// `{"artifacts":{"config.json":{"sha256":...},"corpus_prefix":null,
/// "dimensions":384,"max_input_tokens":512,"normalization":"l2",
/// "pooling":"mean","query_prefix":null,"repo":"sentence-transformers/
/// all-MiniLM-L6-v2","revision":"1110a24..."}`
pub fn profile() -> ProfileManifest {
    ProfileManifest {
        profile_id: PROFILE_ID.to_string(),
        repo_id: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
        revision: "1110a243fdf4706b3f48f1d95db1a4f5529b4d41".to_string(),
        artifacts: vec![
            ManifestArtifact {
                filename: "model.safetensors".to_string(),
                sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db"
                    .to_string(),
            },
            ManifestArtifact {
                filename: "config.json".to_string(),
                sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41"
                    .to_string(),
            },
            ManifestArtifact {
                filename: "tokenizer.json".to_string(),
                sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037"
                    .to_string(),
            },
        ],
    }
}
