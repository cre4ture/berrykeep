use crate::shared_transport_error::SharedTransportError;
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream::FuturesUnordered};
use reqwest::Url;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::watch;
use transport_sdk::{
    IrohRelayTicket, IrohRelayTicketCollection, IrohRelayTicketSet, RendezvousControlClient,
};

mod origin;
use origin::OriginBudget;

/// Keep two server-side lease slots available for ticket rotation and recovery.
pub(crate) const MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN: usize = 8;

#[derive(Clone)]
pub(crate) struct IrohRelayLeaseBudget {
    inner: Arc<IrohRelayLeaseBudgetInner>,
}

struct IrohRelayLeaseBudgetInner {
    origins: Mutex<HashMap<String, Arc<OriginBudget>>>,
    owners: Mutex<HashMap<String, Weak<IrohRelayLeaseHandle>>>,
    next_generation: AtomicU64,
    max_per_origin: usize,
}

struct LeaseOrigin {
    budget: Arc<OriginBudget>,
    client: RendezvousControlClient,
    request: Mutex<Option<TicketReceiver>>,
}

type SharedTicketResult = std::result::Result<IrohRelayTicket, SharedTransportError>;
type TicketReceiver = watch::Receiver<Option<SharedTicketResult>>;

pub(crate) struct IrohRelayLeaseHandle {
    endpoint_id: String,
    generation: u64,
    origins: Vec<LeaseOrigin>,
    retired: AtomicBool,
    revoked_origins: Mutex<BTreeSet<String>>,
    changes: watch::Sender<u64>,
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
                origins: Mutex::new(HashMap::new()),
                owners: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(0),
                max_per_origin,
            }),
        }
    }

    /// Creates a logical owner. Capacity is admitted independently at each origin,
    /// immediately before ticket issuance, including refresh and additional tickets.
    pub(crate) fn lease_for_endpoint(
        &self,
        rendezvous: RendezvousControlClient,
        endpoint_id: &str,
    ) -> Result<Arc<IrohRelayLeaseHandle>> {
        let endpoint_id = endpoint_id.trim();
        if endpoint_id.is_empty() {
            bail!("iroh relay lease endpoint ID must not be blank");
        }
        transport_sdk::IrohRelayTicketRequest {
            cluster_id: rendezvous.config().cluster_id,
            endpoint_id: endpoint_id.to_string(),
        }
        .validate()?;
        let mut owners = lock(&self.inner.owners);
        owners.retain(|_, owner| owner.strong_count() > 0);
        if let Some(owner) = owners.get(endpoint_id).and_then(Weak::upgrade)
            && owner.is_active()
        {
            return Ok(owner);
        }

        let mut clients = BTreeMap::new();
        for url in &rendezvous.config().rendezvous_urls {
            let origin = Url::parse(url)?.origin().ascii_serialization();
            clients
                .entry(origin)
                .or_insert(rendezvous.for_endpoint(url)?);
        }
        let mut budgets = lock(&self.inner.origins);
        let origins = clients
            .into_iter()
            .map(|(origin, client)| LeaseOrigin {
                budget: budgets
                    .entry(origin.clone())
                    .or_insert_with(|| {
                        Arc::new(OriginBudget::new(origin, self.inner.max_per_origin))
                    })
                    .clone(),
                client,
                request: Mutex::new(None),
            })
            .collect();
        let owner = Arc::new(IrohRelayLeaseHandle {
            endpoint_id: endpoint_id.to_string(),
            generation: self.inner.next_generation.fetch_add(1, Ordering::Relaxed) + 1,
            origins,
            retired: AtomicBool::new(false),
            revoked_origins: Mutex::new(BTreeSet::new()),
            changes: watch::channel(0).0,
        });
        owners.insert(endpoint_id.to_string(), Arc::downgrade(&owner));
        Ok(owner)
    }

    #[cfg(test)]
    fn active_per_origin(&self, origin: &str) -> usize {
        lock(&self.inner.origins)
            .get(origin)
            .map_or(0, |origin| origin.occupied())
    }
}

