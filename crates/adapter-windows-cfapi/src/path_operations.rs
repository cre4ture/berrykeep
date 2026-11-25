//! Serialize provider operations on a path through receipt finalization.
//!
//! The network upload lease ends before the close callback's identity/USN
//! checks. Metadata repair must wait for that entire callback, not only the
//! server response. These gates cover provider threads; they do not prevent
//! user writes, which remain guarded by content snapshots and USNs.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

fn canonical_key(path: &Path) -> String {
    let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
    if let Some(unc) = path.strip_prefix("\\\\?\\unc\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    }
}

pub(crate) fn gate(path: &Path) -> Arc<Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();
    let key = canonical_key(path);
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    if gates.len() >= 4096 {
        gates.retain(|_, gate| gate.strong_count() != 0);
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}

/// Deduplicate and order by the same key used by the gate factory. Ordering raw
/// paths can both lock a case-only alias twice and invert acquisition order.
pub(crate) fn gates<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> Vec<Arc<Mutex<()>>> {
    let mut grouped = BTreeMap::new();
    for path in paths {
        let path = path.as_ref();
        grouped
            .entry(canonical_key(path))
            .or_insert_with(|| gate(path));
    }
    grouped.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_groups_deduplicate_case_slash_and_verbatim_aliases() {
        for paths in [
            vec![r"C:\sync\a", r"\\?\c:\SYNC\A", "C:/sync/a"],
            vec![r"\\server\share\a", r"\\?\UNC\SERVER\share\A"],
        ] {
            let grouped = gates(paths.iter().map(Path::new));
            assert_eq!(grouped.len(), 1, "aliases must never lock one mutex twice");
            assert!(Arc::ptr_eq(&grouped[0], &gate(Path::new(paths[0]))));
            let _guards = grouped
                .iter()
                .map(|gate| {
                    gate.try_lock()
                        .expect("canonical group must not self-deadlock")
                })
                .collect::<Vec<_>>();
        }
    }

    #[test]
    fn gate_group_order_is_independent_of_input_case_and_order() {
        let first = gates([Path::new(r"C:\sync\z"), Path::new(r"C:\sync\A")]);
        let second = gates([Path::new(r"c:\SYNC\a"), Path::new(r"c:\sync\Z")]);
        assert_eq!(first.len(), 2);
        assert!(
            first.iter().zip(&second).all(|(a, b)| Arc::ptr_eq(a, b)),
            "all executors must acquire the same canonical order"
        );
    }
}
