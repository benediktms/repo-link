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

/// The Windows counterpart to the unix `0600` check. A file under
/// `%APPDATA%` inherits ACEs for SYSTEM and the local Administrators group,
/// so "only the current user" is a stricter rule than anything inheritance
/// produces — a token file that predates [`restrict_token_file`] is rejected
/// until `rl gh auth` rewrites it.
#[cfg(windows)]
fn enforce_secure_permissions(
    _metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), TokenFileError> {
    use windows_permissions::constants::{SeObjectType, SecurityInformation};
    use windows_permissions::utilities::current_process_sid;
    use windows_permissions::wrappers::GetNamedSecurityInfo;

    let me = current_process_sid().map_err(|e| acl_fault(path, e))?;
    let descriptor = GetNamedSecurityInfo(
        path,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .map_err(|e| acl_fault(path, e))?;

    // An absent DACL is not an empty one: Windows reads NULL as "grant everyone
    // full control", the exact opposite of the empty-ACL deny-all.
    let Some(dacl) = descriptor.dacl() else {
        return Err(TokenFileError::InsecureAcl {
            path: path.to_path_buf(),
            principal: "everyone (no DACL is set)".to_string(),
        });
    };

    for index in 0..dacl.len() {
        let Some(ace) = dacl.get_ace(index) else {
            continue;
        };
        let principal = match ace.sid() {
            Some(sid) if *sid == *me => continue,
            Some(sid) => sid.to_string(),
            None => format!("an entry of type {:?} carrying no SID", ace.ace_type()),
        };
        return Err(TokenFileError::InsecureAcl {
            path: path.to_path_buf(),
            principal,
        });
    }
    Ok(())
}

/// Replace the file's DACL with a single entry granting the current user full
/// control, protected so `%APPDATA%`'s inheritable ACEs cannot re-apply.
/// Called after creating the file and before the token is written into it.
#[cfg(windows)]
pub fn restrict_token_file(path: &Path) -> Result<(), TokenFileError> {
    use windows_permissions::constants::{SeObjectType, SecurityInformation};
    use windows_permissions::utilities::current_process_sid;
    use windows_permissions::wrappers::SetNamedSecurityInfo;
    use windows_permissions::{LocalBox, SecurityDescriptor};

    let me = current_process_sid().map_err(|e| acl_fault(path, e))?;
    let descriptor: LocalBox<SecurityDescriptor> = format!("D:P(A;;FA;;;{me})")
        .parse()
        .map_err(|e| acl_fault(path, e))?;
    let dacl = descriptor.dacl().ok_or_else(|| TokenFileError::Acl {
        path: path.to_path_buf(),
        message: "built a security descriptor with no DACL".to_string(),
    })?;

    SetNamedSecurityInfo(
        path,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        Some(dacl),
        None,
    )
    .map_err(|e| acl_fault(path, e))
}

#[cfg(windows)]
fn acl_fault(path: &Path, e: impl std::fmt::Display) -> TokenFileError {
    TokenFileError::Acl {
        path: path.to_path_buf(),
        message: format!("could not read or set the Windows security descriptor: {e}"),
    }
}

#[cfg(not(windows))]
pub fn restrict_token_file(_path: &Path) -> Result<(), TokenFileError> {
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
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

    /// The Windows half of `read_token_file_rejects_group_or_world_readable`:
    /// a file carrying only the owner-full-control ACE that
    /// [`restrict_token_file`] writes must read back cleanly.
    #[cfg(windows)]
    #[test]
    fn read_token_file_accepts_an_owner_only_dacl() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\nbenediktms\n").unwrap();
        restrict_token_file(&path).unwrap();

        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
        assert_eq!(c.login.as_deref(), Some("benediktms"));
    }

    /// Grant `Everyone` (`WD`) alongside the owner and the read must refuse.
    /// An explicit well-known SID rather than the runner's inherited ACEs, so
    /// the test doesn't depend on how the temp directory happens to be ACL'd.
    #[cfg(windows)]
    #[test]
    fn read_token_file_rejects_a_dacl_naming_any_other_principal() {
        use windows_permissions::constants::{SeObjectType, SecurityInformation};
        use windows_permissions::utilities::current_process_sid;
        use windows_permissions::wrappers::SetNamedSecurityInfo;
        use windows_permissions::{LocalBox, SecurityDescriptor};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();

        let me = current_process_sid().unwrap();
        let loosened: LocalBox<SecurityDescriptor> =
            format!("D:P(A;;FA;;;{me})(A;;FR;;;WD)").parse().unwrap();
        SetNamedSecurityInfo(
            &path,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            loosened.dacl(),
            None,
        )
        .unwrap();

        match read_token_file_contents(&path).unwrap_err() {
            TokenFileError::InsecureAcl { principal, .. } => {
                assert_eq!(principal, "S-1-1-0", "Everyone should be the offender")
            }
            other => panic!("expected InsecureAcl, got {other:?}"),
        }
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
