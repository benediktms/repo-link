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
    enforce_secure_permissions(&file, path)?;
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
fn enforce_secure_permissions(file: &std::fs::File, path: &Path) -> Result<(), TokenFileError> {
    use std::os::unix::fs::MetadataExt;
    let mode = file
        .metadata()
        .map_err(|source| TokenFileError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .mode()
        & 0o777;
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
///
/// Reads through the open handle rather than the path, so the descriptor
/// checked here belongs to the same file the caller goes on to read.
#[cfg(windows)]
fn enforce_secure_permissions(file: &std::fs::File, path: &Path) -> Result<(), TokenFileError> {
    match win::foreign_principal(file, path)? {
        // The owner keeps READ_CONTROL and WRITE_DAC whatever the DACL says,
        // so a foreign owner can re-open the file to itself at any time.
        Some(win::Foreign::Owner(owner)) => Err(TokenFileError::ForeignOwner {
            path: path.to_path_buf(),
            owner,
        }),
        Some(win::Foreign::Grantee(principal)) => Err(TokenFileError::InsecureAcl {
            path: path.to_path_buf(),
            principal,
        }),
        None => Ok(()),
    }
}

/// Make the current user the file's owner and replace its DACL with a single
/// entry granting that user full control, protected so `%APPDATA%`'s
/// inheritable ACEs cannot re-apply. Called after creating the file and
/// before the token is written into it.
#[cfg(windows)]
pub fn restrict_token_file(path: &Path) -> Result<(), TokenFileError> {
    let me = win::current_user_sid(path)?;
    let sid = win::sid_string(path, me.psid())?;
    win::apply_security(path, &format!("O:{sid}D:P(A;;FA;;;{sid})"))
}

#[cfg(not(windows))]
pub fn restrict_token_file(_path: &Path) -> Result<(), TokenFileError> {
    Ok(())
}

/// The Win32 calls behind the two functions above. `windows-sys` is a raw
/// binding crate, so every call here is `unsafe`; the module exists to keep
/// that contained and to own the `LocalFree`/`CloseHandle` pairings, which
/// are the only way these APIs hand memory back.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use crate::error::TokenFileError;

    /// A principal with standing access to the file that is not the current
    /// user. Owner and grantee are distinguished because the owner's hold is
    /// implicit — it survives any DACL we write.
    pub(super) enum Foreign {
        Owner(String),
        Grantee(String),
    }

    /// A `LocalAlloc`-owned pointer. Every Win32 call below that returns
    /// memory returns it on the local heap, and leaks it if we forget.
    struct LocalPtr(*mut c_void);

    impl Drop for LocalPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0 as HLOCAL) };
            }
        }
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The process token's user SID, kept alive by the `TOKEN_USER` buffer it
    /// points into. `u64` backing rather than `u8` so the buffer satisfies the
    /// struct's pointer alignment.
    pub(super) struct UserSid(Vec<u64>);

    impl UserSid {
        pub(super) fn psid(&self) -> PSID {
            unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
        }
    }

    fn fault(path: &Path, what: &str, e: std::io::Error) -> TokenFileError {
        TokenFileError::Acl {
            path: path.to_path_buf(),
            message: format!("{what}: {e}"),
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub(super) fn current_user_sid(path: &Path) -> Result<UserSid, TokenFileError> {
        let mut raw: HANDLE = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(fault(
                path,
                "OpenProcessToken",
                std::io::Error::last_os_error(),
            ));
        }
        let token = OwnedHandle(raw);

        // First call sizes the buffer: it is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER while writing the length back.
        let mut needed = 0u32;
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(fault(
                path,
                "GetTokenInformation (sizing)",
                std::io::Error::last_os_error(),
            ));
        }

        let mut buf = vec![0u64; needed.div_ceil(8) as usize];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(fault(
                path,
                "GetTokenInformation",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(UserSid(buf))
    }

    pub(super) fn sid_string(path: &Path, sid: PSID) -> Result<String, TokenFileError> {
        let mut raw: *mut u16 = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 {
            return Err(fault(
                path,
                "ConvertSidToStringSidW",
                std::io::Error::last_os_error(),
            ));
        }
        let owned = LocalPtr(raw as *mut c_void);
        let mut len = 0;
        while unsafe { *raw.add(len) } != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
        drop(owned);
        Ok(s)
    }

    /// Parse `sddl` and install it on `path`: its DACL protected so inherited
    /// ACEs cannot re-apply, and its owner when the string carries one.
    pub(super) fn apply_security(path: &Path, sddl: &str) -> Result<(), TokenFileError> {
        let sddl_w: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                null_mut(),
            )
        } == 0
        {
            return Err(fault(
                path,
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                std::io::Error::last_os_error(),
            ));
        }
        let _owned = LocalPtr(psd);

        let mut present = 0i32;
        let mut dacl: *mut ACL = null_mut();
        let mut defaulted = 0i32;
        if unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) } == 0 {
            return Err(fault(
                path,
                "GetSecurityDescriptorDacl",
                std::io::Error::last_os_error(),
            ));
        }
        if present == 0 || dacl.is_null() {
            return Err(TokenFileError::Acl {
                path: path.to_path_buf(),
                message: "built a security descriptor with no DACL".to_string(),
            });
        }

        let mut owner: PSID = null_mut();
        let mut owner_defaulted = 0i32;
        if unsafe { GetSecurityDescriptorOwner(psd, &mut owner, &mut owner_defaulted) } == 0 {
            return Err(fault(
                path,
                "GetSecurityDescriptorOwner",
                std::io::Error::last_os_error(),
            ));
        }
        let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if !owner.is_null() {
            info |= OWNER_SECURITY_INFORMATION;
        }

        let mut name = wide(path);
        let status = unsafe {
            SetNamedSecurityInfoW(
                name.as_mut_ptr(),
                SE_FILE_OBJECT,
                info,
                owner,
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        if status != 0 {
            return Err(fault(
                path,
                "SetNamedSecurityInfoW",
                std::io::Error::from_raw_os_error(status as i32),
            ));
        }
        Ok(())
    }

    /// Name the first principal with standing access to the file that is not
    /// the current user, or `None` when there is none. Reads the descriptor
    /// through `file`'s handle, so it describes the same object the caller
    /// reads rather than whatever the path resolves to a moment later.
    ///
    /// An ACE type this cannot parse counts as foreign: object ACEs put the
    /// SID at a different offset, so refusing to interpret one is safer than
    /// reading the wrong bytes as a SID.
    pub(super) fn foreign_principal(
        file: &std::fs::File,
        path: &Path,
    ) -> Result<Option<Foreign>, TokenFileError> {
        use std::os::windows::io::AsRawHandle;

        let me = current_user_sid(path)?;

        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut psd,
            )
        };
        if status != 0 {
            return Err(fault(
                path,
                "GetSecurityInfo",
                std::io::Error::from_raw_os_error(status as i32),
            ));
        }
        let _owned = LocalPtr(psd);

        if owner.is_null() || unsafe { EqualSid(owner, me.psid()) } == 0 {
            let named = if owner.is_null() {
                "an unreadable account".to_string()
            } else {
                sid_string(path, owner)?
            };
            return Ok(Some(Foreign::Owner(named)));
        }

        // A NULL DACL is not an empty one: Windows reads it as "grant everyone
        // full control", the exact opposite of the empty-ACL deny-all.
        if dacl.is_null() {
            return Ok(Some(Foreign::Grantee(
                "everyone (no DACL is set)".to_string(),
            )));
        }

        for index in 0..unsafe { (*dacl).AceCount } as u32 {
            let mut ace: *mut c_void = null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(fault(path, "GetAce", std::io::Error::last_os_error()));
            }
            let ace_type = unsafe { (*(ace as *const ACE_HEADER)).AceType } as u32;
            // A deny ACE naming someone else takes access away rather than
            // granting it, so it is not an exposure — reporting one would
            // name a principal that in fact holds nothing.
            if ace_type == ACCESS_DENIED_ACE_TYPE {
                continue;
            }
            // Anything that is not a plain allow ACE may still grant access —
            // the object variants do — but its SID sits elsewhere, so it is
            // reported rather than parsed.
            if ace_type != ACCESS_ALLOWED_ACE_TYPE {
                return Ok(Some(Foreign::Grantee(format!(
                    "an access-control entry of type {ace_type}"
                ))));
            }
            let sid = unsafe { std::ptr::addr_of!((*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) }
                as PSID;
            if unsafe { EqualSid(sid, me.psid()) } == 0 {
                return Ok(Some(Foreign::Grantee(sid_string(path, sid)?)));
            }
        }
        Ok(None)
    }
}

