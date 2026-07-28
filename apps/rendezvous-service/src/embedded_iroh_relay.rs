use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use iroh_relay::server::{
    Access, AccessControl, ClientRateLimit, ClientRequest, RelayConfig, Server, ServerConfig,
};
use subtle::ConstantTimeEq;

use crate::config::EmbeddedIrohRelayConfig;

#[derive(Clone)]
struct StaticTokenAccess {
    token: String,
}

impl std::fmt::Debug for StaticTokenAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StaticTokenAccess")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl AccessControl for StaticTokenAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if request
            .auth_token()
            .as_deref()
            .is_some_and(|provided| constant_time_token_eq(provided, &self.token))
        {
            Access::Allow
        } else {
            Access::Deny {
                reason: Some("relay authorization failed".to_string()),
            }
        }
    }
}

pub async fn spawn(config: &EmbeddedIrohRelayConfig) -> Result<Server> {
    let bytes_per_second = NonZeroU32::new(config.client_rx_bytes_per_second)
        .expect("validated relay byte rate should be non-zero");
    let max_burst_bytes = NonZeroU32::new(config.client_rx_max_burst_bytes)
        .expect("validated relay burst size should be non-zero");
    let mut client_rate_limit = ClientRateLimit::new(bytes_per_second);
    client_rate_limit.max_burst_bytes = Some(max_burst_bytes);

    let mut relay = RelayConfig::new(config.bind_addr);
    relay.limits.client_rx = Some(client_rate_limit);
    relay.access = Arc::new(StaticTokenAccess {
        token: config.auth_token.clone(),
    });

    let mut server = ServerConfig::default();
    server.relay = Some(relay);
    Server::spawn(server)
        .await
        .map_err(|error| anyhow!("failed starting embedded iroh relay: {error}"))
}

fn constant_time_token_eq(provided: &str, expected: &str) -> bool {
    provided.len() == expected.len() && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Version};
    use iroh::SecretKey;
    use iroh_relay::http::ProtocolVersion;

    #[tokio::test]
    async fn static_token_access_rejects_missing_or_incorrect_tokens() {
        let access = StaticTokenAccess {
            token: "correct-token-with-at-least-32-characters".to_string(),
        };

        assert!(matches!(
            access.on_connect(&request_with_token(None)).await,
            Access::Deny { .. }
        ));
        assert!(matches!(
            access
                .on_connect(&request_with_token(Some("incorrect")))
                .await,
            Access::Deny { .. }
        ));
        assert_eq!(
            access
                .on_connect(&request_with_token(Some(
                    "correct-token-with-at-least-32-characters"
                )))
                .await,
            Access::Allow
        );
    }

    fn request_with_token(token: Option<&str>) -> ClientRequest {
        let mut request = Request::builder().uri("/relay").version(Version::HTTP_11);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let (parts, ()) = request.body(()).expect("request should build").into_parts();
        ClientRequest::new(SecretKey::generate().public(), ProtocolVersion::V1, parts)
    }
}
