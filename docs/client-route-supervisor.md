# Client route supervisor

The client SDK separates route discovery, route health, request failover, and transport sessions.
All clones of an `IronMeshClient` share these components; a request does not create another router
or another background maintenance loop.

## Stable route plans

Rendezvous discovery may add, remove, or reorder the route registry. Numeric indices therefore
exist only in diagnostics and mobile logs. Foreground request plans and upload affinity use
`RouteId`, which wraps the canonical transport route key. A candidate is resolved from its
`RouteId` immediately before an attempt, so reconciliation cannot make an old index point at a
different route.

Admission is also resolved immediately before each attempt. When one request reports a transport
failure and opens a route's circuit, other requests that have not started their next attempt skip
that route in favor of an available candidate. Attempts already using the route are not cancelled.

## Orthogonal route state

A route has three independent state dimensions:

- validation: `Probation` or `Validated`;
- circuit: `Closed` or `OpenUntil(timestamp)`;
- probe ownership: `Idle` or `InFlight`.

Success, failure, probe-claim, and probe-completion transitions are centralized on the endpoint
state. Measurements and diagnostic history remain data associated with that state, rather than
additional state-machine variants.

## Foreground execution

Buffered requests, upload chunks, streaming reads/writes, and object downloads use the same
foreground executor. It owns stable candidate resolution, retryable HTTP classification,
Rendezvous-backpressure suppression, circuit updates, and terminal diagnostic publication.
Transport-specific helpers only perform I/O and report measurements. A request with a consumed
streaming body is terminal because its body cannot safely be replayed.

## Background maintenance

Synthetic route probing is not started by ordinary request entry points. One shared supervisor task
is started lazily for the shared router and coalesces these signals:

- route membership changed;
- a foreground request completed;
- lifecycle or periodic maintenance is due.

Intermediate route failures do not wake maintenance while the same foreground request is still
failing over, so a synthetic probe cannot race that request to its next candidate.

Probe claims remain atomic, and the policy caps both batch size and total probe concurrency. The
explicit diagnostics refresh is intentionally separate and can still probe every route.
Both paths share one probe-execution gate, so a full diagnostic refresh cannot overlap the normal
supervisor and clear its probe ownership. Background attempts are always published with
`BackgroundMaintenance` impact and never become a user-facing connection result.

The mobile maintenance policy targets one usable primary and one validated backup, runs at most
one probe at a time, and deliberately warms at most two routes. Foreground requests still return
after their first successful route; they never wait for the maintenance target. Additional
discovered candidates stay on probation until the target becomes deficient. A backup may be
disconnected by lower-level ticket or session resource management; validation does not promise a
permanently open socket.
