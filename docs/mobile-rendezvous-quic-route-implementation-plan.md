# Dynamic Rendezvous-Discovered QUIC Routes for Mobile Clients

Status: implemented in PR #221

Audience: an implementation agent working on a macOS host, with responsibility
for the shared Rust stack and the Android/iOS bindings

Related documents:

- `docs/iroh-direct-quic-integration-plan.md`
- `docs/rendezvous-dynamic-reachability-proposal.md`
- `docs/client-sdk-multi-node-routing-design.md`
- `docs/nat-traversal-rendezvous-strategy.md`
- `docs/iroh-relay-companion-operations.md`

## Implementation record

This plan is implemented by the following shared and platform changes:

- `client-sdk` now owns asynchronous discovery, managed route refresh,
  stable-key reconciliation, route retirement, identity renewal, and
  all-route-failure refresh triggers.
- CLI, Android, and iOS construct that managed client; Kotlin and Swift only
  forward lifecycle/network hints and persist a renewed identity through their
  existing secure stores.
- Direct QUIC candidates validate their authenticated transport identity, while
  server nodes accept `IRONMESH_DIRECT_QUIC_RELAY_URLS` and advertise the
  configured iroh relay metadata through Rendezvous.

An iroh-compatible relay must still be deployed and health-checked in each
production environment before relying on relay-assisted NAT traversal. The
operational procedure is in `docs/iroh-relay-companion-operations.md`; this
repository cannot deploy environment-owned relay infrastructure itself.

## 1. Decision summary

Make dynamic Rendezvous discovery and route reconciliation a capability of the
shared `client-sdk`, not of the Android or iOS applications.

Every bootstrap-aware client must be able to:

1. construct a usable client from static bootstrap targets immediately;
2. discover fresher target candidates from the trusted Rendezvous service;
3. merge, rank, add, retire, and probe routes without replacing the public
   client handle;
4. prefer a discovered `DirectQuic` route when policy and quality permit it;
5. keep direct HTTPS and the existing Rendezvous relay as safe fallbacks.

Android and iOS must only provide lifecycle and network-change *hints* plus
identity persistence. They must not parse candidates, choose transports, or
contain QUIC/NAT-specific routing rules.

This is a client-route-lifecycle project. It does not replace the existing
IronMesh Rendezvous control plane, the buffered HTTP-over-stream protocol, or
the current relay tunnel.

## 2. Why the mobile apps do not currently use hole punching

The necessary pieces already exist on `main`, but they are not composed by the
mobile startup paths.

| Layer | Existing capability | Current gap |
| --- | --- | --- |
| Rendezvous | `/control/discovery` returns node candidates and Rendezvous peers. | None for basic candidate discovery. |
| `client-sdk` bootstrap | `ConnectionBootstrap::refresh_dynamic_targets_blocking(...)` fetches discovery and produces `DirectQuic`, direct HTTPS, and relay planned targets. | It is a blocking one-shot helper and does not update an already-built client. |
| Direct QUIC | `DirectQuicEndpoint` and the client session pool can dial a discovered `DirectQuic` candidate with iroh. | A mobile client never receives the discovered targets when it is constructed. |
| CLI | The bootstrap path refreshes dynamic targets before building the client. | This logic is application-specific and is not reusable by mobile clients. |
| Android/iOS | Both create a client from `build_client_with_identity...` using only static bootstrap targets. | Their later route snapshot refresh probes the existing endpoint set; it does not rediscover targets. |

In particular, `IronMeshClient::refresh_connection_route_snapshot()` is a
quality probe for routes already held by `ClientEndpointRouter`. It cannot add
a `DirectQuic` route because the router currently owns an immutable endpoint
vector. Therefore it is expected that an Android test never observes a
Rendezvous-discovered QUIC route.

The relevant code locations are:

- `crates/client-sdk/src/bootstrap.rs` — dynamic discovery and refreshed
  target construction;
