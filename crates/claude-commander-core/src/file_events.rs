//! Filesystem invalidations. Watch directories so atomic replacement is visible.
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

pub struct FileEvents {
    _watcher: RecommendedWatcher,
    rx: watch::Receiver<u64>,
    failed: Arc<AtomicBool>,
    review_changed: Arc<AtomicBool>,
}

impl FileEvents {
    pub fn watch(path: &Path, recursive: bool) -> notify::Result<Self> {
        Self::watch_filtered(path, recursive, |_| true)
    }

    pub fn watch_filtered(
        path: &Path,
        recursive: bool,
        accept: impl Fn(&Path) -> bool + Send + 'static,
    ) -> notify::Result<Self> {
        Self::watch_paths(&[path.to_path_buf()], recursive, accept)
    }

    /// State stores are flat siblings of worktrees; never recursively register
    /// the data tree merely to observe these three small directories.
    pub fn state(data_dir: &Path) -> notify::Result<Self> {
        let paths = [
            data_dir.to_path_buf(),
            data_dir.join("comments"),
            data_dir.join("reviewed"),
        ];
        for path in &paths {
            std::fs::create_dir_all(path)?;
        }
        let review_changed = Arc::new(AtomicBool::new(false));
        let review_flag = review_changed.clone();
        let mut events = Self::watch_paths(&paths, false, move |path| {
            let review = path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|name| name == "comments" || name == "reviewed")
                && path.extension().is_some_and(|ext| ext == "json");
            if review {
                review_flag.store(true, Ordering::Relaxed);
            }
            path.file_name().is_some_and(|name| name == "state.json")
                || path
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|name| name == "comments" || name == "reviewed")
                    && path.extension().is_some_and(|ext| ext == "json")
        })?;
        events.review_changed = review_changed;
        Ok(events)
    }

    fn watch_paths(
        paths: &[PathBuf],
        recursive: bool,
        accept: impl Fn(&Path) -> bool + Send + 'static,
    ) -> notify::Result<Self> {
        let (tx, rx) = watch::channel(0_u64);
        let failed = Arc::new(AtomicBool::new(false));
        let (paths, handler) = invalidation_handler(paths, accept, tx, failed.clone())?;
        let mut watcher = notify::recommended_watcher(handler)?;
        for path in &paths {
            watcher.watch(
                path,
                if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )?;
        }
        Ok(Self {
            _watcher: watcher,
            rx,
            failed,
            review_changed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Consume invalidations synchronously when checking a cached resource.
    pub fn take_changed(&mut self) -> bool {
        let changed = self.rx.has_changed().unwrap_or(true);
        acknowledge_change(&mut self.rx, changed)
    }
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Relaxed)
    }

    /// Watch only the root and Git metadata. Nested worktree edits retain the
    /// ordinary TTL: recursively registering build trees consumes inotify watches.
    pub fn git(path: &Path) -> notify::Result<Self> {
        let mut events = Self::watch(path, false)?;
        let dotgit = path.join(".git");
        let gitdir: PathBuf = match std::fs::read_to_string(&dotgit) {
            Ok(text) => path.join(text.trim().strip_prefix("gitdir: ").unwrap_or(text.trim())),
            Err(_) => dotgit,
        };
        if gitdir.is_dir() {
            events
                ._watcher
                .watch(&gitdir, RecursiveMode::NonRecursive)?;
            let common = std::fs::read_to_string(gitdir.join("commondir"))
                .map(|p| gitdir.join(p.trim()))
                .unwrap_or(gitdir);
            events
                ._watcher
                .watch(&common, RecursiveMode::NonRecursive)?;
            if common.join("refs").is_dir() {
                events
                    ._watcher
                    .watch(&common.join("refs"), RecursiveMode::Recursive)?;
            }
        }
        Ok(events)
    }

    pub fn take_review_changed(&self) -> bool {
        self.review_changed.swap(false, Ordering::Relaxed) || !self.healthy()
    }

    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }
}

fn acknowledge_change(rx: &mut watch::Receiver<u64>, changed: bool) -> bool {
    // A callback can arrive after an empty check. Leave it pending rather
    // than acknowledge an event the caller is not about to invalidate for.
    if changed {
        rx.borrow_and_update();
    }
    changed
}

