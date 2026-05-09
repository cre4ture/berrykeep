# Client SDK Multi-Node Routing Design

## Status

Initial multi-endpoint routing, startup probing, foreground failover, and
opportunistic background quality refresh are implemented.

## Context

The current client bootstrap already carries multiple direct public API endpoints and relay
targets, but `client-sdk` collapses that set to a single `IronMeshClient` transport during
construction. Once that single target is selected, request retries only reset that target's
local session pool.

This means a single bad node or bad path can still dominate the client runtime even when the
bootstrap contains other usable server-nodes.

## Correction: read-through cache is already available

The earlier assumption that read-through cache was still only a design direction is outdated.
The server-side implementation is already present and is exercised by the gallery map-view path.

Confirmed implementation surfaces:

- `crates/server-node-sdk/src/lib.rs`: synchronous read-through chunk hydration on object reads,
- `crates/web-ui-backend/src/mbtiles.rs`: logical-file chunk caching used by map tile reads,
- `tests/system-tests/src/cluster_test.rs`: five-node non-replica read-through coverage.

So the client-side routing work should treat read-through cache as an existing server capability,
not as a prerequisite gap.

## Goals

- Keep multiple server-node targets active inside one `IronMeshClient`.
- Isolate failures per target so one failing connection never blocks other targets.
- Automatically route requests to the best currently known target.
- Preserve the existing public `IronMeshClient` API for callers.
- Keep direct and relay paths to the same logical node independent.
- Seed route quality at construction time and keep adapting during runtime.

## Non-goals

- Changing client-visible HTTP APIs.
- Replacing server-side placement, replication, or read-through cache behavior.
- Making request execution exactly-once across node failover. Transport-level retries remain
  best-effort and at-least-once where the current SDK already behaves that way.

## Endpoint model

Each configured route becomes an internal endpoint entry with:

- endpoint identity: `node_id + path_kind + endpoint locator`,
- transport kind: direct HTTPS or relay tunnel,
- independent HTTP client or relay transport state,
- independent `TransportSessionPool`,
- runtime quality state.

Direct and relay routes to the same node are separate endpoints. A direct-path failure must not
 poison that node's relay path, and vice versa.

## Quality model

Each endpoint tracks:

- EWMA latency,
- EWMA throughput,
- consecutive failures,
- last success and last failure timestamps,
- circuit-open-until timestamp,
- bootstrap order and static transport bias.

Ranking rules:

- lower latency is better,
- higher throughput is better,
- recent failures apply a large penalty,
- open circuits are skipped until their cooldown expires,
- direct retains only a small static bias over relay so materially better relay paths can win.

## Startup behavior

Bootstrap construction should no longer stop after the first reachable target.

Instead it should:

1. build all syntactically valid endpoint transports,
2. combine them into one `IronMeshClient`,
3. run a focused connection-quality probe across all built endpoints,
4. choose the best successful endpoint as the initial active route,
5. return an error only if no endpoint can be built or no endpoint can complete the startup probe.

The startup probe should be cheap and safe:

- direct unsigned clients: `/api/v1/health`,
- signed direct or relay clients: `/api/v1/diagnostics/latency` with a minimal sample config.

## Request routing

Buffered requests, streamed reads, and streamed writes should all route through the same
endpoint-selection layer.

For each request:

1. rank endpoints using current quality state,
2. try the best candidate,
3. on transport/session/setup failure, record the failure on that endpoint only,
4. immediately retry the next candidate,
5. stop failover once a response is received unless the response is one of the small retryable
   gateway/unavailable statuses.

The router must not hold a global mutex while performing network I/O.

## Background quality refresh

The client should opportunistically refresh endpoint quality in the background when:

- an endpoint has never been measured,
- the last quality probe is stale,
- repeated failures indicate the active ordering may be wrong.

These refreshes should be best-effort, rate-limited, and must never block foreground requests.

## Public API semantics

Existing helper methods keep representing the current active route:

- `uses_relay_transport()` => active route is relay,
- `relay_target_node_id()` => active relay target if the active route is relay,
- `direct_server_base_url()` => active direct URL if the active route is direct,
- `rendezvous_client()` => active route's rendezvous client if the active route is relay,
- `transport_session_pool_snapshot()` => active route snapshot.

This preserves current CLI and web-backend expectations.

## Failure handling

Target failover should ignore the exact transport failure class from the caller perspective.

Equivalent failover triggers include:

- DNS or TCP connect failure,
- TLS failure,
- WebSocket failure,
- relay ticket or rendezvous failure,
- multiplex handshake failure,
- stream open or response read failure,
- request timeout.

HTTP responses are different:

- `502`, `503`, and `504` are retryable across endpoints,
- semantic application responses such as `401`, `403`, `404`, and `409` are returned directly.

## Validation plan

- unit tests for endpoint ranking and circuit-breaker behavior,
- direct multi-endpoint test where one node fails and another succeeds,
- mixed direct and relay test where direct degrades and relay becomes active,
- bootstrap test proving multi-target construction no longer collapses to one target,
- focused `client-sdk` regression tests for current helper semantics.

## Rollout notes

The current implementation lands the internal endpoint pool, startup probing, foreground
failover, and best-effort background reprobes without changing the public API again.