- `apps/cli-client/src/main.rs` — the only current application consumer of
  refreshed targets;
- `apps/android-app/src/lib.rs` and `apps/ios-app/src/lib.rs` — static mobile
  construction paths;
- `crates/client-sdk/src/ironmesh_client.rs` — immutable router endpoint set;
- `crates/client-sdk/src/session_pool.rs` — already supports `DirectQuic`;
- `crates/transport-sdk/src/direct_quic.rs` — iroh endpoint and candidate
  conversion.

## 3. Goals and boundaries

### Goals

- One transport-selection implementation for CLI, sync agents, Android, and
  iOS.
- Dynamic addition and removal of trusted Rendezvous candidates while retaining
  a stable `IronMeshClient` handle for callers.
- Bounded, fail-open discovery: a broken or unavailable Rendezvous service
  must never make a valid static direct or relay route unusable.
- Route diagnostics that make the selected transport, candidate source, and
  direct-versus-relay outcome visible to tests and support tools.
- Correct identity renewal and persistence across all FFI callers.
- A production-ready path for relay-assisted iroh NAT traversal, rather than
  assuming that candidate exchange alone can traverse all NATs.

### Non-goals

- No Android-only or iOS-only path-selection policy.
- No Kotlin or Swift implementation of iroh, UDP hole punching, candidate
  parsing, or fallback logic.
- No guarantee that a suspended mobile process keeps a QUIC session alive.
- No removal of direct HTTPS or the current WebSocket relay tunnel in this
  work.
- No change to enrollment, request authorization, or application request
  semantics.

## 4. Target architecture

```text
ConnectionBootstrap + ClientIdentityMaterial
                 |
                 v
     client-sdk managed route controller
       | static targets       | Rendezvous discovery
       |                      |
       +---------- merge, validate, rank ----------+
                                                    |
                                                    v
                          mutable ClientEndpointRouter
                           /           |            \
                    DirectQuic     DirectHttps    RelayTunnel
                       (iroh)         (TLS)       (existing)
                                                    |
                  Android / iOS: persistence and lifecycle hints only
```

The route controller owns three separable responsibilities:

1. **Discovery:** obtain and validate dynamic candidate sets using only the
   bootstrap's trusted Rendezvous URLs and the enrolled client identity.
2. **Reconciliation:** turn static and discovered targets into one keyed route
   set, preserving live state for unchanged routes and retiring obsolete ones.
3. **Quality selection:** reuse the existing endpoint quality and circuit
   breaker model to select and fail over between routes.

Keeping these responsibilities inside `client-sdk` is the key to an
implementation without platform special cases.

## 5. Common client-sdk contract

The exact public names are an implementation detail. The following contract
is the required behavior and recommended shape:

```rust
pub struct ManagedClientOptions {
    pub initial_discovery_timeout: Duration,
    pub discovery_ttl: Duration,
    pub refresh_on_transport_failure: bool,
}

pub enum RouteRefreshReason {
    Startup,
    Stale,
    NetworkChanged,
    TransportFailure,
    Foregrounded,
    ExplicitDiagnosticRequest,
}

pub struct RouteRefreshOutcome {
    pub discovery_used: bool,
    pub discovery_error: Option<RouteDiscoveryError>,
    pub routes_added: usize,
    pub routes_removed: usize,
    pub routes_retained: usize,
}

impl ConnectionBootstrap {
    pub async fn build_managed_client_with_identity(
        &self,
        identity: ClientIdentityMaterial,
        options: ManagedClientOptions,
    ) -> Result<ManagedIronMeshClient>;
}

impl ManagedIronMeshClient {
    pub fn client(&self) -> IronMeshClient;
    pub async fn refresh_routes(
        &self,
        reason: RouteRefreshReason,
    ) -> RouteRefreshOutcome;
    pub fn notify_network_changed(&self);
    pub fn route_snapshot(&self) -> ConnectionRouteSnapshot;
}
```

