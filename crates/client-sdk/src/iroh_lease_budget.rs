use anyhow::{Context, Result, anyhow};
use reqwest::Url;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Mutex as AsyncMutex;
use transport_sdk::RendezvousControlClient;

/// Keep two server-side lease slots available for ticket rotation and recovery.
pub(crate) const MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN: usize = 8;

#[derive(Clone)]
pub(crate) struct IrohRelayLeaseBudget {
    inner: Arc<IrohRelayLeaseBudgetInner>,
}

struct IrohRelayLeaseBudgetInner {
    admission: AsyncMutex<()>,
    state: Mutex<IrohRelayLeaseBudgetState>,
    next_generation: AtomicU64,
    max_per_origin: usize,
}

#[derive(Default)]
struct IrohRelayLeaseBudgetState {
    entries: HashMap<String, IrohRelayLeaseEntry>,
    next_access: u64,
}

struct IrohRelayLeaseEntry {
    generation: u64,
    origins: BTreeSet<String>,
    last_access: u64,
    lease: Arc<IrohRelayLeaseHandle>,
}

pub(crate) struct IrohRelayLeaseHandle {
    budget: Weak<IrohRelayLeaseBudgetInner>,
    rendezvous: RendezvousControlClient,
    endpoint_id: String,
    generation: u64,
    active: AtomicBool,
    release_started: AtomicBool,
}

impl Default for IrohRelayLeaseBudget {
    fn default() -> Self {
        Self::with_limit(MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN)
    }
}

impl IrohRelayLeaseBudget {
    fn with_limit(max_per_origin: usize) -> Self {
        assert!(
            max_per_origin > 0,
            "iroh relay lease budget must be positive"
        );
        Self {
            inner: Arc::new(IrohRelayLeaseBudgetInner {
                admission: AsyncMutex::new(()),
                state: Mutex::new(IrohRelayLeaseBudgetState::default()),
                next_generation: AtomicU64::new(0),
                max_per_origin,
            }),
        }
    }

    /// Reserves capacity before a new endpoint-bound ticket is requested.
    /// Any conflicting LRU leases are revoked and released before the new
    /// reservation becomes visible to callers.
    pub(crate) async fn reserve(
        &self,
        rendezvous: RendezvousControlClient,
        endpoint_id: &str,
    ) -> Result<Arc<IrohRelayLeaseHandle>> {
        let endpoint_id = endpoint_id.trim();
        if endpoint_id.is_empty() {
            return Err(anyhow!("iroh relay lease endpoint ID must not be blank"));
        }
        let origins = rendezvous_origins(&rendezvous)?;
        let _admission = self.inner.admission.lock().await;

        if let Some(existing) = self.existing_lease(endpoint_id) {
            if existing.is_active() {
                existing.touch();
                return Ok(existing);
            }
            existing
                .release_without_admission("retry_failed_release")
                .await
                .with_context(|| {
                    format!(
                        "failed releasing inactive iroh relay lease {endpoint_id} before reserving it again"
                    )
                })?;
            remove_matching_entry(&self.inner.state, endpoint_id, existing.generation);
        }

        loop {
            let evicted = {
                let state = lock_state(&self.inner.state);
                select_lru_eviction(&state, &origins, self.inner.max_per_origin, endpoint_id)?
            };
            let Some(lease) = evicted else {
                break;
            };
            let evicted_endpoint_id = lease.endpoint_id.clone();
            lease
                .release_without_admission("lru_eviction")
                .await
                .with_context(|| {
                    format!(
                        "failed releasing LRU iroh relay lease {evicted_endpoint_id} before reserving {endpoint_id}"
                    )
                })?;
            remove_matching_entry(&self.inner.state, &lease.endpoint_id, lease.generation);
        }

        let generation = self
            .inner
            .next_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let lease = Arc::new(IrohRelayLeaseHandle {
            budget: Arc::downgrade(&self.inner),
            rendezvous,
            endpoint_id: endpoint_id.to_string(),
            generation,
            active: AtomicBool::new(true),
            release_started: AtomicBool::new(false),
        });
        let mut state = lock_state(&self.inner.state);
        let last_access = next_access(&mut state);
        state.entries.insert(
            endpoint_id.to_string(),
            IrohRelayLeaseEntry {
                generation,
                origins,
                last_access,
                lease: lease.clone(),
            },
        );
        Ok(lease)
    }

