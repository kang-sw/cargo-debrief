use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Categorized file changes between two index states.
pub struct FileChanges {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

/// Get the current HEAD commit hash.
///
/// Shells out to `git -C <repo_root> rev-parse HEAD`.
/// Returns an error if git is not available or the directory is not a repo.
pub fn head_commit(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .context("failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }

    let hash = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    Ok(hash.trim().to_string())
}

/// Detect changed files between two index states.
///
/// - `from == None`: returns all tracked files via `git ls-files`, all in `added`.
/// - `from == Some(hash)`: returns files changed between `hash` and HEAD via
///   `git diff --name-status`. Renames are split into a delete of the old path
///   and an add of the new path.
pub fn changed_files(repo_root: &Path, from: Option<&str>) -> Result<FileChanges> {
    match from {
        None => all_tracked_files(repo_root),
        Some(from_hash) => diff_since(repo_root, from_hash),
    }
}

fn all_tracked_files(repo_root: &Path) -> Result<FileChanges> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "ls-files"])
        .output()
        .context("failed to run git ls-files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-files failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    let added = stdout.lines().map(String::from).collect();

    Ok(FileChanges {
        added,
        modified: vec![],
        deleted: vec![],
    })
}

fn diff_since(repo_root: &Path, from_hash: &str) -> Result<FileChanges> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "diff",
            "--name-status",
            from_hash,
            "HEAD",
        ])
        .output()
        .context("failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --name-status failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    let mut changes = FileChanges {
        added: vec![],
        modified: vec![],
        deleted: vec![],
    };

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        match parts.as_slice() {
            // Renames: "R<score>\told-path\tnew-path" — treat as delete + add.
            // Copies: "C<score>\told-path\tnew-path" — treat as add of new path.
            [status, old_path, new_path] if status.starts_with('R') || status.starts_with('C') => {
                if status.starts_with('R') {
                    changes.deleted.push(old_path.to_string());
                }
                changes.added.push(new_path.to_string());
            }
            [status, path] => match status.chars().next() {
                Some('M') => changes.modified.push(path.to_string()),
                Some('A') => changes.added.push(path.to_string()),
                Some('D') => changes.deleted.push(path.to_string()),
                _ => {} // skip unknown status codes
            },
            _ => {} // skip malformed lines
        }
    }

    Ok(changes)
}

/// Walk parent directories looking for `.git` (file or directory).
/// Returns the directory that contains `.git`, or None if not found.
/// Handles both regular `.git/` dirs and `.git` files (worktrees, submodules).
///
/// Pure filesystem traversal — no subprocess.
pub fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };

    loop {
        let git_path = current.join(".git");
        if git_path.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Get the current HEAD commit hash for the given git root.
///
/// Shells out to `git -C <root> rev-parse HEAD`.
pub fn current_head(root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .context("failed to run git rev-parse HEAD")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }

    let hash = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    Ok(hash.trim().to_string())
}

/// Parse `git -C <root> status --porcelain` and compute sha256 per dirty file.
/// Returns a map from repo-relative path to sha256 content hash.
///
/// Covers tracked-modified, staged, deleted (hash is all-zeros for deleted),
/// and untracked files. `.gitignore` is respected.
pub fn dirty_files(root: &Path) -> Result<HashMap<PathBuf, [u8; 32]>> {
    let output = Command::new("git")
        .args(["-C", &root.to_string_lossy(), "status", "--porcelain"])
        .output()
        .context("failed to run git status --porcelain")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git status --porcelain failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("git output is not valid UTF-8")?;
    let mut result = HashMap::new();

    for line in stdout.lines() {
        // Porcelain format: XY <path> or XY <path> -> <renamed-path>
        if line.len() < 4 {
            continue;
        }
        let status_x = line.as_bytes()[0];
        let status_y = line.as_bytes()[1];

        // Skip lines that are only deletions with no remaining file
        let is_deleted = status_x == b'D' || status_y == b'D';

        // Extract path (skip "XY " prefix)
        let path_str = &line[3..];
        // Handle renames: "XY old -> new"
        let path_str = if let Some(arrow_pos) = path_str.find(" -> ") {
            &path_str[arrow_pos + 4..]
        } else {
            path_str
        };

        let rel_path = PathBuf::from(path_str);
        let abs_path = root.join(&rel_path);

        let hash = if is_deleted && !abs_path.exists() {
            [0u8; 32]
        } else if abs_path.is_file() {
            let content = std::fs::read(&abs_path)
                .with_context(|| format!("failed to read dirty file: {}", abs_path.display()))?;
            let mut hasher = Sha256::new();
            hasher.update(&content);
            let result = hasher.finalize();
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&result);
            hash
        } else {
            // Directory or non-existent — skip
            continue;
        };

        result.insert(rel_path, hash);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn head_commit_returns_valid_hash() {
        let hash = head_commit(repo_root()).expect("head_commit should succeed");
        assert_eq!(hash.len(), 40, "expected 40-char hex hash, got: {hash:?}");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex: {hash:?}"
        );
    }

    #[test]
    fn changed_files_none_lists_tracked_files() {
        let changes = changed_files(repo_root(), None).expect("changed_files(None) should succeed");
        assert!(
            changes.added.contains(&"src/main.rs".to_string()),
            "expected src/main.rs in added, got: {:?}",
            changes.added
        );
        assert!(changes.modified.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn changed_files_from_known_commit_returns_diff() {
        // This is the "feat(service): DebriefService async trait + InProcessService stub"
        // commit. Files known to have been modified since then include src/config.rs
        // and src/service.rs.
        let from_hash = "b9436123d648e7a0906fbb82f24f1541c30eb2bb";
        let changes = changed_files(repo_root(), Some(from_hash))
            .expect("changed_files(Some(hash)) should succeed");

        assert!(
            changes.modified.contains(&"src/config.rs".to_string()),
            "expected src/config.rs in modified, got: {:?}",
            changes.modified
        );
        assert!(
            changes.modified.contains(&"src/service.rs".to_string()),
            "expected src/service.rs in modified, got: {:?}",
            changes.modified
        );
    }

    #[test]
    fn head_commit_on_non_repo_path_returns_error() {
        let err = head_commit(Path::new("/tmp")).expect_err("should fail on non-repo path");
        assert!(
            !err.to_string().is_empty(),
            "error message should be non-empty"
        );
    }
}