The implementation may expose these methods directly on `IronMeshClient`
instead, provided that cloning the public client keeps sharing one mutable
route controller. It must not require every app to reconstruct a new client
after discovery.

`refresh_dynamic_targets_blocking` remains useful as a CLI compatibility shim,
but the new core API must be asynchronous. It currently creates a Tokio runtime
internally and is unsafe to call from an async FFI runtime or another Tokio
runtime.

### 5.1 Startup behavior

1. Validate bootstrap and identity, including cluster binding.
2. Build a router from static targets so direct HTTPS and relay fallback exist
   before discovery completes.
3. Start a bounded initial discovery attempt. A foreground caller may wait up
   to `initial_discovery_timeout`; otherwise the static client is returned and
   discovery continues only while the runtime is active.
4. Reconcile successful discovery into the router and run focused background
   quality probes.
5. If discovery fails, retain the static routes, record the structured error,
   and retry only according to the refresh policy.

The default initial timeout should be short and configurable (for example,
one to two seconds). It must not turn ordinary app startup into an unbounded
network wait.

### 5.2 Discovery and validation rules

The controller must reuse the existing discovery semantics and enforce the
following invariants before a route becomes selectable:

- use only `rendezvous_urls` rooted in the bootstrap or learned from a
  successful response from such a trusted Rendezvous service;
- require the Rendezvous client certificate when
  `rendezvous_mtls_required` is enabled;
- retain static target node IDs, because they seed per-node discovery and
  identity validation;
- bind every discovered candidate to the expected `target_node_id` and
  cluster, never to an unverified endpoint URL alone;
- normalize and deduplicate candidates before creating sessions;
- honour `RelayMode::Required` and `RelayMode::Disabled` exactly; a
  `DirectQuic` candidate must not bypass explicit relay policy;
- treat discovery as advisory routing information, not as a new trust root.

An iroh endpoint ID is not an HTTPS certificate hostname. The route registry
must retain the authenticated Rendezvous mapping from the expected node ID to
the DirectQuic endpoint ID, and the connection handshake must prove that the
remote peer owns that endpoint key. The implementation agent must also verify
that the server-side transport handshake rejects a cluster or target-node
mismatch. If the current handshake does not give the client a reciprocal,
authenticated node assertion, add one (or bind the node credential to the iroh
endpoint ID) before treating DirectQuic as equivalent to the existing
node-identity-validated HTTPS and inner-mTLS relay paths.

Static targets are retained even after a dynamic refresh. They are needed for
bootstrap recovery, fresh target-node discovery, and direct/relay fallback.

### 5.3 Stable route identity and reconciliation

`ClientEndpointRouter` currently contains `Arc<Vec<ClientEndpoint>>`. Replace
that immutable membership with an internal, synchronized route registry.
Do not expose a write lock while performing network I/O.

Use a stable route key rather than a vector index. A suitable key contains:

- target node ID;
- transport path kind;
- normalized direct URL or QUIC endpoint ID;
- QUIC ALPN and relay identity where relevant.

On every successful discovery refresh:

1. derive desired route entries from static targets plus discovered targets;
2. preserve quality state and session pools for unchanged stable keys;
3. create new endpoint entries lazily, including a DirectQuic session pool;
4. retire routes that disappeared from the dynamic set after a freshness
   grace period, but never remove the static fallback solely because discovery
   omitted it;
5. close obsolete QUIC sessions outside the registry write section;
6. publish a new immutable route snapshot for diagnostics and request routing.

Route selection should rank the complete reconciled set. `DirectQuic` is a
preferred transport candidate, but endpoint quality, circuit state, and the
configured relay mode remain authoritative. A newly discovered candidate must
not discard a currently healthy route merely because it is newer.

### 5.4 Refresh triggers and backoff

Refreshes are coalesced: one in-flight discovery is shared by all callers.
The controller should apply jittered exponential backoff after failures and
should not issue discovery for every individual HTTP request.