#[cfg(all(not(unix), not(windows)))]
fn enforce_secure_permissions(_file: &std::fs::File, _path: &Path) -> Result<(), TokenFileError> {
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
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();

        let me = win::current_user_sid(&path).unwrap();
        let sid = win::sid_string(&path, me.psid()).unwrap();
        // The owner is pinned too, so the check reaches the ACE walk rather
        // than tripping on whoever the runner made the owner.
        win::apply_security(&path, &format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FR;;;WD)")).unwrap();

        match read_token_file_contents(&path).unwrap_err() {
            TokenFileError::InsecureAcl { principal, .. } => {
                assert_eq!(principal, "S-1-1-0", "Everyone should be the offender")
            }
            other => panic!("expected InsecureAcl, got {other:?}"),
        }
    }

    /// The SID round-trips through the string form used to build the SDDL —
    /// a wrong length or a missing NUL walk in `sid_string` would corrupt the
    /// DACL that `restrict_token_file` installs.
    #[cfg(windows)]
    #[test]
    fn current_user_sid_renders_as_a_well_formed_sid_string() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();

        let me = win::current_user_sid(&path).unwrap();
        let sid = win::sid_string(&path, me.psid()).unwrap();
        assert!(sid.starts_with("S-1-"), "unexpected SID form: {sid}");
        assert!(!sid.contains('\0'), "SID string kept its terminator: {sid}");
    }

    /// A deny ACE hands out no access, so it must not be reported as a
    /// principal the token is exposed to. Denies `AN` (anonymous logon)
    /// rather than `WD` (everyone), which would include the test's own
    /// account and lock it out of its own file.
    #[cfg(windows)]
    #[test]
    fn read_token_file_ignores_a_deny_ace_for_another_principal() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\nbenediktms\n").unwrap();

        let me = win::current_user_sid(&path).unwrap();
        let sid = win::sid_string(&path, me.psid()).unwrap();
        win::apply_security(&path, &format!("O:{sid}D:P(D;;FA;;;AN)(A;;FA;;;{sid})")).unwrap();

        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
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
