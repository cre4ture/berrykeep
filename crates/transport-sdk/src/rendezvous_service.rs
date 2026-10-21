use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Registration policy for one rendezvous service. URL-only configurations
/// remain supported; an explicit service entry takes precedence over defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RendezvousServiceConfig {
    pub url: String,
    #[serde(default)]
    pub global_registration_enabled: bool,
}

pub fn resolve_rendezvous_services(
    urls: &[String],
    services: &[RendezvousServiceConfig],
    legacy_global_registration_enabled: bool,
) -> Result<Vec<RendezvousServiceConfig>> {
    let mut explicit = HashMap::new();
    let mut normalized_services = Vec::new();
    for service in services {
        let url = normalize_url(&service.url)?;
        if let Some(previous) = explicit.insert(url.clone(), service.global_registration_enabled)
            && previous != service.global_registration_enabled
        {
            bail!("conflicting global registration settings for rendezvous URL {url}");
        }
        normalized_services.push(url);
    }

    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for url in urls
        .iter()
        .filter(|url| !url.trim().is_empty())
        .map(|url| normalize_url(url))
        .chain(normalized_services.into_iter().map(Ok))
    {
        let url = url?;
        if !seen.insert(url.clone()) {
            continue;
        }
        let global_registration_enabled = explicit
            .get(&url)
            .copied()
            .unwrap_or(legacy_global_registration_enabled);
        if global_registration_enabled && !url.starts_with("https://") {
            bail!("global rendezvous registration requires an HTTPS URL: {url}");
        }
        resolved.push(RendezvousServiceConfig {
            url,
            global_registration_enabled,
        });
    }
    Ok(resolved)
}

fn normalize_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("invalid rendezvous service URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("rendezvous service URL must use HTTP or HTTPS and include a host");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(url: &str, global: bool) -> RendezvousServiceConfig {
        RendezvousServiceConfig {
            url: url.into(),
            global_registration_enabled: global,
        }
    }

    #[test]
    fn explicit_policy_overrides_legacy_global_switch_per_url() {
        let services = resolve_rendezvous_services(
            &[
                "https://private.example".into(),
                "https://global.example".into(),
            ],
            &[service("https://private.example/", false)],
            true,
        )
        .unwrap();
        assert_eq!(
            services,
            vec![
                service("https://private.example/", false),
                service("https://global.example/", true)
            ]
        );
    }

    #[test]
    fn old_urls_default_to_private_and_explicit_entries_extend_the_list() {
        let services = resolve_rendezvous_services(
            &[
                " https://private.example ".into(),
                "https://private.example/".into(),
            ],
            &[service("https://global.example", true)],
            false,
        )
        .unwrap();
        assert_eq!(
            services,
            vec![
                service("https://private.example/", false),
                service("https://global.example/", true)
            ]
        );
        assert!(
            !serde_json::from_str::<RendezvousServiceConfig>(r#"{"url":"https://old.example"}"#)
                .unwrap()
                .global_registration_enabled
        );
    }

    #[test]
    fn conflicting_duplicate_policies_are_rejected() {
        assert!(
            resolve_rendezvous_services(
                &[],
                &[
                    service("https://same.example", false),
                    service("https://same.example/", true)
                ],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn global_services_require_https() {
        assert!(
            resolve_rendezvous_services(&[], &[service("http://global.example", true)], false)
                .is_err()
        );
    }
}
