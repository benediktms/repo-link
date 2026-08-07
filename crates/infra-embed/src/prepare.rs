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
/// set, the runtime embedding config, and the profile id (SHA-256 of the
/// canonical manifest including the config fields below).
#[derive(Debug, Clone)]
pub struct ProfileManifest {
    pub profile_id: String,
    pub repo_id: String,
    pub revision: String,
    pub artifacts: Vec<ManifestArtifact>,
    pub pooling: crate::model::Pooling,
    pub corpus_prefix: Option<String>,
    pub query_prefix: Option<String>,
    pub dimensions: usize,
    pub max_input_tokens: usize,
}

impl ProfileManifest {
    /// The runtime embedding configuration this profile pins. Identity
    /// (`profile_id`) and behaviour (pooling/prefixes/dims/input limit) live
    /// in one manifest so a change to either changes the profile id.
    pub fn embed_config(&self) -> crate::model::EmbedConfig {
        crate::model::EmbedConfig {
            pooling: self.pooling,
            corpus_prefix: self.corpus_prefix.clone(),
            query_prefix: self.query_prefix.clone(),
            dims: self.dimensions,
            max_input_tokens: self.max_input_tokens,
        }
    }

    /// True when every manifest artifact is present at `cache_root/<id>`
    /// and matches its pinned digest. A cheap, non-downloading, non-loading
    /// probe for diagnostics (status) and idempotent prepare.
    pub fn verify_cached(&self, cache_root: &Path) -> bool {
        let dir = cache_root.join(&self.profile_id);
        self.artifacts.iter().all(|a| {
            let path = dir.join(&a.filename);
            sha256_file(&path).map(|h| h == a.sha256).unwrap_or(false)
        })
    }
}

fn sha256_file(path: &Path) -> Result<String, PortError> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)
        .map_err(|e| PortError::Backend(format!("open {}: {e}", path.display())))?;
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
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
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

/// Reject artifact filenames that escape the staging directory: a single
/// normal path component only (no separators, no `.`/`..`).
fn validate_artifact_filename(filename: &str) -> Result<(), PortError> {
    let name = std::path::Path::new(filename);
    if name.components().count() != 1
        || name.file_name().and_then(|f| f.to_str()) != Some(filename)
        || filename == "."
        || filename == ".."
    {
        return Err(PortError::Backend(format!(
            "unsafe artifact filename: {filename}"
        )));
    }
    Ok(())
}

/// Create a directory tree with owner-only (`0o700`) permissions: the mmap
/// in `model::load` relies on the cache never being writable by other
/// accounts. Applies to every directory the recursive builder creates.
fn create_dir_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Typed prepare failures. [`PrepareError::AlreadyPrepared`] is not a
/// failure — re-running `prepare-model` on a prepared profile is idempotent.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("profile {profile_id} already prepared at {path}")]
    AlreadyPrepared { profile_id: String, path: PathBuf },
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
        // Idempotent path, but never trust an existing cache: a tampered
        // artifact must fail here, not report `prepared: true`.
        for artifact in &manifest.artifacts {
            verify_artifact(&final_dir.join(&artifact.filename), &artifact.sha256)?;
        }
        return Err(PrepareError::AlreadyPrepared {
            profile_id: manifest.profile_id.clone(),
            path: final_dir,
        });
    }
    for artifact in &manifest.artifacts {
        validate_artifact_filename(&artifact.filename)?;
    }
    create_dir_private(cache_root)
        .map_err(|e| PortError::Backend(format!("create cache root: {e}")))?;

    let nonce: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let tmp = cache_root.join(format!(
        ".tmp-prepare-{}-{}-{}",
        manifest.profile_id.get(..8).unwrap_or(&manifest.profile_id),
        std::process::id(),
        nonce
    ));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)
            .map_err(|e| PortError::Backend(format!("clear stale tmp: {e}")))?;
    }
    create_dir_private(&tmp).map_err(|e| PortError::Backend(format!("create tmp: {e}")))?;

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
