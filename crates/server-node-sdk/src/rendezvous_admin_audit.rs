use serde_json::{Value, json};
use transport_sdk::RendezvousServiceConfig;

pub(super) fn request_details(
    urls: Option<&[String]>,
    services: Option<&[RendezvousServiceConfig]>,
) -> Value {
    // Denied requests are audited too; keep unvalidated strings out of storage.
    json!({
        "requested_url_count": urls.map(<[String]>::len),
        "requested_service_count": services.map(<[RendezvousServiceConfig]>::len),
        "requested_global_service_count": services.map(|entries| entries.iter().filter(|entry| entry.global_registration_enabled).count()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_request_metadata_has_bounded_size_even_for_large_urls() {
        let url = format!("https://example.com/{}", "untrusted-input".repeat(50_000));
        let urls = vec![url.clone()];
        let services = vec![RendezvousServiceConfig {
            url,
            global_registration_enabled: true,
        }];
        let details = request_details(Some(&urls), Some(&services));
        let encoded = serde_json::to_string(&details).unwrap();
        assert!(
            encoded.len() < 256,
            "pre-authorization audit metadata must not copy untrusted request strings"
        );
        assert!(!encoded.contains("untrusted-input"));
    }
}