    fn existing_lease(&self, endpoint_id: &str) -> Option<Arc<IrohRelayLeaseHandle>> {
        lock_state(&self.inner.state)
            .entries
            .get(endpoint_id)
            .map(|entry| entry.lease.clone())
    }

    #[cfg(test)]
    fn active_per_origin(&self, origin: &str) -> usize {
        lock_state(&self.inner.state)
            .entries
            .values()
            .filter(|entry| entry.origins.contains(origin))
            .count()
    }
}

impl IrohRelayLeaseHandle {
    pub(crate) fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn touch(&self) {
        if !self.is_active() {
            return;
        }
        let Some(budget) = self.budget.upgrade() else {
            return;
        };
        let mut state = lock_state(&budget.state);
        let last_access = next_access(&mut state);
        if let Some(entry) = state.entries.get_mut(&self.endpoint_id)
            && entry.generation == self.generation
        {
            entry.last_access = last_access;
        }
    }

    pub(crate) async fn release_now(&self, reason: &'static str) -> Result<()> {
        let Some(budget) = self.budget.upgrade() else {
            return self.release_server_once(reason).await;
        };
        let _admission = budget.admission.lock().await;
        self.release_server_once(reason).await?;
        remove_matching_entry(&budget.state, &self.endpoint_id, self.generation);
        Ok(())
    }

    async fn release_without_admission(&self, reason: &'static str) -> Result<()> {
        self.release_server_once(reason).await
    }

    async fn release_server_once(&self, reason: &'static str) -> Result<()> {
        self.active.store(false, Ordering::Release);
        if self.release_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let release_result = self
            .rendezvous
            .release_iroh_relay_ticket(&self.endpoint_id)
            .await
            .with_context(|| {
                format!(
                    "failed releasing endpoint-bound iroh relay lease {}",
                    self.endpoint_id
                )
            });
        let released = match release_result {
            Ok(released) => released,
            Err(error) => {
                self.release_started.store(false, Ordering::Release);
                return Err(error);
            }
        };
        tracing::info!(
            event = "iroh_relay_lease_released",
            endpoint_id = %self.endpoint_id,
            released,
            reason,
            "released client-budgeted endpoint-bound iroh relay lease"
        );
        Ok(())
    }
}

impl Drop for IrohRelayLeaseHandle {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if self.release_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let endpoint_id = self.endpoint_id.clone();
        let generation = self.generation;
        let rendezvous = self.rendezvous.clone();
        let budget = self.budget.upgrade();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                %endpoint_id,
                "cannot release endpoint-bound iroh relay lease outside a Tokio runtime"
            );
            return;
        };
        runtime.spawn(async move {
            if let Some(budget) = budget {
                let _admission = budget.admission.lock().await;
                match rendezvous.release_iroh_relay_ticket(&endpoint_id).await {
                    Ok(released) => {
                        remove_matching_entry(&budget.state, &endpoint_id, generation);
                        tracing::info!(
                            event = "iroh_relay_lease_released",
                            %endpoint_id,
                            released,
                            reason = "last_handle_dropped",
                            "released client-budgeted endpoint-bound iroh relay lease"
                        );
                    }
                    Err(error) => tracing::warn!(
                        %endpoint_id,
                        %error,
                        "failed releasing dropped endpoint-bound iroh relay lease"
                    ),
                }
                return;
            }
            match rendezvous.release_iroh_relay_ticket(&endpoint_id).await {
                Ok(released) => tracing::info!(
                    event = "iroh_relay_lease_released",
                    %endpoint_id,
                    released,
                    reason = "last_handle_dropped",
                    "released client-budgeted endpoint-bound iroh relay lease"
                ),
                Err(error) => tracing::warn!(
                    %endpoint_id,
                    %error,
                    "failed releasing dropped endpoint-bound iroh relay lease"
                ),
            }
        });
    }
}

fn rendezvous_origins(rendezvous: &RendezvousControlClient) -> Result<BTreeSet<String>> {
    rendezvous
        .config()
        .rendezvous_urls
        .iter()
        .map(|value| {
            Url::parse(value.trim())
                .map(|url| url.origin().ascii_serialization())
                .with_context(|| format!("invalid rendezvous URL {value:?}"))
        })
        .collect()
}