impl IrohRelayLeaseHandle {
    pub(crate) fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.retired.load(Ordering::Acquire)
            && lock(&self.revoked_origins).len() < self.origins.len()
    }

    pub(crate) fn authorizes_origin(&self, url: &str) -> bool {
        let Ok(url) = Url::parse(url) else {
            return false;
        };
        let origin = url.origin().ascii_serialization();
        !self.retired.load(Ordering::Acquire)
            && self.origins.iter().any(|item| item.budget.name == origin)
            && !lock(&self.revoked_origins).contains(&origin)
    }

    pub(crate) fn touch(&self) {
        for origin in &self.origins {
            origin.budget.touch(&self.endpoint_id, self.generation);
        }
    }

    pub(crate) fn ticket_set(&self) -> IrohRelayTicketSet {
        let mut tickets = IrohRelayTicketSet::default();
        for origin in &self.origins {
            if self.authorizes_origin(&origin.budget.name)
                && let Some(ticket) = origin.budget.ticket(&self.endpoint_id, self.generation)
            {
                tickets.insert(&ticket);
            }
        }
        tickets
    }

    pub(crate) async fn request_tickets(self: &Arc<Self>) -> Result<IrohRelayTicketCollection> {
        let requests = self
            .origins
            .iter()
            .map(|origin| {
                let url = origin.client.config().rendezvous_urls[0].clone();
                let mut receiver = self.ticket_request(origin);
                (url, async move {
                    loop {
                        if let Some(result) = receiver.borrow().clone() {
                            return result.map_err(anyhow::Error::from);
                        }
                        receiver
                            .changed()
                            .await
                            .context("iroh relay ticket worker ended without a result")?;
                    }
                })
            })
            .collect();
        IrohRelayTicketCollection::from_requests(requests).await
    }

    fn ticket_request(self: &Arc<Self>, origin: &LeaseOrigin) -> TicketReceiver {
        let mut active = lock(&origin.request);
        if let Some(receiver) = active.as_ref()
            && receiver.borrow().is_none()
            && receiver.has_changed().is_ok()
        {
            return receiver.clone();
        }
        let (sender, receiver) = watch::channel(None);
        *active = Some(receiver.clone());
        let client = origin.client.clone();
        let budget = origin.budget.clone();
        let owner = self.clone();
        // Shared ownership spans consumer cancellation, admission queues and late
        // HTTP responses. Only one worker per endpoint/origin may be in flight.
        tokio::spawn(async move {
            let result = budget
                .request_ticket(owner, client)
                .await
                .map_err(SharedTransportError::from);
            sender.send_replace(Some(result));
        });
        receiver
    }

    pub(crate) async fn release_now(&self, reason: &'static str) -> Result<()> {
        self.retire(reason);
        let mut releases = self
            .origins
            .iter()
            .map(|origin| {
                origin
                    .budget
                    .release_once(&self.endpoint_id, self.generation)
            })
            .collect::<FuturesUnordered<_>>();
        let mut last_error = None;
        while let Some(result) = releases.next().await {
            if let Err(error) = result {
                last_error = Some(error);
            }
        }
        last_error.map_or(Ok(()), Err)
    }

    fn revoke_origin(&self, origin: &str) {
        if lock(&self.revoked_origins).insert(origin.to_string()) {
            self.changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    pub(crate) fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn retire(&self, reason: &'static str) {
        if !self.retired.swap(true, Ordering::AcqRel) {
            self.changes
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
        for origin in &self.origins {
            origin
                .budget
                .retire(&self.endpoint_id, self.generation, reason);
        }
    }
}

impl Drop for IrohRelayLeaseHandle {
    fn drop(&mut self) {
        self.retire("last_handle_dropped");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
