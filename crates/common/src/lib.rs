use serde::{Deserialize, Serialize};
use unicode_general_category::{GeneralCategory, get_general_category};
use uuid::Uuid;

pub mod content_fingerprint;
pub mod logging;
pub mod range_chunk_cache;
pub mod traced_mutex;
pub mod traced_rwlock;
pub mod xmp;

pub type NodeId = Uuid;
pub type ClusterId = Uuid;
pub type DeviceId = Uuid;

/// Maximum UTF-8 byte length accepted for a host name shown in IronMesh user
/// interfaces. This matches the DNS host name limit while deliberately not
/// requiring the operating system's display value to be a DNS name.
pub const MAX_NODE_HOSTNAME_BYTES: usize = 255;

/// Normalizes an operating-system supplied node host name for display and
/// distribution. Host names are descriptive metadata only, never an identity
/// or routing input.
pub fn normalize_node_hostname(value: impl AsRef<str>) -> Option<String> {
    let value = value.as_ref();
    let hostname = value.trim();
    (!value.chars().any(is_disallowed_hostname_character)
        && !hostname.is_empty()
        && hostname.len() <= MAX_NODE_HOSTNAME_BYTES)
        .then(|| hostname.to_string())
}

fn is_disallowed_hostname_character(character: char) -> bool {
    matches!(
        get_general_category(character),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::PrivateUse
            | GeneralCategory::Surrogate
            | GeneralCategory::Unassigned
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_NODE_HOSTNAME_BYTES, normalize_node_hostname};

    #[test]
    fn normalizes_display_hostnames_without_accepting_invalid_values() {
        assert_eq!(
            normalize_node_hostname("  edge-a  ").as_deref(),
            Some("edge-a")
        );
        assert_eq!(normalize_node_hostname("\nedge-a"), None);
        assert_eq!(normalize_node_hostname("edge-\u{202e}a"), None);
        assert_eq!(normalize_node_hostname("edge-\u{200b}a"), None);
        assert_eq!(normalize_node_hostname("   "), None);
        assert_eq!(
            normalize_node_hostname("a".repeat(MAX_NODE_HOSTNAME_BYTES + 1)),
            None
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageObjectMeta {
    pub key: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub key: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthStatus {
    pub node_id: NodeId,
    pub role: String,
    pub online: bool,
    pub version: String,
    pub revision: String,
}
