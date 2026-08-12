use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Leaves roughly half of a 1024-descriptor process limit available for the
/// listener, outbound connections, files, and short-lived accepted sockets.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;
pub const DEFAULT_MAX_TLS_HANDSHAKES: usize = 64;
pub const DEFAULT_MAX_RELAY_TICKETS_PER_CLIENT: usize = 10;
pub const DEFAULT_MAX_RELAY_TICKET_ISSUES_PER_MINUTE: usize = 10;

const ACCEPT_ERROR_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendezvousAdmissionConfig {
    pub max_connections: usize,
    pub max_tls_handshakes: usize,
    pub max_relay_tickets_per_client: usize,
    pub max_relay_ticket_issues_per_minute: usize,
}

impl Default for RendezvousAdmissionConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_tls_handshakes: DEFAULT_MAX_TLS_HANDSHAKES,
            max_relay_tickets_per_client: DEFAULT_MAX_RELAY_TICKETS_PER_CLIENT,
            max_relay_ticket_issues_per_minute: DEFAULT_MAX_RELAY_TICKET_ISSUES_PER_MINUTE,
        }
    }
}

impl RendezvousAdmissionConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_connections == 0 {
            bail!("rendezvous maximum connections must be greater than zero");
        }
        if self.max_tls_handshakes == 0 {
            bail!("rendezvous maximum concurrent TLS handshakes must be greater than zero");
        }
        if self.max_tls_handshakes > self.max_connections {
            bail!(
                "rendezvous maximum concurrent TLS handshakes must not exceed maximum connections"
            );
        }
        if self.max_relay_tickets_per_client == 0 {
            bail!("rendezvous maximum relay tickets per client must be greater than zero");
        }
        if self.max_relay_ticket_issues_per_minute == 0 {
            bail!("rendezvous relay ticket issue rate must be greater than zero");
        }
        if self.max_connections > Semaphore::MAX_PERMITS {
            bail!("rendezvous maximum connections exceeds the supported semaphore capacity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmissionLimitReached {
    pub active: usize,
    pub limit: usize,
    pub events_total: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RendezvousAdmissionController {
    config: RendezvousAdmissionConfig,
    connections: Arc<Semaphore>,
    tls_handshakes: Arc<Semaphore>,
    connection_saturation_events: Arc<AtomicU64>,
    rejected_tls_handshakes: Arc<AtomicU64>,
}

impl RendezvousAdmissionController {
    pub(crate) fn new(config: RendezvousAdmissionConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            connections: Arc::new(Semaphore::new(config.max_connections)),
            tls_handshakes: Arc::new(Semaphore::new(config.max_tls_handshakes)),
            connection_saturation_events: Arc::new(AtomicU64::new(0)),
            rejected_tls_handshakes: Arc::new(AtomicU64::new(0)),
            config,
        })
    }

    pub(crate) fn config(&self) -> RendezvousAdmissionConfig {
        self.config
    }

    pub(crate) fn try_admit_connection(
        &self,
    ) -> std::result::Result<OwnedSemaphorePermit, AdmissionLimitReached> {
        self.connections.clone().try_acquire_owned().map_err(|_| {
            let events_total = self
                .connection_saturation_events
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            AdmissionLimitReached {
                active: self
                    .config
                    .max_connections
                    .saturating_sub(self.connections.available_permits()),
                limit: self.config.max_connections,
                events_total,
            }
        })
    }

    pub(crate) async fn admit_connection(&self) -> OwnedSemaphorePermit {
        self.connections
            .clone()
            .acquire_owned()
            .await
            .expect("rendezvous connection semaphore is never closed")
    }

    pub(crate) fn try_admit_tls_handshake(
        &self,
    ) -> std::result::Result<OwnedSemaphorePermit, AdmissionLimitReached> {
        self.tls_handshakes
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                let events_total = self.rejected_tls_handshakes.fetch_add(1, Ordering::Relaxed) + 1;
                AdmissionLimitReached {
                    active: self
                        .config
                        .max_tls_handshakes
                        .saturating_sub(self.tls_handshakes.available_permits()),
                    limit: self.config.max_tls_handshakes,
                    events_total,
                }
            })
    }
}

