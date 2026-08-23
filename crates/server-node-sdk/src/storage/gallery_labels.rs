//! Encoding helpers for the `gallery_objects.labels_json` projection column.
//!
//! User labels (for example XMP sidecar keywords such as `private`) are kept
//! outside the content-addressed media bytes, so the gallery projection carries
//! them in a dedicated column. The column holds the full label list as a JSON
//! array, which keeps the projection generic: any keyword vocabulary can be
//! filtered without parsing sidecar files per request.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

/// Restricts gallery listings to entries whose labels satisfy it.
///
/// The motivating case is keeping media labelled `private` out of the default
/// view, but the filter is deliberately symmetric so a caller can also ask for
/// exactly those entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GalleryLabelFilter {
    /// Labels an entry must carry to be listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) required: Vec<String>,
    /// Labels that keep an entry out of the listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) excluded: Vec<String>,
}

impl GalleryLabelFilter {
    pub(crate) fn is_empty(&self) -> bool {
        self.required.is_empty() && self.excluded.is_empty()
    }
}

/// Builds a `GLOB` pattern matching projection rows whose label list contains
/// `label`.
///
/// `GLOB` is used instead of `LIKE` because SQLite's (and Turso's) `LIKE` only
/// case-folds ASCII by default, which would make a filter for `private` also
/// match an entry labelled only `Private` -- silently contradicting the
/// storage layer, where `encode_gallery_labels` treats case as significant.
/// `GLOB` is always case-sensitive.
///
/// The canonical encoding quotes every entry, so matching the quoted form only
/// ever matches whole labels: `not-private` is stored as `"not-private"` and
/// cannot satisfy the pattern built for `private`. `GLOB` has no `ESCAPE`
/// clause, so its metacharacters (`*`, `?`, `[`) in the label are neutralised
/// by wrapping each in a single-character bracket class, which `GLOB` matches
/// literally rather than as a wildcard.
fn gallery_label_glob_pattern(label: &str) -> Result<String> {
    let encoded =
        serde_json::to_string(label).context("failed to encode a gallery label filter")?;
    let mut pattern = String::with_capacity(encoded.len() + 2);
    pattern.push('*');
    for character in encoded.chars() {
        if matches!(character, '*' | '?' | '[') {
            pattern.push('[');
            pattern.push(character);
            pattern.push(']');
        } else {
            pattern.push(character);
        }
    }
    pattern.push('*');
    Ok(pattern)
}

/// Renders `filter` as SQL predicates plus their bind values.
///
/// Placeholders are numbered from `first_placeholder`, so a caller can append
/// the predicates to a statement that already binds parameters of its own.
pub(crate) fn gallery_label_predicates(
    filter: &GalleryLabelFilter,
    first_placeholder: usize,
) -> Result<(String, Vec<String>)> {
    let mut predicates = String::new();
    let mut values = Vec::new();
    let mut placeholder = first_placeholder;
    let mut push = |keyword: &str, label: &str, placeholder: usize| -> Result<String> {
        values.push(gallery_label_glob_pattern(label)?);
        Ok(format!(
            " AND {GALLERY_LABELS_COLUMN} {keyword} ?{placeholder}"
        ))
    };
    for label in &filter.required {
        predicates.push_str(&push("GLOB", label, placeholder)?);
        placeholder += 1;
    }
    for label in &filter.excluded {
        predicates.push_str(&push("NOT GLOB", label, placeholder)?);
        placeholder += 1;
    }
    Ok((predicates, values))
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
