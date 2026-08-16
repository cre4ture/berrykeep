use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct CoordinationRegistry {
    entries: HashMap<PathBuf, CoordinationEntry>,
}

struct CoordinationEntry {
    lock: Arc<Mutex<()>>,
    caller_count: usize,
}

struct CoordinationPermit {
    target_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl CoordinationPermit {
    fn acquire(target_path: &Path) -> Self {
        let target_path = target_path.to_path_buf();
        let mut registry = coordination_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = registry
            .entries
            .entry(target_path.clone())
            .or_insert_with(|| CoordinationEntry {
                lock: Arc::new(Mutex::new(())),
                caller_count: 0,
            });
        entry.caller_count += 1;
        Self {
            target_path,
            lock: entry.lock.clone(),
        }
    }
}

impl Drop for CoordinationPermit {
    fn drop(&mut self) {
        let mut registry = coordination_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove_entry = registry
            .entries
            .get_mut(&self.target_path)
            .is_some_and(|entry| {
                if !Arc::ptr_eq(&entry.lock, &self.lock) {
                    return false;
                }
                entry.caller_count = entry.caller_count.saturating_sub(1);
                entry.caller_count == 0
            });
        if remove_entry {
            registry.entries.remove(&self.target_path);
        }
    }
}

fn coordination_registry() -> &'static Mutex<CoordinationRegistry> {
    static REGISTRY: OnceLock<Mutex<CoordinationRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CoordinationRegistry::default()))
}

pub(crate) fn coordinate_staged_download<T>(
    target_path: &Path,
    operation: impl FnOnce() -> T,
) -> T {
    // Streaming removes the staged target after the copy, so coordination must cover both
    // downloading and writing to the caller instead of only the final rename.
    let permit = CoordinationPermit::acquire(target_path);
    let _guard = permit
        .lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    operation()
}
