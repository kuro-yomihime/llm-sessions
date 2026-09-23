//! Workspace identity: canonicalize a recorded cwd, then roll it up to the
//! repository it belongs to.
//!
//! Never decode an agent's directory-name encoding here. Every encoding in use
//! (`-home-user-x`, `--abs--`, …) replaces `/` with `-` and is therefore
//! irreversible: `/tmp/a-b` and `/tmp/a/b` both encode to `-tmp-a-b`. Callers
//! pass the path the agent *recorded*; decoding survives only as a display
//! fallback tagged [`crate::model::PathTrust::Decoded`].

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Caches repo-root lookups; one scan touches the same directories repeatedly.
#[derive(Debug, Default)]
pub struct Resolver {
    roots: HashMap<PathBuf, Option<PathBuf>>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonical workspace root for a recorded cwd.
    ///
    /// Resolves symlinks when the path still exists, walks up to the nearest
    /// enclosing repository, and maps a linked worktree onto its main
    /// worktree. Falls back to the literal path when the directory is gone.
    pub fn resolve(&mut self, recorded: &Path) -> PathBuf {
        let canonical = fs::canonicalize(recorded).unwrap_or_else(|_| recorded.to_path_buf());
        if let Some(cached) = self.roots.get(&canonical) {
            return cached.clone().unwrap_or(canonical);
        }
        let root = repo_root(&canonical);
        self.roots.insert(canonical.clone(), root.clone());
        root.unwrap_or(canonical)
    }
}

/// Nearest ancestor holding `.git`, with linked worktrees mapped to their main
/// worktree. `None` when the path is not inside a repository.
fn repo_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        match fs::metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return Some(dir.to_path_buf()),
            // A `.git` *file* means a linked worktree or submodule.
            Ok(_) => return main_worktree(&dot_git).or_else(|| Some(dir.to_path_buf())),
            Err(_) => {}
        }
    }
    None
}

/// `.git` file -> `gitdir: <main>/.git/worktrees/<name>` -> `<main>`.
///
/// `commondir` inside the worktree admin directory points at the shared `.git`
/// directory; its parent is the main worktree.
fn main_worktree(dot_git_file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dot_git_file).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    let admin = resolve_relative(dot_git_file.parent()?, Path::new(gitdir));

    let common = fs::read_to_string(admin.join("commondir")).ok()?;
    let common = resolve_relative(&admin, Path::new(common.trim()));
    let common = fs::canonicalize(&common).unwrap_or(common);
    // `<main>/.git` -> `<main>`; a bare repo has no worktree to roll up to.
    common.parent().map(Path::to_path_buf)
}

fn resolve_relative(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// `~`-shortened display form.
pub fn display(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_worktree_rolls_up_to_main_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        let admin = main.join(".git/worktrees/feature");
        fs::create_dir_all(&admin).unwrap();
        // Real git writes a relative `../..` here.
        fs::write(admin.join("commondir"), "../..\n").unwrap();

        let linked = tmp.path().join("wt/feature");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        let resolved = Resolver::new().resolve(&linked);
        assert_eq!(resolved, fs::canonicalize(&main).unwrap());
    }

    #[test]
    fn subdirectory_rolls_up_to_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("src/inner");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            Resolver::new().resolve(&deep),
            fs::canonicalize(&root).unwrap()
        );
    }

    #[test]
    fn vanished_directory_keeps_its_literal_path() {
        let gone = Path::new("/nonexistent-llm-sessions-test/project");
        assert_eq!(Resolver::new().resolve(gone), gone.to_path_buf());
    }
}