Required triggers:

| Trigger | Expected behavior |
| --- | --- |
| Client startup | One bounded initial discovery attempt. |
| Discovery TTL expired | Best-effort refresh before or alongside the next foreground operation. |
| Transport circuit opens / all routes fail | Coalesced refresh before final fallback failure. |
| Explicit diagnostics action | Immediate refresh and an observable outcome. |
| Android connectivity change | Hint only; schedule/coalesce a refresh. |
| iOS app foreground or a new File Provider operation | Hint only; refresh if stale. |

No platform needs to maintain a permanent background timer. In particular, iOS
must work when the process was suspended and the next operation is its only
opportunity to refresh.

### 5.5 Identity renewal and FFI persistence

Identity renewal is common logic. The route controller should check and renew
the Rendezvous client identity before each discovery session when necessary.
It must report an `IdentityUpdated` event or callback carrying the renewed
serialized identity.

Each FFI wrapper persists that update through its existing secure application
state mechanism. This is deliberate and is the only platform-specific part of
the flow:

- Android passes the update to its current persisted client-identity storage.
- iOS passes it to the shared App Group/keychain-backed identity storage used
  by the app and File Provider.

The core must continue to use the in-memory renewed identity if persistence
fails, while returning a structured persistence warning. It must never silently
fall back to an expired identity for discovery.

## 6. Relay-assisted QUIC traversal is a deployment prerequisite

The existing DirectQuic implementation is real, but server endpoints are
currently created with `DirectQuicEndpointConfig::new(secret_key)`. That
default has no relay URL, so iroh's relay mode is disabled. Candidate exchange
can still enable direct connections in favourable networks, but it is not a
reliable production hole-punching solution for restrictive or symmetric NATs.

The existing IronMesh HTTPS/WebSocket relay tunnel is not automatically an
iroh relay. A production rollout therefore needs an iroh-compatible relay
companion or an explicitly supported equivalent.

### Required deployment work

1. Deploy and health-check an iroh-compatible relay next to, or independently
   from, the Rendezvous service.
2. Add server-node configuration for the relay URL assigned to its DirectQuic
   endpoint and construct `DirectQuicEndpointConfig` with that configuration.
3. Publish that relay URL through the existing `DirectQuic` candidate transport
   hints. Clients must consume the advertised value; do not hard-code a relay
   URL in Android or iOS.
4. Add rollout validation and diagnostics for endpoint ID, advertised relay,
   selected iroh path, direct success, and relay fallback reason.
5. Keep the current IronMesh relay tunnel as the guaranteed fallback during
   rollout and for UDP-hostile networks.

For the first implementation, one canonical relay URL per endpoint is enough
because the current candidate model carries one `relay_url`. Multi-relay
advertisement and failover should be a versioned follow-up rather than a
platform-specific workaround.

## 7. Changes by component

| Component | Required change | Must not do |
| --- | --- | --- |
| `crates/client-sdk` | Add the async route controller, route reconciliation, refresh policy, events, and diagnostics. Refactor the CLI to consume it. | Call a blocking discovery helper inside async code or expose router internals to FFI. |
| `crates/client-sdk::ClientEndpointRouter` | Replace immutable membership with keyed, snapshot-based route reconciliation. Preserve quality/session state by key. | Use vector indices as durable route IDs. |
| `crates/transport-sdk` | Validate DirectQuic candidate hints and expose iroh path diagnostics needed by `client-sdk`. | Add mobile-specific transport behavior. |
| `crates/server-node-sdk` | Configure a DirectQuic relay companion, advertise its candidate metadata, and report health. | Treat IronMesh's HTTP relay as an iroh relay. |
| `apps/cli-client` | Replace its bespoke one-shot refresh/build sequence with the common managed client. | Remain the only user of dynamic discovery. |
| Android JNI/Rust wrapper | Construct the managed shared client, persist identity updates, and forward connectivity hints. | Parse candidate payloads or choose QUIC versus relay. |
| iOS Rust/Swift wrapper | Construct the managed shared client, persist identity updates, and forward foreground/operation hints. | Depend on perpetual background execution. |
| Sync/desktop consumers | Adopt the same managed constructor when touched, so the behavior remains uniform. | Duplicate the CLI sequence. |