fn select_lru_eviction(
    state: &IrohRelayLeaseBudgetState,
    requested_origins: &BTreeSet<String>,
    max_per_origin: usize,
    endpoint_id: &str,
) -> Result<Option<Arc<IrohRelayLeaseHandle>>> {
    let saturated_origins = requested_origins
        .iter()
        .filter(|origin| {
            state
                .entries
                .values()
                .filter(|entry| entry.origins.contains(*origin))
                .count()
                >= max_per_origin
        })
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if saturated_origins.is_empty() {
        return Ok(None);
    }
    state
        .entries
        .iter()
        .filter(|(candidate_id, entry)| {
            candidate_id.as_str() != endpoint_id
                && entry
                    .origins
                    .iter()
                    .any(|origin| saturated_origins.contains(origin.as_str()))
        })
        .min_by_key(|(_, entry)| entry.last_access)
        .map(|(_, entry)| Some(entry.lease.clone()))
        .ok_or_else(|| anyhow!("iroh relay lease budget is full without an evictable lease"))
}

fn remove_matching_entry(
    state: &Mutex<IrohRelayLeaseBudgetState>,
    endpoint_id: &str,
    generation: u64,
) {
    let mut state = lock_state(state);
    if state
        .entries
        .get(endpoint_id)
        .is_some_and(|entry| entry.generation == generation)
    {
        state.entries.remove(endpoint_id);
    }
}

fn next_access(state: &mut IrohRelayLeaseBudgetState) -> u64 {
    state.next_access = state.next_access.saturating_add(1);
    state.next_access
}

