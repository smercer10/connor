//! The project model: where the project root is, and a background walk of
//! its files respecting ignore rules. The picker consumes it today; the
//! file tree and project-wide search reuse it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;

use crate::watch::AppEvent;

/// A chunk of walked project files; `done` marks the walk's last chunk.
pub struct FileBatch {
    pub generation: u64,
    pub paths: Vec<String>,
    pub done: bool,
}

/// Paths per batch: small enough that the first results paint within
/// milliseconds, large enough that a huge repo doesn't flood the channel.
const BATCH: usize = 512;

/// The project root: the nearest ancestor of the working directory holding
/// `.git`, else the working directory itself.
pub fn root() -> PathBuf {
    std::env::current_dir()
        .map(|dir| root_from(&dir))
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn root_from(start: &Path) -> PathBuf {
    repo_of(start).unwrap_or(start).to_path_buf()
}

/// Whether the file sits inside a git project. An ancestor walk, no
/// subprocess: false is what keeps connor from ever running `git` for a
/// buffer outside a repository.
///
/// The parent is resolved first. `Path::ancestors` ends a relative path at
/// an empty component, and `join` resolves that against the working
/// directory — so `connor ../elsewhere/file` would walk sideways into the
/// repository connor was started in rather than upward out of the file's
/// own directory. A parent that cannot be resolved is not in a repository.
pub fn in_repo(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    fs::canonicalize(dir).is_ok_and(|dir| repo_of(&dir).is_some())
}

/// Whether a directory sits inside a git project: `in_repo` for a
/// directory rather than a file, so no parent has to be resolved first.
/// The project root is the answer for the session as a whole.
pub fn is_repo(dir: &Path) -> bool {
    repo_of(dir).is_some()
}

/// The nearest ancestor holding `.git`, which may be a directory or, in a
/// worktree, a file.
fn repo_of(start: &Path) -> Option<&Path> {
    start.ancestors().find(|dir| dir.join(".git").exists())
}

/// Walks `root` on its own thread, streaming batches into `tx`. Returns the
/// cancel flag; setting it ends the walk within one entry. The thread owns
/// no editor state and also exits when the receiver is gone.
pub fn spawn_walk(root: PathBuf, generation: u64, tx: Sender<AppEvent>) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || walk(&root, generation, &flag, &tx));
    cancel
}

