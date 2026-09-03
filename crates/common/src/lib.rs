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

/// Decodes the stable comma-separated label-filter format used by the node and
/// user interfaces.
///
/// A backslash escapes a comma or another backslash, allowing an XMP keyword
/// containing either character to remain an exact-match filter value. Blank
/// entries are dropped, so a trailing comma or an empty parameter does not
/// become a filter on the empty label.
pub fn parse_comma_separated_labels(
    raw: Option<&str>,
) -> std::result::Result<Vec<String>, &'static str> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let mut labels = Vec::new();
    let mut label = String::new();
    let mut escaped = false;
    for character in raw.chars() {
        if escaped {
            if !matches!(character, ',' | '\\') {
                return Err("label filters may only escape commas and backslashes");
            }
            label.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            ',' => {
                let trimmed_label = label.trim();
                if !trimmed_label.is_empty() {
                    labels.push(trimmed_label.to_string());
                }
                label.clear();
            }
            _ => label.push(character),
        }
    }
    if escaped {
        return Err("label filters must not end with an escape character");
    }
    let label = label.trim();
    if !label.is_empty() {
        labels.push(label.to_string());
    }
    labels.sort_unstable();
    labels.dedup();
    Ok(labels)
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
    use super::{MAX_NODE_HOSTNAME_BYTES, normalize_node_hostname, parse_comma_separated_labels};

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

    #[test]
    fn label_filter_wire_format_decodes_commas_and_backslashes() {
        assert_eq!(
            parse_comma_separated_labels(Some(r"family\, close,travel\\journal")),
            Ok(vec![
                "family, close".to_string(),
                "travel\\journal".to_string()
            ])
        );
        assert_eq!(
            parse_comma_separated_labels(Some(r"invalid\q")),
            Err("label filters may only escape commas and backslashes")
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageObjectMeta {
    pub key: String,
    pub size_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
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
