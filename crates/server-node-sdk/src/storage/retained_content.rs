//! The reachability view shared by scrub, availability, recovery and cleanup.
//! Paths describe references; the immutable manifest hash identifies repair work.
use super::*;

pub(crate) const MANIFEST_SUBJECT_PREFIX: &str = "cas-manifest:";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RetainedReference {
    pub key: Option<String>,
    pub object_id: Option<String>,
    pub version_id: Option<String>,
    pub manifest_hash: String,
    #[serde(default)]
    pub snapshot_only: bool,
}

impl RetainedReference {
    pub fn subject(&self) -> Option<String> {
        if self.snapshot_only || self.key.is_none() {
            return Some(format!("{MANIFEST_SUBJECT_PREFIX}{}", self.manifest_hash));
        }
        let key = self.key.as_ref()?;
        Some(match &self.version_id {
            Some(version) => format!("{key}@{version}"),
            None => key.clone(),
        })
    }
}

#[derive(Default)]
pub(crate) struct RetainedContent {
    /// References are indexed by subject within each manifest. A manifest can
    /// be shared by a deep version history, so linear duplicate detection here
    /// turns catalog construction into quadratic work.
    pub manifests: BTreeMap<String, BTreeMap<String, RetainedReference>>,
    subjects: BTreeMap<String, String>,
    pub current_keys: usize,
    pub version_indexes: usize,
    pub version_records: usize,
}

impl RetainedContent {
    pub(super) async fn load(metadata: &dyn MetadataStore, current: &CurrentState) -> Result<Self> {
        let mut result = Self {
            current_keys: current.objects.len(),
            ..Self::default()
        };
        for (key, hash) in &current.objects {
            result.insert(RetainedReference {
                key: Some(key.clone()),
                object_id: current.object_ids.get(key).cloned(),
                version_id: None,
                manifest_hash: hash.clone(),
                snapshot_only: false,
            });
        }
        let paths: HashMap<_, _> = current.object_ids.iter().map(|(k, v)| (v, k)).collect();
        // Load one index at a time: metadata histories can be substantially larger
        // than the deduplicated set of retained content references.
        let object_ids = metadata.list_version_index_object_ids().await?;
        result.version_indexes = object_ids.len();
        for object_id in object_ids {
            let Some(index) = metadata.load_version_index_by_object_id(&object_id).await? else {
                continue;
            };
            result.version_records += index.versions.len();
            for record in index.versions.values() {
                result.insert(RetainedReference {
                    key: record
                        .logical_path
                        .clone()
                        .or_else(|| paths.get(&object_id).map(|p| (*p).clone())),
                    object_id: Some(object_id.clone()),
                    version_id: Some(record.version_id.clone()),
                    manifest_hash: record.manifest_hash.clone(),
                    snapshot_only: false,
                });
            }
        }
        // A compacted version index must not make snapshot-only bytes invisible.
        // One immutable reference is sufficient to protect/recover shared bytes.
        // Snapshot payloads can be much larger than their lightweight index
        // rows. Decode one at a time so ordinary availability and repair work
        // retains the same bounded-memory behavior as version-index loading.
        for snapshot_info in metadata.list_snapshot_infos().await? {
            let Some(snapshot) = metadata.load_snapshot_by_id(&snapshot_info.id).await? else {
                continue;
            };
            for (key, hash) in snapshot.objects {
                if !result.manifests.contains_key(&hash) {
                    result.insert(RetainedReference {
                        object_id: snapshot.object_ids.get(&key).cloned(),
                        key: Some(key),
                        version_id: None,
                        manifest_hash: hash,
                        snapshot_only: true,
                    });
                }
            }
        }
        Ok(result)
    }

    fn insert(&mut self, reference: RetainedReference) {
        let Some(subject) = reference.subject() else {
            return;
        };
        let manifest_hash = reference.manifest_hash.clone();
        self.manifests
            .entry(manifest_hash.clone())
            .or_default()
            .entry(subject.clone())
            .or_insert(reference);
        self.subjects.entry(subject).or_insert(manifest_hash);
    }

    pub fn reference_for_subject(&self, subject: &str) -> Option<&RetainedReference> {
        let manifest_hash = self.subjects.get(subject)?;
        self.manifests.get(manifest_hash)?.get(subject)
    }

    pub fn subjects(&self) -> Vec<String> {
        self.subjects.keys().cloned().collect()
    }
}

impl PersistentStore {
    pub(crate) async fn retained_content(&self) -> Result<RetainedContent> {
        RetainedContent::load(
            self.metadata_store.as_ref(),
            &self.metadata_store.load_current_state().await?,
        )
        .await
    }
}
