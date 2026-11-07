//! Bounded, deterministic discovery under the caller's registered project folders.
//! No Git commands run in repositories and no symlinks are followed.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;

mod file;
pub use file::{ProjectSourceFile, SourceEditError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSourceRoot {
    /// Canonical registered folder authorizing this directory.
    pub root: PathBuf,
    /// The registered folder itself or a repository beneath it.
    pub directory: PathBuf,
}

pub fn ignored_directory(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules"
                | "vendor"
                | "build"
                | "dist"
                | "out"
                | "target"
                | "coverage"
                | "tmp"
                | "output"
                | "worktrees"
                | "qa-artifacts"
                | "_bmad-output"
                | "__pycache__"
                | "Pods"
                | "DerivedData"
                | "Carthage"
                | "venv"
        )
}

pub fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

pub fn discover_roots(roots: &[PathBuf]) -> io::Result<Vec<ProjectSourceRoot>> {
    let mut registered = BTreeSet::new();
    for root in roots {
        if !root.is_absolute() {
            continue;
        }
        match fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() && !is_link(&meta) => {
                registered.insert(root.canonicalize()?);
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let mut found = BTreeMap::new();
    // The budget limits optional discovery only. Explicitly attached roots and
    // complete shallower levels remain usable even in very wide repositories.
    let mut remaining = 4096_usize;
    for root in &registered {
        let mut level = vec![root.clone()];
        for depth in 0..=3 {
            let mut next = Vec::new();
            let mut full = false;
            for dir in level {
                let repository =
                    fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| !is_link(&m) && (m.is_file() || m.is_dir()));
                if depth == 0 || repository {
                    found.insert(
                        dir.clone(),
                        ProjectSourceRoot {
                            root: root.clone(),
                            directory: dir.clone(),
                        },
                    );
                }
                if full || remaining == 0 || depth == 3 || (depth != 0 && repository) {
                    continue;
                }
                for entry in fs::read_dir(&dir)? {
                    let entry = entry?;
                    if ignored_directory(&entry.file_name().to_string_lossy()) || !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let child = entry.path();
                    if registered.contains(&child) || is_link(&fs::symlink_metadata(&child)?) {
                        continue;
                    }
                    if next.len() == remaining {
                        // Discard this entire optional level instead of selecting a
                        // filesystem-order-dependent subset or failing chat startup.
                        full = true;
                        break;
                    }
                    next.push(child);
                }
            }
            if full || next.is_empty() {
                break;
            }
            remaining -= next.len();
            level = next;
        }
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_repo_child_worktree_and_attached_root_are_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        let child = root.join("apps/client");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join(".git"), "gitdir: ../unread").unwrap();
        fs::create_dir_all(root.join("node_modules/private/.git")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&child, root.join("link")).unwrap();
        let paths = discover_roots(&[root.clone(), child.clone()]).unwrap();
        assert_eq!(
            paths,
            vec![
                ProjectSourceRoot {
                    root: root.clone(),
                    directory: root
                },
                ProjectSourceRoot {
                    root: child.clone(),
                    directory: child
                }
            ]
        );
    }
}
