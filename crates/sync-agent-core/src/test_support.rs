use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Reserve a distinct directory even when clocks repeat or an earlier run left files.
pub(crate) fn test_root() -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    loop {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "berrykeep-sync-agent-test-{}-{id}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => return root,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!(
                "failed to reserve test directory {}: {error}",
                root.display()
            ),
        }
    }
}

#[test]
fn test_roots_are_reserved_and_independent() {
    let first = test_root();
    let second = test_root();
    assert_ne!(first, second);
    assert!(first.is_dir());
    assert!(second.is_dir());
    fs::remove_dir_all(first).unwrap();
    assert!(second.is_dir());
    fs::remove_dir_all(second).unwrap();
}
