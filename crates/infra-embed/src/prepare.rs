use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use hf_hub::api::sync::Api;
use ports::PortError;

/// One artifact in a pinned profile manifest (RFC 0007 D7): repo-relative
/// filename and its expected SHA-256 digest.
#[derive(Debug, Clone)]
pub struct ManifestArtifact {
    pub filename: String,
    pub sha256: String,
}

/// A complete pinned profile: Hub repo id, immutable revision, the artifact
/// set, and the profile id (SHA-256 of the canonical manifest).
#[derive(Debug, Clone)]
pub struct ProfileManifest {
    pub profile_id: String,
    pub repo_id: String,
    pub revision: String,
    pub artifacts: Vec<ManifestArtifact>,
}

fn sha256_file(path: &Path) -> Result<String, PortError> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path).map_err(|e| PortError::Backend(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| PortError::Backend(format!("read {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}

fn verify_artifact(path: &Path, expected_sha256: &str) -> Result<(), PortError> {
    let actual = sha256_file(path)?;
    if actual != expected_sha256 {
        return Err(PortError::Backend(format!(
            "digest mismatch for {}: expected {expected_sha256}, got {actual}",
            path.display()
        )));
    }
    Ok(())
}

/// Typed prepare failures. [`PrepareError::AlreadyPrepared`] is not a
/// failure — re-running `prepare-model` on a prepared profile is idempotent.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("profile {profile_id} already prepared at {path}")]
    AlreadyPrepared {
        profile_id: String,
        path: PathBuf,
    },
    #[error(transparent)]
    Port(#[from] PortError),
}

pub type PrepareResult<T> = Result<T, PrepareError>;

/// Install the pinned profile into the content-addressed cache at
/// `<cache_root>/<profile_id>/`. Downloads into a temp dir first, verifies
/// every artifact against the manifest, then atomically renames the whole
/// profile into place. Returns the resulting profile directory.
///
/// Idempotent: a cache that already holds this profile id returns
/// [`PrepareError::AlreadyPrepared`] with the existing directory.
pub fn prepare(manifest: &ProfileManifest, cache_root: &Path) -> PrepareResult<PathBuf> {
    let final_dir = cache_root.join(&manifest.profile_id);
    if final_dir.exists() {
        return Err(PrepareError::AlreadyPrepared {
            profile_id: manifest.profile_id.clone(),
            path: final_dir,
        });
    }
    fs::create_dir_all(cache_root)
        .map_err(|e| PortError::Backend(format!("create cache root: {e}")))?;

    let tmp = cache_root.join(format!(".tmp-prepare-{}", std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| PortError::Backend(format!("clear stale tmp: {e}")))?;
    }
    fs::create_dir(&tmp).map_err(|e| PortError::Backend(format!("create tmp: {e}")))?;

    let api = Api::new().map_err(|e| PortError::Network(format!("hf-hub api: {e}")))?;
    let repo = api.repo(hf_hub::Repo::with_revision(
        manifest.repo_id.clone(),
        hf_hub::RepoType::Model,
        manifest.revision.clone(),
    ));

    let result = (|| -> PrepareResult<PathBuf> {
        for artifact in &manifest.artifacts {
            let fetched = repo
                .get(&artifact.filename)
                .map_err(|e| PortError::Network(format!("fetch {}: {e}", artifact.filename)))?;
            verify_artifact(&fetched, &artifact.sha256)?;
            let dst = tmp.join(&artifact.filename);
            fs::copy(&fetched, &dst)
                .map_err(|e| PortError::Backend(format!("stage {}: {e}", artifact.filename)))?;
            verify_artifact(&dst, &artifact.sha256)?;
        }
        fs::rename(&tmp, &final_dir)
            .map_err(|e| PortError::Backend(format!("install profile: {e}")))?;
        Ok(final_dir.clone())
    })();

    if result.is_err() && tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}
