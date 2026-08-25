//! Bounded parallel directory walker.
//!
//! The walker drives any [`DirectoryLister`] over a tree with a fixed number of
//! worker threads and delivers one [`DirEvent`] per directory to a single
//! consumer. Ordering guarantee: the event for a directory is always delivered
//! before any event for its children, which lets a catalog writer resolve
//! parent object IDs without buffering.
//!
//! Memory is bounded by the event channel capacity (backpressure on workers)
//! plus the pending work queue, which holds at most one small item per
//! discovered-but-unlisted directory.

use crate::entry::{DirectoryLister, RawEntry};
use crate::error::ScanError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Walker-assigned identifier for a directory within one walk. The root is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirToken(pub u64);

#[derive(Debug)]
pub struct DirEvent {
    pub token: DirToken,
    pub parent: Option<DirToken>,
    pub path: PathBuf,
    pub depth: u32,
    pub result: Result<Vec<RawEntry>, ScanError>,
    /// `(entry index, token)` for every child directory that will be walked.
    pub child_tokens: Vec<(usize, DirToken)>,
}

#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub threads: usize,
    pub max_depth: Option<u32>,
    /// Capacity of the event channel (backpressure).
    pub event_buffer: usize,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(1, 32),
            max_depth: None,
            event_buffer: 1024,
            cancel: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    pub directories_listed: u64,
    pub entries: u64,
    pub errors: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
}

struct WorkItem {
    token: DirToken,
    parent: Option<DirToken>,
    path: PathBuf,
    depth: u32,
}