## 8. Implementation sequence

The work should be delivered in small, independently reviewable pull requests.
The macOS implementation agent should not attempt a broad Android/iOS rewrite
first.

### PR 1 — async discovery contract and common managed construction

- Introduce asynchronous discovery methods in `client-sdk`; retain the
  blocking helper as a thin CLI compatibility wrapper.
- Add the managed-client/route-controller abstraction with static-only initial
  construction and fail-open initial discovery.
- Refactor the CLI to use the common abstraction.
- Add core tests for successful dynamic discovery, mTLS discovery, and static
  fallback on discovery failure.

Exit criterion: the CLI contains no bespoke refreshed-target construction, and
a core test can observe a discovered `direct_quic` route through the managed
client snapshot.

### PR 2 — mutable endpoint registry and reconciliation

- Introduce stable route keys and a synchronized route registry behind the
  existing client request surface.
- Implement add/retain/retire reconciliation and coalesced refreshes.
- Preserve route quality state and live session pools for unchanged keys.
- Add failure-triggered and TTL-triggered refresh tests.

Exit criterion: a client handle created once can gain, prefer, and later retire
a discovered QUIC route without dropping its static relay fallback.

### PR 3 — DirectQuic relay companion configuration and diagnostics

- Add server/deployment configuration for iroh relay URLs.
- Bind server DirectQuic endpoints with the configured relay and advertise it
  in Rendezvous candidates.
- Add path diagnostics and system coverage for candidate propagation.
- Document the operational relay deployment and health checks.

Exit criterion: a test environment with a configured iroh relay can verify
that both server and client use the advertised relay metadata, while the
existing IronMesh relay path still succeeds when QUIC cannot.

### PR 4 — thin mobile adoption

- Replace Android and iOS static client construction with the common managed
  constructor.
- Wire common identity-update persistence callbacks.
- Add a single `notify_network_changed` FFI call for Android and lifecycle/
  operation refresh hints for iOS.
- Preserve current user-facing route diagnostics, extending them only with the
  common snapshot fields.

Exit criterion: neither mobile wrapper contains candidate ranking, direct QUIC
configuration, or app-specific fallback logic.

### PR 5 — device validation and rollout hardening

- Run the mobile tests on macOS, add diagnostics assertions, and exercise
  static fallback after a simulated discovery failure.
- Perform a real-device or representative NAT test only after the relay
  companion exists.
- Tune TTL/backoff defaults from observed metrics; do not tune them separately
  by operating system without evidence of a core defect.

## 9. Test and validation strategy

### Shared Rust tests

- Discovery merges `DirectQuic`, direct HTTPS, and relay candidates in the
  correct policy order.
- mTLS-enabled Rendezvous discovery rejects a missing client identity and
  succeeds with the enrolled Rendezvous identity.
- Discovery failure leaves static routes request-capable and records a
  diagnostic error.
- A DirectQuic candidate whose authenticated node mapping or transport
  handshake target does not match is rejected before application traffic.
- Reconciliation preserves the session/quality state of an unchanged key,
  creates new keys, and removes only stale dynamic entries.
- An all-route transport failure schedules one coalesced refresh rather than a
  request storm.
- `RelayMode::Required` never selects a direct candidate.

### System tests

- A bootstrap-aware managed client obtains a discovered `DirectQuic` candidate
  from Rendezvous and exposes it in its route snapshot.
- A reachable direct QUIC path is selected over an otherwise healthy relay
  route according to the common quality model.
