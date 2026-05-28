use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Walk `root` and return every directory that contains a `.git` entry.
///
/// Useful for bulk-attaching repos: `repo-link` can scan `~/code/` once and
/// surface every git checkout below it. We bound depth at 6 to avoid
/// pathological `node_modules`-style trees.
pub fn discover_repos_under(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name() == ".git"
                && std::fs::metadata(e.path())
                    .map(|m| m.is_dir() || m.is_file())
                    .unwrap_or(false)
        })
        .filter_map(|e| e.path().parent().map(PathBuf::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discover_repos_finds_nested_dot_git_dirs() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("b/c/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("d/not-a-repo")).unwrap();
        let mut found: Vec<_> = discover_repos_under(dir.path())
            .into_iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                std::path::PathBuf::from("a"),
                std::path::PathBuf::from("b/c"),
            ]
        );
    }

    #[test]
    fn discover_repos_finds_linked_worktrees() {
        let dir = TempDir::new().unwrap();
        // Primary checkout: .git is a directory.
        std::fs::create_dir_all(dir.path().join("real-repo/.git")).unwrap();
        // Linked worktree: .git is a file (pointer to the main repo).
        std::fs::create_dir_all(dir.path().join("linked-worktree")).unwrap();
        std::fs::write(
            dir.path().join("linked-worktree/.git"),
            "gitdir: /path/to/main/.git/worktrees/foo\n",
        )
        .unwrap();
        // Not a repo: no .git at all.
        std::fs::create_dir_all(dir.path().join("not-a-repo")).unwrap();

        let mut found: Vec<_> = discover_repos_under(dir.path())
            .into_iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                std::path::PathBuf::from("linked-worktree"),
                std::path::PathBuf::from("real-repo"),
            ]
        );
    }
}