/// Walk `root`, invoking `sink` for every directory event on the calling
/// thread. Returns when the tree is exhausted or the walk was cancelled.
pub fn walk(
    root: &Path,
    lister: &dyn DirectoryLister,
    opts: &WalkOptions,
    mut sink: impl FnMut(DirEvent),
) -> WalkStats {
    let start = Instant::now();
    let threads = opts.threads.max(1);
    let (work_tx, work_rx) = crossbeam_channel::unbounded::<Option<WorkItem>>();
    let (event_tx, event_rx) = crossbeam_channel::bounded::<DirEvent>(opts.event_buffer.max(1));
    let outstanding = Arc::new(AtomicUsize::new(1));
    let next_token = Arc::new(AtomicU64::new(1));
    let cancel = opts
        .cancel
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let stats_dirs = Arc::new(AtomicU64::new(0));
    let stats_entries = Arc::new(AtomicU64::new(0));
    let stats_errors = Arc::new(AtomicU64::new(0));

    work_tx
        .send(Some(WorkItem {
            token: DirToken(0),
            parent: None,
            path: root.to_path_buf(),
            depth: 0,
        }))
        .expect("work channel open");

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let work_rx = work_rx.clone();
            let work_tx = work_tx.clone();
            let event_tx = event_tx.clone();
            let outstanding = outstanding.clone();
            let next_token = next_token.clone();
            let cancel = cancel.clone();
            let stats_dirs = stats_dirs.clone();
            let stats_entries = stats_entries.clone();
            let stats_errors = stats_errors.clone();
            let max_depth = opts.max_depth;
            scope.spawn(move || {
                while let Ok(Some(item)) = work_rx.recv() {
                    if cancel.load(Ordering::Relaxed) {
                        // Drain without listing so termination still happens.
                        finish_item(&outstanding, &work_tx, threads);
                        continue;
                    }
                    let result = lister.list(&item.path);
                    stats_dirs.fetch_add(1, Ordering::Relaxed);
                    let mut child_tokens = Vec::new();
                    let mut children = Vec::new();
                    match &result {
                        Ok(entries) => {
                            stats_entries.fetch_add(entries.len() as u64, Ordering::Relaxed);
                            let within_depth = max_depth.is_none_or(|m| item.depth < m);
                            if within_depth {
                                for (i, e) in entries.iter().enumerate() {
                                    if e.is_traversable_dir() {
                                        let tok =
                                            DirToken(next_token.fetch_add(1, Ordering::Relaxed));
                                        child_tokens.push((i, tok));
                                        children.push(WorkItem {
                                            token: tok,
                                            parent: Some(item.token),
                                            path: item.path.join(&e.name),
                                            depth: item.depth + 1,
                                        });
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            stats_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // Reserve outstanding slots for children *before* sending
                    // the event so the counter never transiently hits zero.
                    outstanding.fetch_add(children.len(), Ordering::AcqRel);
                    let ev = DirEvent {
                        token: item.token,
                        parent: item.parent,
                        path: item.path,
                        depth: item.depth,
                        result,
                        child_tokens,
                    };
                    if event_tx.send(ev).is_err() {
                        // Consumer went away; stop producing.
                        cancel.store(true, Ordering::Relaxed);
                    }
                    for c in children {
                        let _ = work_tx.send(Some(c));
                    }
                    finish_item(&outstanding, &work_tx, threads);
                }
            });
        }
        drop(event_tx);
        drop(work_tx);
        for ev in event_rx.iter() {
            sink(ev);
        }
    });

    WalkStats {
        directories_listed: stats_dirs.load(Ordering::Relaxed),
        entries: stats_entries.load(Ordering::Relaxed),
        errors: stats_errors.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
        cancelled: cancel.load(Ordering::Relaxed),
    }
}

fn finish_item(
    outstanding: &AtomicUsize,
    work_tx: &crossbeam_channel::Sender<Option<WorkItem>>,
    threads: usize,
) {
    if outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
        for _ in 0..threads {
            let _ = work_tx.send(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::std_lister::StdLister;
    use std::collections::HashMap;

    fn make_tree(root: &Path) {
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("a/d")).unwrap();
        std::fs::create_dir_all(root.join("e")).unwrap();
        std::fs::write(root.join("a/one.txt"), "1").unwrap();
        std::fs::write(root.join("a/b/two.txt"), "22").unwrap();
        std::fs::write(root.join("a/b/c/three.txt"), "333").unwrap();
        std::fs::write(root.join("e/four.md"), "4444").unwrap();
        std::fs::write(root.join("five.bin"), "55555").unwrap();
    }

    #[test]
    fn walks_whole_tree_with_parent_before_child() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut seen: HashMap<DirToken, PathBuf> = HashMap::new();
        let mut files = 0;
        let mut order_ok = true;
        let stats = walk(
            tmp.path(),
            &StdLister,
            &WalkOptions {
                threads: 4,
                ..Default::default()
            },
            |ev| {
                if let Some(p) = ev.parent {
                    if !seen.contains_key(&p) {
                        order_ok = false;
                    }
                }
                if let Ok(entries) = &ev.result {
                    files += entries.iter().filter(|e| !e.is_dir()).count();
                }
                seen.insert(ev.token, ev.path.clone());
            },
        );
        assert!(order_ok, "parent event must precede child event");
        assert_eq!(seen.len(), 6, "root, a, a/b, a/b/c, a/d, e");
        assert_eq!(files, 5);
        assert_eq!(stats.directories_listed, 6);
        assert_eq!(stats.errors, 0);
        assert!(!stats.cancelled);
    }

    #[test]
    fn respects_max_depth() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let mut dirs = 0;
        walk(
            tmp.path(),
            &StdLister,
            &WalkOptions {
                threads: 2,
                max_depth: Some(1),
                ..Default::default()
            },
            |_| dirs += 1,
        );
        // root + a + e (depth 1), not a/b, a/d, a/b/c
        assert_eq!(dirs, 3);
    }

    #[test]
    fn reports_missing_root_as_error_event() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let mut errs = 0;
        let stats = walk(&missing, &StdLister, &WalkOptions::default(), |ev| {
            if ev.result.is_err() {
                errs += 1;
            }
        });
        assert_eq!(errs, 1);
        assert_eq!(stats.errors, 1);
    }

    /// Cancels itself once it has listed `after` directories, and counts how
    /// many listings happened in total.
    struct CancelAfter {
        inner: StdLister,
        listed: AtomicU64,
        after: u64,
        cancel: Arc<AtomicBool>,
    }

    impl crate::DirectoryLister for CancelAfter {
        fn list(&self, dir: &std::path::Path) -> Result<Vec<RawEntry>, ScanError> {
            let result = self.inner.list(dir);
            if self.listed.fetch_add(1, Ordering::SeqCst) + 1 >= self.after {
                self.cancel.store(true, Ordering::SeqCst);
            }
            result
        }
        fn volume_info(&self, root: &std::path::Path) -> Result<crate::VolumeInfo, ScanError> {
            self.inner.volume_info(root)
        }
        fn stat(&self, path: &std::path::Path) -> Result<RawEntry, ScanError> {
            self.inner.stat(path)
        }
        fn name(&self) -> &'static str {
            "cancel-after"
        }
    }

    /// Cancellation is observed between work items, so the bound is the
    /// listing that asked to stop plus at most one already in flight per
    /// worker. Counting *events consumed* instead would race: with tiny
    /// directories the workers can finish the whole tree before the consumer
    /// sees its fifth event, which is not a bug and used to fail this test on
    /// a fast machine.
    #[test]
    fn cancellation_stops_early() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::create_dir_all(tmp.path().join(format!("d{i}"))).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let threads = 2;
        let after = 5;
        let lister = CancelAfter {
            inner: StdLister,
            listed: AtomicU64::new(0),
            after,
            cancel: cancel.clone(),
        };
        let stats = walk(
            tmp.path(),
            &lister,
            &WalkOptions {
                threads,
                cancel: Some(cancel),
                ..Default::default()
            },
            |_| {},
        );
        assert!(stats.cancelled);
        assert!(
            stats.directories_listed <= after + threads as u64,
            "listed {} directories after cancelling on the {after}th",
            stats.directories_listed
        );
    }
}
