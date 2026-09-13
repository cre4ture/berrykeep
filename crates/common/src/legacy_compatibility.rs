//! Temporary compatibility helpers for renamed BerryKeep environment settings.
//!
//! New callers must use the `BERRYKEEP_` setting family.  This module is the
//! one intentional place that recognizes the former prefix so deployed
//! services can upgrade without rewriting their environment files first.

use std::env::VarError;
use std::ffi::{OsStr, OsString};

/// Looks up a canonical environment variable, falling back to its legacy
/// spelling only when the canonical variable is absent.
pub fn var(key: impl AsRef<OsStr>) -> Result<String, VarError> {
    let key = key.as_ref();
    std::env::var(key).or_else(|error| match error {
        VarError::NotPresent => {
            legacy_environment_key(key).map_or(Err(VarError::NotPresent), std::env::var)
        }
        other => Err(other),
    })
}

/// `var_os` counterpart to [`var`].
pub fn var_os(key: impl AsRef<OsStr>) -> Option<OsString> {
    let key = key.as_ref();
    std::env::var_os(key).or_else(|| legacy_environment_key(key).and_then(std::env::var_os))
}

fn legacy_environment_key(key: &OsStr) -> Option<OsString> {
    let suffix = key.to_str()?.strip_prefix("BERRYKEEP_")?;
    Some(format!("IRONMESH_{suffix}").into())
}

#[cfg(test)]
mod tests {
    use super::legacy_environment_key;
    use std::ffi::OsStr;

    #[test]
    fn maps_only_canonical_brand_settings_to_a_legacy_fallback() {
        assert_eq!(
            legacy_environment_key(OsStr::new("BERRYKEEP_SERVER_BIND")),
            Some("IRONMESH_SERVER_BIND".into())
        );
        assert_eq!(legacy_environment_key(OsStr::new("RUST_LOG")), None);
    }
}