pub(crate) fn should_log_admission_event(events_total: u64) -> bool {
    events_total.is_power_of_two()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptErrorClass {
    Interrupted,
    DescriptorExhaustion,
    Other,
}

pub(crate) fn classify_accept_error(error: &io::Error) -> AcceptErrorClass {
    if error.kind() == io::ErrorKind::Interrupted {
        return AcceptErrorClass::Interrupted;
    }
    #[cfg(unix)]
    if matches!(error.raw_os_error(), Some(23 | 24)) {
        return AcceptErrorClass::DescriptorExhaustion;
    }
    AcceptErrorClass::Other
}

#[derive(Debug)]
pub(crate) struct AcceptErrorBackoff {
    next_delay: Duration,
}

impl Default for AcceptErrorBackoff {
    fn default() -> Self {
        Self {
            next_delay: ACCEPT_ERROR_INITIAL_BACKOFF,
        }
    }
}

impl AcceptErrorBackoff {
    pub(crate) fn delay_after(&mut self, error: &io::Error) -> Option<Duration> {
        if classify_accept_error(error) == AcceptErrorClass::Interrupted {
            return None;
        }
        let delay = self.next_delay;
        self.next_delay = self
            .next_delay
            .saturating_mul(2)
            .min(ACCEPT_ERROR_MAX_BACKOFF);
        Some(delay)
    }

    pub(crate) fn reset(&mut self) {
        self.next_delay = ACCEPT_ERROR_INITIAL_BACKOFF;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_config_rejects_invalid_limits() {
        assert!(
            RendezvousAdmissionConfig {
                max_connections: 0,
                max_tls_handshakes: 1,
                ..RendezvousAdmissionConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RendezvousAdmissionConfig {
                max_connections: 10,
                max_tls_handshakes: 0,
                ..RendezvousAdmissionConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RendezvousAdmissionConfig {
                max_connections: 10,
                max_tls_handshakes: 11,
                ..RendezvousAdmissionConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RendezvousAdmissionConfig {
                max_relay_tickets_per_client: 0,
                ..RendezvousAdmissionConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RendezvousAdmissionConfig {
                max_relay_ticket_issues_per_minute: 0,
                ..RendezvousAdmissionConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn connection_and_handshake_limits_are_independent() {
        let admission = RendezvousAdmissionController::new(RendezvousAdmissionConfig {
            max_connections: 2,
            max_tls_handshakes: 1,
            ..RendezvousAdmissionConfig::default()
        })
        .expect("admission config should be valid");

        let connection_one = admission
            .try_admit_connection()
            .expect("first connection should be admitted");
        let _connection_two = admission
            .try_admit_connection()
            .expect("second connection should be admitted");
        let connection_saturation = admission
            .try_admit_connection()
            .expect_err("third connection should reach the limit");
        assert_eq!(connection_saturation.active, 2);
        assert_eq!(connection_saturation.limit, 2);

        let handshake = admission
            .try_admit_tls_handshake()
            .expect("first handshake should be admitted");
        assert!(admission.try_admit_tls_handshake().is_err());
        drop(handshake);
        let _handshake = admission
            .try_admit_tls_handshake()
            .expect("released handshake permit should be reusable");

        drop(connection_one);
        let _connection = admission
            .try_admit_connection()
            .expect("released connection permit should be reusable");
    }

    #[tokio::test]
    async fn connection_backpressure_waits_for_a_full_connection_permit() {
        let admission = RendezvousAdmissionController::new(RendezvousAdmissionConfig {
            max_connections: 1,
            max_tls_handshakes: 1,
            ..RendezvousAdmissionConfig::default()
        })
        .expect("admission config should be valid");
        let active_connection = admission
            .try_admit_connection()
            .expect("first connection should be admitted");
        let waiting_admission = admission.clone();
        let waiter = tokio::spawn(async move { waiting_admission.admit_connection().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        drop(active_connection);
        let _next_connection = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("connection waiter should resume after a permit is released")
            .expect("connection waiter task should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_exhaustion_is_classified_for_unix_error_codes() {
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(23)),
            AcceptErrorClass::DescriptorExhaustion
        );
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(24)),
            AcceptErrorClass::DescriptorExhaustion
        );
    }

    #[test]
    fn accept_error_backoff_is_bounded_and_resets() {
        let error = io::Error::from_raw_os_error(24);
        let mut backoff = AcceptErrorBackoff::default();
        assert_eq!(
            backoff.delay_after(&error),
            Some(ACCEPT_ERROR_INITIAL_BACKOFF)
        );
        let mut last = Duration::ZERO;
        for _ in 0..20 {
            last = backoff.delay_after(&error).expect("error should back off");
        }
        assert_eq!(last, ACCEPT_ERROR_MAX_BACKOFF);
        backoff.reset();
        assert_eq!(
            backoff.delay_after(&error),
            Some(ACCEPT_ERROR_INITIAL_BACKOFF)
        );
    }
}
