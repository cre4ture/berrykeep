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

/// Keep cached cluster contacts outside the legacy node-wide global default.
/// Hints classify existing targets; explicit per-service policies still win.
pub fn resolve_rendezvous_services_with_contact_hints(
    urls: &[String],
    services: &[RendezvousServiceConfig],
    contact_urls: &[String],
    legacy_global_registration_enabled: bool,
) -> Result<Vec<RendezvousServiceConfig>> {
    let explicit_urls = services
        .iter()
        .map(|service| normalize_url(&service.url))
        .collect::<Result<HashSet<_>>>()?;
    let mut known_urls = urls
        .iter()
        .filter(|url| !url.trim().is_empty())
        .map(|url| normalize_url(url))
        .collect::<Result<HashSet<_>>>()?;
    known_urls.extend(explicit_urls.iter().cloned());
    let mut policies = services.to_vec();
    let mut seen_hints = HashSet::new();
    for hint in contact_urls {
        let url = normalize_url(hint)?;
        if !known_urls.contains(&url) {
            bail!("rendezvous contact hint is not a configured rendezvous URL: {url}");
        }
        if !explicit_urls.contains(&url) && seen_hints.insert(url.clone()) {
            policies.push(RendezvousServiceConfig {
                url,
                global_registration_enabled: false,
            });
        }
    }
    resolve_rendezvous_services(urls, &policies, legacy_global_registration_enabled)
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
    #[test]
    fn contact_hints_do_not_inherit_the_legacy_global_default() {
        let urls: Vec<String> = vec![
            "https://owned.example/".into(),
            "http://private.example/".into(),
            "https://override.example/".into(),
        ];
        let hints = vec![urls[1].clone(), urls[2].clone()];
        for global in [false, true] {
            let resolved = resolve_rendezvous_services_with_contact_hints(
                &urls,
                &[service(&urls[2], global)],
                &hints,
                true,
            )
            .unwrap();
            assert_eq!(
                resolved,
                vec![
                    service(&urls[0], true),
                    service(&urls[1], false),
                    service(&urls[2], global)
                ]
            );
        }
    }

    #[test]
    fn contact_hint_metadata_cannot_introduce_an_unlisted_target() {
        assert!(
            resolve_rendezvous_services_with_contact_hints(
                &["https://known.example/".into()],
                &[],
                &["https://unlisted.example/".into()],
                true
            )
            .is_err()
        );
    }
}
