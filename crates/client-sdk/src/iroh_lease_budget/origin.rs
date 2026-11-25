use super::{IrohRelayLeaseHandle, lock};
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use transport_sdk::{IrohRelayTicket, RendezvousControlClient};

// Match the existing ticket clock contract, conservatively retaining expired
// tickets for another minute. Unknown issuance results never expire locally.
const EXPIRY_GRACE_SECS: u64 = 60;

pub(super) struct OriginBudget {
    pub(super) name: String,
    admission: AsyncMutex<()>,
    state: Mutex<OriginState>,
    limit: usize,
}

#[derive(Default)]
struct OriginState {
    entries: HashMap<String, Entry>,
    next_access: u64,
}

struct Entry {
    generation: u64,
    owner: Weak<IrohRelayLeaseHandle>,
    client: RendezvousControlClient,
    last_access: u64,
    expires_at: Option<Instant>,
    ticket: Option<IrohRelayTicket>,
    release_pending: bool,
    cleanup_scheduled: bool,
    release_failures: u32,
    retry_at: Option<Instant>,
}

impl OriginBudget {
    pub(super) fn new(name: String, limit: usize) -> Self {
        Self {
            name,
            admission: AsyncMutex::new(()),
            state: Mutex::new(OriginState::default()),
            limit,
        }
    }

    pub(super) async fn request_ticket(
        self: Arc<Self>,
        owner: Arc<IrohRelayLeaseHandle>,
        client: RendezvousControlClient,
    ) -> Result<IrohRelayTicket> {
        let _admission = self.admission.lock().await;
        self.admit(&owner, &client).await?;
        if !owner.authorizes_origin(&self.name) {
            self.retire(&owner.endpoint_id, owner.generation, "retired_before_issue");
            bail!(
                "iroh relay lease retired before ticket issuance at {}",
                self.name
            );
        }
        // An unsuccessful renewal may nevertheless have created a newer server
        // lease. Do not free that slot using an older ticket's expiry.
        if let Some(entry) = lock(&self.state).entries.get_mut(&owner.endpoint_id) {
            entry.expires_at = None;
        }
        let ticket = client.issue_iroh_relay_ticket(&owner.endpoint_id).await?;
        if let Some(entry) = lock(&self.state).entries.get_mut(&owner.endpoint_id) {
            // Convert once to a monotonic deadline so later wall-clock changes
            // cannot prematurely free an occupied slot.
            entry.expires_at = Instant::now().checked_add(Duration::from_secs(
                ticket
                    .expires_at_unix
                    .saturating_add(EXPIRY_GRACE_SECS)
                    .saturating_sub(unix_ts()),
            ));
            entry.ticket = Some(ticket.clone());
        }
        if !owner.authorizes_origin(&self.name) {
            self.retire(&owner.endpoint_id, owner.generation, "retired_during_issue");
            bail!(
                "iroh relay lease retired during ticket issuance at {}",
                self.name
            );
        }
        Ok(ticket)
    }

    async fn admit(
        self: &Arc<Self>,
        owner: &Arc<IrohRelayLeaseHandle>,
        client: &RendezvousControlClient,
    ) -> Result<()> {
        if !owner.authorizes_origin(&self.name) {
            bail!("iroh relay lease revoked at {}", self.name);
        }
        loop {
            let candidate = {
                let mut state = lock(&self.state);
                prune_expired(&mut state);
                if let Some(entry) = state.entries.get(&owner.endpoint_id)
                    && entry.generation == owner.generation
                {
                    if entry.release_pending {
                        bail!("iroh relay lease release pending at {}", self.name);
                    }
                    return Ok(());
                }
                state
                    .entries
                    .get(&owner.endpoint_id)
                    .map(|entry| {
                        (
                            owner.endpoint_id.clone(),
                            entry.generation,
                            entry.owner.clone(),
                        )
                    })
                    .or_else(|| {
                        (state.entries.len() >= self.limit)
                            .then(|| {
                                state
                                    .entries
                                    .iter()
                                    .min_by_key(|(_, entry)| {
                                        (!entry.release_pending, entry.last_access)
                                    })
                                    .map(|(id, entry)| {
                                        (id.clone(), entry.generation, entry.owner.clone())
                                    })
                            })
                            .flatten()
                    })
            };
            let Some((endpoint_id, generation, previous)) = candidate else {
                break;
            };
            if let Some(previous) = previous.upgrade() {
                previous.revoke_origin(&self.name);
            }
            self.retire(&endpoint_id, generation, "lru_eviction");
            self.release_locked(&endpoint_id, generation)
                .await
                .map_err(|error| {
                    anyhow!(
                        "failed releasing LRU iroh relay lease {endpoint_id} at {}: {error:#}",
                        self.name
                    )
                })?;
        }
        if !owner.authorizes_origin(&self.name) {
            bail!("iroh relay lease retired during admission at {}", self.name);
        }
        let mut state = lock(&self.state);
        state.next_access = state.next_access.saturating_add(1);
        let last_access = state.next_access;
        state.entries.insert(
            owner.endpoint_id.clone(),
            Entry {
                generation: owner.generation,
                owner: Arc::downgrade(owner),
                client: client.clone(),
                last_access,
                expires_at: None,
                ticket: None,
                release_pending: false,
                cleanup_scheduled: false,
                release_failures: 0,
                retry_at: None,
            },
        );
        Ok(())
    }

