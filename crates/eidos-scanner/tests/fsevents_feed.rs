//! The FSEvents adapter against the real kernel: a live stream must deliver
//! what happened, and a cursor must refuse to resume a store it did not come
//! from.

#![cfg(target_os = "macos")]

use eidos_scanner::fsevents::{
    current_cursor, store_uuid, FeedMessage, FsEventsCursor, FsEventsFeed,
};
use std::path::Path;
use std::time::{Duration, Instant};

/// Collect paths for up to `budget`, stopping once `wanted` have arrived.
fn collect(feed: &mut FsEventsFeed, wanted: usize, budget: Duration) -> Vec<String> {
    let deadline = Instant::now() + budget;
    let mut seen = Vec::new();
    while Instant::now() < deadline && seen.len() < wanted {
        match feed.recv_timeout(Duration::from_millis(250)) {
            Some(FeedMessage::Batch { changes, event_id }) => {
                assert!(event_id > 0, "a batch must carry the id it is complete to");
                seen.extend(changes.into_iter().map(|c| c.path.display().to_string()));
            }
            Some(FeedMessage::HistoryDone) | Some(FeedMessage::Rescan(_)) | None => {}
        }
    }
    seen
}

#[test]
fn a_live_stream_reports_creates_and_removals() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let Ok(mut feed) = FsEventsFeed::open(&root, None) else {
        eprintln!("skipping: no FSEvents stream available for the temporary directory");
        return;
    };
    // Let the stream reach the kernel before making changes.
    std::thread::sleep(Duration::from_millis(300));
    std::fs::create_dir(root.join("cedar")).unwrap();
    std::fs::write(root.join("cedar/leaf.txt"), b"two").unwrap();
    std::fs::write(root.join("alpha.txt"), b"one").unwrap();
    std::fs::remove_file(root.join("alpha.txt")).unwrap();

    let seen = collect(&mut feed, 3, Duration::from_secs(20));
    assert!(
        seen.iter().any(|p| p.ends_with("cedar")),
        "a new directory must be reported: {seen:?}"
    );
    assert!(
        seen.iter().any(|p| p.ends_with("cedar/leaf.txt")),
        "file events must be per file, not per directory: {seen:?}"
    );
    assert!(
        seen.iter().any(|p| p.ends_with("alpha.txt")),
        "a path that was created and removed must still be reported: {seen:?}"
    );
}

#[test]
fn stored_history_is_replayed_from_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let Some(cursor) = current_cursor(&root) else {
        eprintln!("skipping: this volume keeps no event history");
        return;
    };
    // Changes made while nothing is watching.
    std::fs::write(root.join("written-while-closed.txt"), b"x").unwrap();
    std::thread::sleep(Duration::from_millis(600));

    let mut feed = FsEventsFeed::open(&root, Some(&cursor)).expect("resume from a valid cursor");
    assert!(feed.replaying(), "a resumed stream starts in the past");
    let seen = collect(&mut feed, 1, Duration::from_secs(20));
    assert!(
        seen.iter().any(|p| p.ends_with("written-while-closed.txt")),
        "the stored history must be replayed: {seen:?}"
    );
}

#[test]
fn a_cursor_from_another_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    if store_uuid(&root).is_none() {
        eprintln!("skipping: this volume keeps no event history");
        return;
    }
    let foreign = FsEventsCursor {
        store_uuid: "00000000-0000-0000-0000-000000000000".into(),
        event_id: 1,
    };
    match FsEventsFeed::open(&root, Some(&foreign)) {
        Ok(_) => panic!("a cursor from another store must not resume"),
        Err(error) => assert_eq!(error.kind, eidos_scanner::ScanErrorKind::Unsupported),
    }
}

#[test]
fn a_cursor_names_the_store_it_came_from() {
    let root = Path::new("/System/Volumes/Data");
    let root = if root.exists() {
        root.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    if let Some(cursor) = current_cursor(&root) {
        assert_eq!(cursor.store_uuid, store_uuid(&root).unwrap());
        assert_eq!(cursor.store_uuid.len(), 36);
        assert!(cursor.event_id > 0);
    }
}