/// Build the callback and return the paths to register with the watcher.
fn invalidation_handler(
    paths: &[PathBuf],
    accept: impl Fn(&Path) -> bool + Send + 'static,
    tx: watch::Sender<u64>,
    failed: Arc<AtomicBool>,
) -> notify::Result<(
    Vec<PathBuf>,
    impl FnMut(notify::Result<notify::Event>) + Send + 'static,
)> {
    // notify's FSEvents backend canonicalises registrations (notify 8.2.0,
    // src/fsevent.rs:392). Cache the same identities before a root disappears,
    // and register them on every backend so callback paths and roots agree.
    let paths = paths
        .iter()
        .map(std::fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    let roots = paths.clone();
    let handler = move |event: notify::Result<notify::Event>| {
        let relevant = match event {
            Ok(event) => {
                // A directory watch may no longer cover its replacement.
                // Wake the consumer and restore reconciliation on that path.
                // notify 8.2.0 src/inotify.rs:213 emits pathless Rescan on overflow.
                let rescan = event.need_rescan() || event.paths.is_empty();
                let replaced_root = matches!(
                    event.kind,
                    notify::EventKind::Remove(_)
                        | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                ) && event.paths.iter().any(|path| roots.contains(path));
                if replaced_root || rescan {
                    failed.store(true, Ordering::Relaxed);
                }
                replaced_root
                    || rescan
                    || !matches!(event.kind, notify::EventKind::Access(_))
                        && event.paths.iter().any(|path| accept(path))
            }
            Err(_) => {
                failed.store(true, Ordering::Relaxed);
                true
            }
        };
        if relevant {
            tx.send_modify(|g| *g = g.wrapping_add(1));
        }
    };
    Ok((paths, handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_event_after_an_empty_check_remains_pending() {
        let (tx, mut rx) = watch::channel(0_u64);
        let changed = rx.has_changed().unwrap();
        assert!(!changed);
        // The callback runs after the consumer checked but before it acks.
        tx.send_modify(|generation| *generation += 1);
        assert!(!acknowledge_change(&mut rx, changed));
        assert!(
            rx.has_changed().unwrap(),
            "the late event must survive for the next check"
        );
    }

    #[test]
    fn an_observed_change_acknowledges_the_coalesced_batch() {
        let (tx, mut rx) = watch::channel(0_u64);
        tx.send_modify(|generation| *generation += 1);
        let changed = rx.has_changed().unwrap();
        tx.send_modify(|generation| *generation += 1);
        assert!(acknowledge_change(&mut rx, changed));
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn queue_overflow_invalidates_even_without_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tx, rx) = watch::channel(0);
        let failed = Arc::new(AtomicBool::new(false));
        let (_, mut handler) =
            invalidation_handler(&[dir.path().to_path_buf()], |_| false, tx, failed.clone())
                .unwrap();
        handler(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        assert!(rx.has_changed().unwrap());
        assert!(failed.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn state_watch_covers_new_comment_dirs_and_atomic_replacements() {
        for relative in [
            "state.json",
            "comments/session.json",
            "reviewed/session.json",
        ] {
            // Each rename can emit several notifications. A fresh watcher
            // ensures this assertion observes this file's replacement.
            let dir = tempfile::TempDir::new().unwrap();
            let mut events = FileEvents::state(dir.path()).unwrap();
            let target = dir.path().join(relative);
            let temporary = target.with_extension("tmp");
            std::fs::write(&temporary, "updated").unwrap();
            std::fs::rename(&temporary, &target).unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), events.changed())
                    .await
                    .unwrap()
            );
        }
        // No earlier rename notifications can masquerade as a worktree event.
        let dir = tempfile::TempDir::new().unwrap();
        let mut events = FileEvents::state(dir.path()).unwrap();
        let worktree = dir.path().join("worktrees/session");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("source.rs"), "unrelated").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), events.changed())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_canonical_root_rename_invalidates_a_watch_created_through_a_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        std::fs::create_dir_all(real.join("comments")).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let canonical_root = std::fs::canonicalize(real.join("comments")).unwrap();
        let (tx, mut rx) = watch::channel(0_u64);
        let failed = Arc::new(AtomicBool::new(false));
        let (registered, mut handler) =
            invalidation_handler(&[alias.join("comments")], |_| false, tx, failed.clone()).unwrap();

        // The old path is gone when the callback arrives. Normalise watch roots
        // at registration, rather than trying to resolve a deleted event path.
        std::fs::rename(real.join("comments"), real.join("old-comments")).unwrap();
        handler(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::From),
        ))
        .add_path(canonical_root.clone())));

        assert!(
            rx.has_changed().unwrap(),
            "root replacement must wake reconciliation"
        );
        assert!(
            failed.load(Ordering::Relaxed),
            "the old watch is no longer healthy"
        );
        rx.borrow_and_update();
        assert_eq!(registered, vec![canonical_root]);
    }

    #[tokio::test]
    async fn replacing_state_child_directory_restores_reconciliation() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut events = FileEvents::state(dir.path()).unwrap();
        std::fs::rename(dir.path().join("comments"), dir.path().join("old-comments")).unwrap();
        std::fs::create_dir(dir.path().join("comments")).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), events.changed())
                .await
                .unwrap()
        );
        assert!(!events.healthy());
    }

    #[tokio::test]
    async fn git_watch_observes_a_real_lockfile() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("Cargo.lock");
        std::fs::write(&target, "old").unwrap();
        let mut events = FileEvents::git(dir.path()).unwrap();
        std::fs::write(&target, "new").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.changed())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn observes_atomic_replacement_and_in_place_edits() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("state.json");
        std::fs::write(&target, "old").unwrap();
        let mut events = FileEvents::watch(dir.path(), true).unwrap();
        let temp = dir.path().join("replacement.tmp");
        std::fs::write(&temp, "new").unwrap();
        std::fs::rename(temp, &target).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), events.changed())
                .await
                .unwrap()
        );
        std::fs::write(target, "changed in place").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), events.changed())
                .await
                .unwrap()
        );
    }
}
