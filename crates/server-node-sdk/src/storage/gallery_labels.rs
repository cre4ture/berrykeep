//! Encoding helpers for the `gallery_objects.labels_json` projection column.
//!
//! User labels (for example XMP sidecar keywords such as `private`) are kept
//! outside the content-addressed media bytes, so the gallery projection carries
//! them in a dedicated column. The column holds the full label list as a JSON
//! array, which keeps the projection generic: any keyword vocabulary can be
//! filtered without parsing sidecar files per request.

use anyhow::{Context, Result};

/// Name of the projection column holding the canonical label list.
pub(crate) const GALLERY_LABELS_COLUMN: &str = "labels_json";

/// Column definition shared by the initial schema and the additive migration of
/// databases created before the column existed.
pub(crate) const GALLERY_LABELS_COLUMN_DEFINITION: &str = "TEXT NOT NULL DEFAULT '[]'";

/// Encodes labels into the canonical JSON array persisted in the projection.
///
/// Labels are trimmed, emptied entries are dropped, duplicates are removed and
/// the result is sorted. The canonical form keeps projection rows comparable, so
/// reordering keywords in a sidecar does not produce a spurious gallery revision.
pub(crate) fn encode_gallery_labels(labels: &[String]) -> Result<String> {
    let mut canonical = labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    canonical.sort_unstable();
    canonical.dedup();
    serde_json::to_string(&canonical).context("failed to encode gallery labels")
}

/// Decodes the persisted label column back into its label list.
pub(crate) fn decode_gallery_labels(raw: &str) -> Result<Vec<String>> {
    serde_json::from_str(raw)
        .with_context(|| format!("invalid gallery labels in projection column: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Must match the `DEFAULT` clause of [`GALLERY_LABELS_COLUMN_DEFINITION`].
    const EMPTY_LABELS_JSON: &str = "[]";

    #[test]
    fn empty_labels_encode_to_the_column_default() {
        assert_eq!(encode_gallery_labels(&[]).unwrap(), EMPTY_LABELS_JSON);
        assert!(GALLERY_LABELS_COLUMN_DEFINITION.contains(EMPTY_LABELS_JSON));
    }

    #[test]
    fn labels_are_trimmed_deduplicated_and_sorted() {
        let labels = vec![
            "nsfw".to_string(),
            "  private  ".to_string(),
            "nsfw".to_string(),
            "   ".to_string(),
        ];
        assert_eq!(
            encode_gallery_labels(&labels).unwrap(),
            "[\"nsfw\",\"private\"]"
        );
    }

    #[test]
    fn encoded_labels_round_trip() {
        let labels = vec!["private".to_string(), "familie".to_string()];
        let encoded = encode_gallery_labels(&labels).unwrap();
        assert_eq!(
            decode_gallery_labels(&encoded).unwrap(),
            vec!["familie".to_string(), "private".to_string()]
        );
    }

    #[test]
    fn the_column_default_decodes_to_no_labels() {
        assert!(decode_gallery_labels(EMPTY_LABELS_JSON).unwrap().is_empty());
    }

    #[test]
    fn invalid_label_payloads_are_reported() {
        let error = decode_gallery_labels("not-json").expect_err("invalid labels should fail");
        assert!(error.to_string().contains("invalid gallery labels"));
    }
}