fn lock_state(
    state: &Mutex<IrohRelayLeaseBudgetState>,
) -> std::sync::MutexGuard<'_, IrohRelayLeaseBudgetState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::delete};
    use std::sync::atomic::AtomicUsize;
    use transport_sdk::{
        IrohRelayTicketReleaseRequest, IrohRelayTicketReleaseResponse, RendezvousClientConfig,
    };

    async fn release_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let released = Arc::new(Mutex::new(Vec::new()));
        let released_for_handler = Arc::clone(&released);
        let router = Router::new().route(
            "/control/iroh-relay/ticket/release",
            delete(move |Json(request): Json<IrohRelayTicketReleaseRequest>| {
                let released = Arc::clone(&released_for_handler);
                async move {
                    released
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(request.endpoint_id);
                    Json(IrohRelayTicketReleaseResponse { released: true })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("release listener should bind");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("release listener address")
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("release server should run");
        });
        (base_url, released, server)
    }

    fn rendezvous(base_url: String) -> RendezvousControlClient {
        rendezvous_urls(vec![base_url])
    }

    fn rendezvous_urls(base_urls: Vec<String>) -> RendezvousControlClient {
        RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id: uuid::Uuid::now_v7(),
                rendezvous_urls: base_urls,
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build")
    }

    #[tokio::test]
    async fn ninth_reservation_releases_lru_before_becoming_active() {
        let (base_url, released, server) = release_server().await;
        let origin = Url::parse(&base_url)
            .expect("release URL should parse")
            .origin()
            .ascii_serialization();
        let budget = IrohRelayLeaseBudget::default();
        let rendezvous = rendezvous(base_url);
        let endpoint_ids = (0..=MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN)
            .map(|_| iroh::SecretKey::generate().public().to_string())
            .collect::<Vec<_>>();
        let mut leases = Vec::new();
        for endpoint_id in endpoint_ids
            .iter()
            .take(MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN)
        {
            leases.push(
                budget
                    .reserve(rendezvous.clone(), endpoint_id)
                    .await
                    .expect("lease within the budget should reserve"),
            );
        }
        leases[1].touch();
        let ninth = budget
            .reserve(
                rendezvous,
                &endpoint_ids[MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN],
            )
            .await
            .expect("ninth lease should evict the LRU lease");

        assert!(!leases[0].is_active());
        assert!(leases[1].is_active());
        assert!(ninth.is_active());
        assert_eq!(
            budget.active_per_origin(&origin),
            MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN
        );
        assert_eq!(
            *released
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![endpoint_ids[0].clone()]
        );

        for lease in leases {
            lease.release_started.store(true, Ordering::Release);
        }
        ninth.release_started.store(true, Ordering::Release);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn explicit_release_is_idempotent_and_frees_capacity() {
        let (base_url, released, server) = release_server().await;
        let budget = IrohRelayLeaseBudget::with_limit(1);
        let endpoint_id = iroh::SecretKey::generate().public().to_string();
        let lease = budget
            .reserve(rendezvous(base_url), &endpoint_id)
            .await
            .expect("lease should reserve");
        lease
            .release_now("test")
            .await
            .expect("lease release should succeed");
        lease
            .release_now("test")
            .await
            .expect("repeated release should be a no-op");
        assert_eq!(
            released
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
        assert!(!lease.is_active());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn failed_lru_release_keeps_capacity_reserved_until_retry_succeeds() {
        let release_attempts = Arc::new(AtomicUsize::new(0));
        let release_attempts_for_handler = Arc::clone(&release_attempts);
        let router = Router::new().route(
            "/control/iroh-relay/ticket/release",
            delete(move |Json(_request): Json<IrohRelayTicketReleaseRequest>| {
                let release_attempts = Arc::clone(&release_attempts_for_handler);
                async move {
                    if release_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (StatusCode::INTERNAL_SERVER_ERROR, "release failed")
                            .into_response();
                    }
                    Json(IrohRelayTicketReleaseResponse { released: true }).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("release listener should bind");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("release listener address")
        );
        let origin = Url::parse(&base_url)
            .expect("release URL should parse")
            .origin()
            .ascii_serialization();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("release server should run");
        });

        let budget = IrohRelayLeaseBudget::with_limit(1);
        let rendezvous = rendezvous(base_url);
        let first_endpoint_id = iroh::SecretKey::generate().public().to_string();
        let second_endpoint_id = iroh::SecretKey::generate().public().to_string();
        let first = budget
            .reserve(rendezvous.clone(), &first_endpoint_id)
            .await
            .expect("first lease should reserve");

        let release_error = match budget
            .reserve(rendezvous.clone(), &second_endpoint_id)
            .await
        {
            Ok(_) => panic!("a failed LRU release must reject the new reservation"),
            Err(error) => error,
        };
        assert!(release_error.to_string().contains("failed releasing LRU"));
        assert!(!first.is_active());
        assert_eq!(budget.active_per_origin(&origin), 1);
        assert_eq!(release_attempts.load(Ordering::SeqCst), 1);

        let second = budget
            .reserve(rendezvous, &second_endpoint_id)
            .await
            .expect("the next reservation should retry and complete the LRU release");
        assert!(second.is_active());
        assert_eq!(budget.active_per_origin(&origin), 1);
        assert_eq!(release_attempts.load(Ordering::SeqCst), 2);

        first.release_started.store(true, Ordering::Release);
        second.release_started.store(true, Ordering::Release);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn lru_eviction_only_targets_origins_that_are_at_capacity() {
        let (origin_a_url, origin_a_releases, origin_a_server) = release_server().await;
        let (origin_b_url, origin_b_releases, origin_b_server) = release_server().await;
        let budget = IrohRelayLeaseBudget::with_limit(2);
        let b_only_endpoint = iroh::SecretKey::generate().public().to_string();
        let first_a_endpoint = iroh::SecretKey::generate().public().to_string();
        let second_a_endpoint = iroh::SecretKey::generate().public().to_string();
        let combined_endpoint = iroh::SecretKey::generate().public().to_string();

        let b_only = budget
            .reserve(rendezvous(origin_b_url.clone()), &b_only_endpoint)
            .await
            .expect("B-only lease should reserve first");
        let first_a = budget
            .reserve(rendezvous(origin_a_url.clone()), &first_a_endpoint)
            .await
            .expect("first A lease should reserve");
        let second_a = budget
            .reserve(rendezvous(origin_a_url.clone()), &second_a_endpoint)
            .await
            .expect("second A lease should fill origin A");
        let combined = budget
            .reserve(
                rendezvous_urls(vec![origin_a_url, origin_b_url]),
                &combined_endpoint,
            )
            .await
            .expect("combined lease should evict from saturated origin A");

        assert!(b_only.is_active());
        assert!(!first_a.is_active());
        assert!(second_a.is_active());
        assert!(combined.is_active());
        assert_eq!(
            *origin_a_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![first_a_endpoint]
        );
        assert!(
            origin_b_releases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );

        for lease in [b_only, first_a, second_a, combined] {
            lease.release_started.store(true, Ordering::Release);
        }
        origin_a_server.abort();
        let _ = origin_a_server.await;
        origin_b_server.abort();
        let _ = origin_b_server.await;
    }
}