/// The walk body: hidden files skipped, `.gitignore` and friends honored,
/// symlinks not followed — `ignore`'s defaults. Non-UTF-8 names are
/// dropped: the matcher can't rank what a query can't spell, and Ctrl+O
/// still opens them.
fn walk(root: &Path, generation: u64, cancel: &AtomicBool, tx: &Sender<AppEvent>) {
    let mut paths = Vec::with_capacity(BATCH);
    for entry in ignore::WalkBuilder::new(root).build() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || entry.file_type().is_none_or(|t| t.is_dir()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str() else { continue };
        paths.push(rel.to_string());
        if paths.len() == BATCH {
            let batch = FileBatch {
                generation,
                paths: std::mem::replace(&mut paths, Vec::with_capacity(BATCH)),
                done: false,
            };
            if tx.send(AppEvent::Files(batch)).is_err() {
                return;
            }
        }
    }
    let _ = tx.send(AppEvent::Files(FileBatch {
        generation,
        paths,
        done: true,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    /// A fresh scratch directory per test: tests run in parallel, so each
    /// needs its own.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_repository_is_recognised_from_anywhere_beneath_it() {
        let root = scratch_dir("in-repo");
        touch(&root.join(".git/HEAD"));
        touch(&root.join("src/deep/file.rs"));
        assert!(in_repo(&root.join("src/deep/file.rs")));
        assert!(in_repo(&root.join("top.rs")));
        assert_eq!(root_from(&root.join("src/deep")), root);
        assert!(is_repo(&root));
        assert!(is_repo(&root.join("src/deep")));

        // A worktree's `.git` is a file, and counts the same.
        let linked = scratch_dir("in-worktree");
        fs::write(linked.join(".git"), b"gitdir: /elsewhere").unwrap();
        touch(&linked.join("a.rs"));
        assert!(in_repo(&linked.join("a.rs")));
        assert!(is_repo(&linked));

        let loose = scratch_dir("no-repo");
        assert!(!in_repo(&loose.join("a.rs")));
        assert!(!is_repo(&loose));
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&linked).unwrap();
        fs::remove_dir_all(&loose).unwrap();
    }

    /// A path naming `target` relatively from the working directory, which
    /// under `cargo test` is the crate root — a repository.
    #[cfg(unix)]
    fn relative_to_cwd(target: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let ups: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        ups.join(target.strip_prefix("/").unwrap())
    }

    #[test]
    #[cfg(unix)]
    fn a_relative_path_out_of_the_tree_does_not_borrow_the_working_repository() {
        // The walk must climb out of the file's own directory, not stop at
        // the empty ancestor a relative path ends on — which `join` reads
        // as the working directory, a repository whenever connor was
        // started in one.
        let loose = scratch_dir("relative-outside");
        touch(&loose.join("a.rs"));
        let relative = relative_to_cwd(&loose).join("a.rs");
        assert!(relative.is_relative());
        assert!(relative.exists(), "{relative:?}");
        assert!(in_repo(Path::new("src/project.rs")), "the crate is one");
        assert!(!in_repo(&relative));
        fs::remove_dir_all(&loose).unwrap();
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    /// Runs the walk synchronously and returns its batches.
    fn walk_all(root: &Path, generation: u64, cancel: &AtomicBool) -> Vec<FileBatch> {
        let (tx, rx) = mpsc::channel();
        walk(root, generation, cancel, &tx);
        drop(tx);
        rx.into_iter()
            .map(|ev| match ev {
                AppEvent::Files(batch) => batch,
                _ => unreachable!(),
            })
            .collect()
    }

    fn walked_paths(root: &Path) -> Vec<String> {
        let batches = walk_all(root, 7, &AtomicBool::new(false));
        assert!(batches.iter().all(|b| b.generation == 7));
        assert!(batches.last().unwrap().done);
        assert!(batches.iter().rev().skip(1).all(|b| !b.done));
        let mut paths: Vec<String> = batches.into_iter().flat_map(|b| b.paths).collect();
        paths.sort();
        paths
    }

    #[test]
    fn the_root_is_the_nearest_ancestor_holding_dot_git() {
        let dir = scratch_dir("root-ancestor");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("a/b")).unwrap();
        assert_eq!(root_from(&dir.join("a/b")), dir);
        assert_eq!(root_from(&dir), dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_dot_git_file_also_marks_the_root() {
        let dir = scratch_dir("root-worktree");
        fs::write(dir.join(".git"), b"gitdir: elsewhere").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(root_from(&dir.join("sub")), dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_dot_git_the_root_is_the_starting_directory() {
        let dir = scratch_dir("root-bare");
        let start = dir.join("plain");
        fs::create_dir_all(&start).unwrap();
        assert_eq!(root_from(&start), start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_walk_lists_files_but_not_ignored_or_hidden_ones() {
        let dir = scratch_dir("walk-ignore");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".gitignore"), b"target/\n").unwrap();
        touch(&dir.join("src/main.rs"));
        touch(&dir.join("src/draw.rs"));
        touch(&dir.join("README.md"));
        touch(&dir.join("target/debug/out"));
        touch(&dir.join(".hidden"));
        touch(&dir.join("src/.also-hidden"));
        // `.gitignore` itself is a dotfile, hidden like the rest.
        assert_eq!(
            walked_paths(&dir),
            ["README.md", "src/draw.rs", "src/main.rs"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preset_cancel_flag_ends_the_walk_before_any_batch() {
        let dir = scratch_dir("walk-cancel");
        touch(&dir.join("a"));
        touch(&dir.join("b"));
        let batches = walk_all(&dir, 1, &AtomicBool::new(true));
        assert!(batches.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_is_skipped_without_error() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch_dir("walk-nonutf8");
        touch(&dir.join("kept.rs"));
        // Best-effort: APFS refuses invalid UTF-8 names outright, so on
        // macOS this only proves the walk of the rest still succeeds.
        let _ = fs::write(dir.join(OsStr::from_bytes(b"bad-\xff-name")), b"");
        assert_eq!(walked_paths(&dir), ["kept.rs"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
