//! Filesystem helpers that are safe in the presence of open file handles.
//!
//! On Windows, `std::fs::remove_dir_all` deletes entries one-by-one and can fail
//! *partway through* with `ERROR_SHARING_VIOLATION` (os error 32) when another
//! process holds an open handle inside the tree — e.g. a filesystem watcher using
//! `ReadDirectoryChangesW`, or a child process whose current working directory is
//! inside the tree. A partial delete leaves a corrupted git worktree (its `.git`
//! pointer can be removed while working-tree files survive), which then wedges every
//! subsequent operation. These helpers make deletion **all-or-nothing**.
//!
//! On non-Windows platforms these are thin wrappers around `std::fs::remove_dir_all`
//! so macOS/Linux behavior is unchanged.

use std::{io, path::Path};

/// Prefix used for relocated ("trashed") directories awaiting background deletion.
pub const TRASH_PREFIX: &str = ".vk-trash-";

/// Remove a directory tree without ever leaving it partially deleted.
///
/// On Windows this first atomically renames the directory to a sibling
/// `.vk-trash-*` name. Rename either fully frees the original path or fully fails:
///   - success: the original path is immediately free for reuse; the relocated copy
///     is deleted on a background thread with retry/backoff. If it still can't be
///     deleted (a handle is held for a long time), it is left for [`sweep_trash`].
///   - failure: the directory is left **fully intact** and the error is returned. We
///     deliberately do not fall back to a destructive partial `remove_dir_all`.
///
/// A non-existent path is treated as success.
pub fn remove_dir_all_safe(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }

    #[cfg(windows)]
    {
        let trash = unique_trash_path(path)?;
        // Rename is atomic: it either frees `path` entirely or fails leaving it intact.
        match std::fs::rename(path, &trash) {
            Ok(()) => {
                spawn_background_delete(trash);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Remove a directory tree, retrying briefly on Windows sharing violations.
///
/// Unlike [`remove_dir_all_safe`], this deletes in place (it may delete partway
/// before a retry). Use it only for directories that are **not** live git worktrees
/// — e.g. `.git/worktrees/<name>` metadata or an already-emptied workspace dir —
/// where a partial delete is not corrupting.
pub fn remove_dir_all_with_retry(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }

    #[cfg(windows)]
    {
        use std::time::Duration;

        let mut delay = Duration::from_millis(50);
        let mut last_err = None;
        for _ in 0..6 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(e) if is_sharing_violation(&e) => {
                    last_err = Some(e);
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| io::Error::other("remove_dir_all_with_retry: retries exhausted")))
    }
}

/// Best-effort removal of leftover `.vk-trash-*` directories directly under `dir`.
///
/// Called from orphan cleanup so relocated directories that couldn't be deleted
/// immediately (because a handle was still held) are eventually reclaimed.
pub fn sweep_trash(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_trash = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(TRASH_PREFIX));
        if is_trash && path.is_dir() {
            let _ = remove_dir_all_with_retry(&path);
        }
    }
}

#[cfg(windows)]
fn is_sharing_violation(e: &io::Error) -> bool {
    // 32 = ERROR_SHARING_VIOLATION, 5 = ERROR_ACCESS_DENIED (seen for read-only or
    // briefly-locked entries mid-delete). Both are transient lock conditions.
    matches!(e.raw_os_error(), Some(32) | Some(5))
}

/// Pick an unused sibling `.vk-trash-*` path next to `path`.
#[cfg(windows)]
fn unique_trash_path(path: &Path) -> io::Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("cannot trash a path without a parent"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dir".to_string());
    let pid = std::process::id();

    for _ in 0..10_000 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{TRASH_PREFIX}{name}-{pid}-{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("could not allocate a unique trash path"))
}

/// Delete a relocated trash directory on a detached thread, retrying on transient
/// Windows sharing violations. Anything left behind is reclaimed by [`sweep_trash`].
#[cfg(windows)]
fn spawn_background_delete(trash: std::path::PathBuf) {
    std::thread::spawn(move || {
        use std::time::Duration;

        let mut delay = Duration::from_millis(50);
        for _ in 0..8 {
            match std::fs::remove_dir_all(&trash) {
                Ok(()) => return,
                Err(e) if is_sharing_violation(&e) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(_) => return, // non-retryable; leave for sweep_trash
            }
        }
        tracing::debug!(
            "trash dir still locked after retries, leaving for sweep: {}",
            trash.display()
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_dir_all_safe_missing_path_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(remove_dir_all_safe(&missing).is_ok());
    }

    #[test]
    fn remove_dir_all_safe_removes_populated_tree() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("worktree");
        std::fs::create_dir_all(target.join("nested")).unwrap();
        std::fs::write(target.join("nested").join("file.txt"), b"hi").unwrap();

        remove_dir_all_safe(&target).unwrap();

        // The canonical path is freed immediately (on Windows the actual byte removal
        // may finish on a background thread, but the path is renamed away).
        assert!(!target.exists());
    }

    #[test]
    fn sweep_trash_ignores_non_trash_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep-me");
        std::fs::create_dir_all(&keep).unwrap();
        sweep_trash(dir.path());
        assert!(keep.exists());
    }

    #[cfg(windows)]
    #[test]
    fn remove_dir_all_safe_keeps_tree_intact_when_locked() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("worktree");
        std::fs::create_dir_all(&target).unwrap();
        let locked = target.join("locked.txt");

        // Hold an exclusive handle (no share-delete) on the directory itself by making
        // it the process's footprint via an open file with no sharing. We open the
        // file without FILE_SHARE_* so a rename of the parent is blocked.
        use std::os::windows::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .share_mode(0) // deny all sharing
            .open(&locked)
            .unwrap();
        f.write_all(b"x").unwrap();

        let result = remove_dir_all_safe(&target);

        // Either the OS still allowed the rename (path freed) or it refused and we left
        // the tree intact — but never a partial deletion of a surviving file.
        if result.is_err() {
            assert!(target.exists(), "on failure the tree must be left intact");
            assert!(locked.exists(), "locked file must not be partially deleted");
        }
        drop(f);
    }
}
