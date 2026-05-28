/// Derive a stable `<host>/<owner>/<repo>` form from any git remote URL.
///
/// Handles the four URL shapes git itself supports:
/// - `https://host/owner/repo.git`
/// - `ssh://user@host/owner/repo.git`
/// - `git://host/owner/repo`
/// - `git@host:owner/repo.git`
///
/// Returns `None` for unrecognized inputs rather than guessing — the caller
/// can decide whether to reject or accept a raw URL.
pub fn parse_canonical(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.trim_end_matches(".git");

    // scp-like form: `git@host:owner/repo`
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some(format!("{host}/{path}"));
    }

    for prefix in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            // Strip user@ if present.
            let after_user = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            return Some(after_user.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_from_https() {
        assert_eq!(
            parse_canonical("https://github.com/o/r.git"),
            Some("github.com/o/r".into())
        );
    }

    #[test]
    fn canonical_from_https_with_user() {
        assert_eq!(
            parse_canonical("https://alice@github.com/o/r.git"),
            Some("github.com/o/r".into())
        );
    }

    #[test]
    fn canonical_from_scp_form() {
        assert_eq!(
            parse_canonical("git@github.com:o/r.git"),
            Some("github.com/o/r".into())
        );
    }

    #[test]
    fn canonical_from_ssh_url() {
        assert_eq!(
            parse_canonical("ssh://git@gitlab.com/o/r.git"),
            Some("gitlab.com/o/r".into())
        );
    }

    #[test]
    fn canonical_from_git_protocol() {
        assert_eq!(
            parse_canonical("git://gitlab.com/o/r"),
            Some("gitlab.com/o/r".into())
        );
    }

    #[test]
    fn unknown_form_returns_none() {
        assert_eq!(parse_canonical("file:///tmp/repo"), None);
    }
}
