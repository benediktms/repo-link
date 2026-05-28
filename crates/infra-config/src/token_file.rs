//! On-disk GitHub token file: parsing + permission enforcement.

use std::path::Path;

use crate::error::TokenFileError;

/// Parsed contents of the on-disk token file. Two-line format: line 1 is
/// the token, optional line 2 is the cached GitHub login. Single-line files
/// written before the login was cached parse with `login = None`.
#[derive(Debug, Default)]
pub struct TokenFileContents {
    pub token: Option<String>,
    pub login: Option<String>,
}

pub(crate) fn read_token_file_contents(path: &Path) -> Result<TokenFileContents, TokenFileError> {
    use std::io::Read;

    // Open first, then fstat through the file handle so the permission check
    // and the content read both target the same inode. Avoids a TOCTOU swap
    // between two path-based syscalls.
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TokenFileContents::default());
        }
        Err(source) => {
            return Err(TokenFileError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| TokenFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    enforce_secure_permissions(&metadata, path)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| TokenFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut lines = raw.lines();
    let token = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let login = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(TokenFileContents { token, login })
}

#[cfg(unix)]
fn enforce_secure_permissions(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), TokenFileError> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TokenFileError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_secure_permissions(
    _metadata: &std::fs::Metadata,
    _path: &Path,
) -> Result<(), TokenFileError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_token_file_missing_returns_empty_contents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist");
        let c = read_token_file_contents(&path).unwrap();
        assert!(c.token.is_none());
        assert!(c.login.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_empty_returns_empty_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "   \n  \n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert!(c.token.is_none());
        assert!(c.login.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_legacy_single_line_token_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
        assert!(
            c.login.is_none(),
            "single-line file must not invent a login"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_two_line_yields_token_and_login() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\nbenediktms\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
        assert_eq!(c.login.as_deref(), Some("benediktms"));
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_rejects_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_token_file_contents(&path).unwrap_err();
        match err {
            TokenFileError::InsecurePermissions { mode, .. } => assert_eq!(mode, 0o644),
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
    }
}
