use std::path::Path;

use crate::error::{GitError, Result};
use crate::url::parse_canonical;

/// Discover whether `path` lives inside a git worktree by walking up the tree
/// looking for a `.git` entry. Cheap and dependency-free; we don't read any
/// git internals here.
pub fn is_inside_git_worktree(path: &Path) -> bool {
    let mut cur = path;
    loop {
        if cur.join(".git").exists() {
            return true;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Read the configured `origin` remote URL from the git repo containing
/// `path`. Returns `Ok(None)` if the repo exists but no `origin` remote is
/// set; `Err` only if we couldn't discover a repo at all.
pub fn discover_origin_url(path: &Path) -> Result<Option<String>> {
    let repo =
        gix::discover(path).map_err(|e| GitError::NotARepo(format!("{}: {e}", path.display())))?;
    let remote = match repo.find_remote("origin") {
        Ok(r) => r,
        Err(gix::remote::find::existing::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(GitError::Other(e.to_string())),
    };
    Ok(remote
        .url(gix::remote::Direction::Fetch)
        .map(|u| u.to_bstring().to_string()))
}

/// Convenience: pair `discover_origin_url` with `parse_canonical` to land at
/// the canonical form the rest of the system stores. Returns `None` when
/// the repo has no origin or when the origin URL can't be parsed.
pub fn discover_canonical(path: &Path) -> Result<Option<String>> {
    Ok(discover_origin_url(path)?.and_then(|u| parse_canonical(&u)))
}

// TODO(sibling-repo-detection): we can link worktrees back to their primary
// repo today, but two sibling directories that happen to point at the same
// `origin` URL aren't recognised as the same logical repo. We'd want a
// helper like `same_logical_repo(a, b) -> bool` that compares canonical
// URLs of both, so `repo discover` could merge duplicate clones into a
// single binding with two worktrees. Not needed for the current MVP — punt
// until the use case shows up.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_walks_parents() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(!is_inside_git_worktree(&nested));
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(is_inside_git_worktree(&nested));
    }

    fn init_repo_with_origin(dir: &std::path::Path, url: &str) {
        // Shell to `git`; it's universally available on dev/CI machines and
        // produces a real on-disk repo gix can parse without ceremony.
        let r = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init");
        assert!(r.success());
        let r = std::process::Command::new("git")
            .args(["remote", "add", "origin", url])
            .current_dir(dir)
            .status()
            .expect("git remote add");
        assert!(r.success());
    }

    #[test]
    fn discover_origin_url_reads_real_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo_with_origin(dir.path(), "git@github.com:o/r.git");
        let url = discover_origin_url(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("git@github.com:o/r.git"));
    }

    #[test]
    fn discover_canonical_combines_steps() {
        let dir = tempfile::TempDir::new().unwrap();
        init_repo_with_origin(dir.path(), "https://github.com/o/r.git");
        let canonical = discover_canonical(dir.path()).unwrap();
        assert_eq!(canonical.as_deref(), Some("github.com/o/r"));
    }

    #[test]
    fn discover_origin_url_returns_none_when_no_origin() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(r.success());
        let url = discover_origin_url(dir.path()).unwrap();
        assert!(url.is_none());
    }

    #[test]
    fn discover_errors_on_non_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(discover_origin_url(dir.path()).is_err());
    }
}
