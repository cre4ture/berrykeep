//! Server-side country-code derivation (doc Section 4.2).
//!
//! Per the design, the *only* location signal ever kept is a coarse ISO-3166-1 alpha-2 country
//! code, and it is derived **server-side from the request's source IP** — never self-reported by
//! the node. Crucially, the raw source IP is used only transiently to resolve the country and is
//! then discarded: it is never persisted, logged, or forwarded (Section 2.6 / 4.2).
//!
//! This crate ships a [`NoopCountryResolver`] (always `None`) as the default, so no GeoIP
//! database or extra dependency is pulled in unless a deployment opts in. When the
//! `bundled-country-db` Cargo feature is enabled, [`BundledCountryResolver`] is also available and
//! can be plugged in via [`crate::StatsCollectorAppState::with_country_resolver`] without any other
//! code change — the ingestion path already calls the trait and stores whatever it returns.

use std::net::IpAddr;

/// Resolves a coarse country code from a request's source IP. Implementations MUST NOT retain,
/// log, or forward the IP itself — only the returned country code may leave the call.
pub trait CountryResolver: Send + Sync {
    /// Returns an ISO-3166-1 alpha-2 country code (e.g. `"DE"`), or `None` when the country is
    /// unknown / cannot be determined.
    fn resolve(&self, source_ip: IpAddr) -> Option<String>;
}

/// Default resolver: always `None`. Keeps the collector free of any GeoIP dependency while leaving
/// the `country_code` column in place as a seam (doc Section 4.2).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCountryResolver;

impl CountryResolver for NoopCountryResolver {
    fn resolve(&self, _source_ip: IpAddr) -> Option<String> {
        None
    }
}

/// A real, offline IP-to-country resolver backed by the [`iptocc`] crate, available when this
/// crate is built with the `bundled-country-db` feature.
///
/// ## Why this data source
///
/// `iptocc` embeds a compact (~1.3 MB), statically included lookup table built from the five
/// Regional Internet Registries' (AFRINIC, APNIC, ARIN, LACNIC, RIPE NCC) publicly published
/// "delegated-extended" statistics files — the same joint RIR data set that projects like Tor's
/// GeoIP database and `ipdeny.com`'s country zone files are built from. These files map address
/// blocks to the country a RIR *registered* them to; per `iptocc`'s own docs this agrees with
/// MaxMind's data for ~95% of IPv4 space, which is more than adequate for a coarse, k-anonymized
/// "roughly where in the world" signal (doc Section 4.2) — this is not used for anything requiring
/// higher precision.
///
/// Crucially, unlike MaxMind GeoLite2 / DB-IP (which require creating an account and periodically
/// re-accepting an EULA to keep downloading updates), this data is redistributed by the RIRs
/// without any registration or license-key requirement, and the crate itself is dual
/// `MIT OR Apache-2.0` licensed. The table is embedded in the crate binary via `include_bytes!` —
/// no network access, no file I/O, no runtime dependency on an external service. Refreshing the
/// data is a matter of bumping the `iptocc` crate version (its maintainer republishes nightly).
///
/// `iptocc` is licensed `MIT OR Apache-2.0`; see <https://crates.io/crates/iptocc> and
/// <https://github.com/roniemartinez/IPToCC> for the crate, and
/// <https://www.nro.net/about/rirs/statistics/> for the underlying RIR statistics format this
/// data is generated from. Pin/verify the exact `iptocc` version in `Cargo.lock` when auditing
/// data provenance.
#[cfg(feature = "bundled-country-db")]
#[derive(Debug, Default, Clone, Copy)]
pub struct BundledCountryResolver;

#[cfg(feature = "bundled-country-db")]
impl CountryResolver for BundledCountryResolver {
    fn resolve(&self, source_ip: IpAddr) -> Option<String> {
        iptocc::country_code(source_ip).map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_resolver_never_resolves_a_country() {
        let resolver = NoopCountryResolver;
        assert_eq!(resolver.resolve("203.0.113.7".parse().unwrap()), None);
        assert_eq!(resolver.resolve("::1".parse().unwrap()), None);
    }

    #[cfg(feature = "bundled-country-db")]
    #[test]
    fn bundled_resolver_resolves_known_public_ips() {
        let resolver = BundledCountryResolver;
        // Google Public DNS: a stable, long-lived ARIN allocation registered to the US.
        assert_eq!(
            resolver.resolve("8.8.8.8".parse().unwrap()),
            Some("US".to_string())
        );
        // Cloudflare's 1.1.1.0/24 is an APNIC-delegated block registered to Australia — a good
        // reminder that this is the RIR *registration* country, not necessarily where a service
        // is operated from (see the module docs).
        assert_eq!(
            resolver.resolve("1.1.1.1".parse().unwrap()),
            Some("AU".to_string())
        );
    }

    #[cfg(feature = "bundled-country-db")]
    #[test]
    fn bundled_resolver_returns_none_for_unassigned_or_private_ranges() {
        let resolver = BundledCountryResolver;
        // RFC 1918 private range: not assigned to any country by a RIR.
        assert_eq!(resolver.resolve("10.0.0.1".parse().unwrap()), None);
    }
}
