//! Serialize provider operations on a path through receipt finalization.
//!
//! The network upload lease ends before the close callback's identity/USN
//! checks. Metadata repair must wait for that entire callback, not only the
//! server response. These gates cover provider threads; they do not prevent
//! user writes, which remain guarded by content snapshots and USNs.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub(crate) fn gate(path: &Path) -> Arc<Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let key = path.strip_prefix("\\\\?\\").unwrap_or(&path);
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
        return gate;
    }
    if gates.len() >= 4096 {
        gates.retain(|_, gate| gate.strong_count() != 0);
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key.to_owned(), Arc::downgrade(&gate));
    gate
}