    pub(super) fn touch(&self, endpoint_id: &str, generation: u64) {
        let mut state = lock(&self.state);
        state.next_access = state.next_access.saturating_add(1);
        let access = state.next_access;
        if let Some(entry) = state.entries.get_mut(endpoint_id)
            && entry.generation == generation
            && !entry.release_pending
        {
            entry.last_access = access;
        }
    }

    pub(super) fn ticket(&self, endpoint_id: &str, generation: u64) -> Option<IrohRelayTicket> {
        lock(&self.state)
            .entries
            .get(endpoint_id)
            .filter(|entry| entry.generation == generation && !entry.release_pending)
            .and_then(|entry| entry.ticket.clone())
            .filter(|ticket| ticket.expires_at_unix > unix_ts())
    }

    pub(super) fn retire(
        self: &Arc<Self>,
        endpoint_id: &str,
        generation: u64,
        reason: &'static str,
    ) {
        let mut state = lock(&self.state);
        let Some(entry) = state
            .entries
            .get_mut(endpoint_id)
            .filter(|entry| entry.generation == generation)
        else {
            return;
        };
        entry.release_pending = true;
        if entry.cleanup_scheduled {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(endpoint_id, origin = %self.name, reason, "cannot schedule iroh lease cleanup outside Tokio");
            return;
        };
        entry.cleanup_scheduled = true;
        let budget = self.clone();
        let endpoint_id = endpoint_id.to_string();
        runtime.spawn(async move {
            budget.cleanup(endpoint_id, generation, reason).await;
        });
    }

    async fn cleanup(self: Arc<Self>, endpoint_id: String, generation: u64, reason: &'static str) {
        loop {
            let retry_at = lock(&self.state)
                .entries
                .get(&endpoint_id)
                .filter(|entry| entry.generation == generation)
                .and_then(|entry| entry.retry_at);
            if let Some(retry_at) = retry_at {
                tokio::time::sleep(retry_at.saturating_duration_since(Instant::now())).await;
            }
            match self.release_once(&endpoint_id, generation).await {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(endpoint_id, origin = %self.name, reason, %error, "iroh lease release pending; retrying with backoff")
                }
            }
        }
    }

    pub(super) async fn release_once(&self, endpoint_id: &str, generation: u64) -> Result<()> {
        let _admission = self.admission.lock().await;
        self.release_locked(endpoint_id, generation).await
    }

    async fn release_locked(&self, endpoint_id: &str, generation: u64) -> Result<()> {
        let client = {
            let mut state = lock(&self.state);
            prune_expired(&mut state);
            let Some(entry) = state
                .entries
                .get(endpoint_id)
                .filter(|entry| entry.generation == generation)
            else {
                return Ok(());
            };
            if entry
                .retry_at
                .is_some_and(|retry_at| retry_at > Instant::now())
            {
                bail!("iroh relay lease release backoff at {}", self.name);
            }
            entry.client.clone()
        };
        match client.release_iroh_relay_ticket(endpoint_id).await {
            Ok(released) => {
                let mut state = lock(&self.state);
                if state
                    .entries
                    .get(endpoint_id)
                    .is_some_and(|entry| entry.generation == generation)
                {
                    state.entries.remove(endpoint_id);
                }
                tracing::info!(event = "iroh_relay_lease_released", endpoint_id, origin = %self.name, released,
                    "released client-budgeted endpoint-bound iroh relay lease");
                Ok(())
            }
            Err(error) => {
                if let Some(entry) = lock(&self.state)
                    .entries
                    .get_mut(endpoint_id)
                    .filter(|entry| entry.generation == generation)
                {
                    entry.release_failures = entry.release_failures.saturating_add(1);
                    entry.retry_at =
                        Some(Instant::now() + release_retry_delay(entry.release_failures));
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn occupied(&self) -> usize {
        lock(&self.state).entries.len()
    }
}

fn release_retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1_u64 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

fn prune_expired(state: &mut OriginState) {
    let now = Instant::now();
    state
        .entries
        .retain(|_, entry| entry.expires_at.is_none_or(|expiry| expiry > now));
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