- A failed QUIC path falls back to direct HTTPS or the IronMesh relay tunnel.
- Candidate changes in Rendezvous are reconciled without recreating the public
  client handle.
- With the relay companion configured, diagnostics distinguish an iroh direct
  path from iroh relay assistance and from the existing IronMesh relay tunnel.

### Android validation

- JNI/Rust unit tests construct the managed client using persisted bootstrap
  and identity data.
- A connectivity hint causes at most one coalesced refresh.
- Route diagnostics can show `direct_quic` when the shared test fixture offers
  such a candidate; no Android-specific route assertion is needed.
- Run the repository's Android debug check and the focused Android Rust tests.

### macOS/iOS validation

Run the repository's macOS lane after the shared tests pass:

```bash
just ci-ios
```

For focused iteration, use the commands mirrored by that lane:

```bash
cargo test --locked -p ios-app
(cd apps/apple-file-provider && swift test)
```

Add iOS wrapper tests for initial managed construction, a stale/foreground
refresh hint, identity persistence callback delivery, and static fallback after
failed discovery. These tests should use a deterministic Rust discovery fixture;
they must not depend on a simulator having a particular NAT.

## 10. Acceptance criteria

The implementation is complete only when all of the following hold:

- A valid Android or iOS bootstrap plus enrolled identity can dynamically gain
  a Rendezvous-discovered `DirectQuic` route through shared code.
- The selected route and direct/relay outcome are visible in a common route
  snapshot and diagnostics surface.
- Loss of Rendezvous discovery does not break existing static direct HTTPS or
  relay operation.
- Route membership can change without reconstructing the application-facing
  client handle.
- Rendezvous mTLS identity renewal is persisted through each platform's
  callback, with safe in-memory continuation if persistence is unavailable.
- No Kotlin, Swift, Android manifest permission, or iOS entitlement is needed
  solely to select a transport. Android already has the normal network
  permissions; iOS uses the same outbound networking entitlement model.
- A configured iroh relay companion is advertised by the server and consumed
  from candidate metadata, not from a hard-coded mobile URL.
- The required shared tests, Android check, and `just ci-ios` pass.

## 11. Risks and decisions for the implementer

| Risk | Required decision / mitigation |
| --- | --- |
| Nested Tokio runtime | Make managed discovery async. Keep blocking APIs outside async/FFI execution paths. |
| Races while target membership changes | Use immutable read snapshots plus a short registry update section; never hold it over I/O. |
| Route flapping | Preserve quality state, use TTL/grace periods, and apply the existing stability bias. |
| iOS suspension | Treat lifecycle hooks as opportunistic hints; refresh on foreground/next operation. |
| Android connectivity churn | Coalesce callback hints and apply backoff in core. |
| NATs that cannot be punched | Deploy an iroh relay companion and retain the current IronMesh relay fallback. |
| Trust-boundary regression | Retain the authenticated Rendezvous node-to-endpoint mapping and verify the DirectQuic handshake target; candidates are hints, not identities. |
| Large review surface | Keep the five PR slices above separate; do not combine server relay deployment with native wrapper refactors. |

## 12. Handoff checklist for the macOS implementation agent

1. Start from current `main` and re-check the listed code locations, because
   route APIs are actively evolving.
2. Implement PR 1 in `client-sdk` and refactor the CLI first. Do not touch
   Kotlin or Swift before the common tests prove the contract.
3. Implement PR 2 with a stable-key reconciliation test before enabling
   background or lifecycle triggers.
4. Treat the iroh relay companion as a required production prerequisite, not a
   mobile workaround. Land its configuration and system coverage separately.
5. Make Android/iOS changes thin adapters around the common API and include
   identity persistence tests.
6. Before each commit, verify that no platform layer duplicated candidate
   validation, target ranking, backoff, or fallback code.
7. Before opening the final implementation PR, run focused crate tests,
   Android checks, `just ci-ios`, and the relevant system tests. Report the
   selected path in test output rather than only request success.
