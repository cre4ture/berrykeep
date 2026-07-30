# Embedded Iroh Relay: Same-Port Access Design

## Decision

The standalone IronMesh Rendezvous service exposes its embedded Iroh relay on
the existing Rendezvous origin and TCP port. For example, both control traffic
and relay traffic use `https://rendezvous.example.com:443`; Iroh connects to
the standard `/relay` path on that origin.

The two protocols use separate TCP/TLS connections. An Iroh relay connection
is a long-lived HTTP/1.1 WebSocket upgrade and cannot share an already-upgraded
connection with normal Rendezvous requests. This separation is internal and
does not require another public address, certificate, firewall rule, reverse
proxy, or operator-managed listener.

The embedded relay is enabled by default. Operators may explicitly disable it,
or tune its ticket lifetime and per-connection receive limits.

## Request Routing

One listener owns the configured `IRONMESH_RENDEZVOUS_BIND` address:

1. TLS is terminated with the existing Rendezvous server certificate.
2. TLS ALPN continues to select HTTP/2 for capable Rendezvous clients and
   HTTP/1.1 for Iroh's WebSocket connection.
3. On HTTP/1.1, the raw request path is inspected before it enters the Axum
   router.
4. `GET /relay` is handled by Iroh's unmodified embedded `RelayService`.
5. Every other path is handled by the existing Rendezvous router.

The listener continues to offer optional client-certificate authentication.
Rendezvous control handlers enforce the authenticated certificate at the
application boundary. Iroh does not present an IronMesh client certificate, so
the relay path uses a relay ticket instead.

Plain HTTP remains available only when
`IRONMESH_RENDEZVOUS_ALLOW_INSECURE_HTTP=true` is explicitly selected for local
development and tests.

## Relay Authorization

A shared, long-lived bearer token is not used. Instead, an authenticated
Rendezvous client requests an Iroh relay ticket for its local Iroh endpoint ID:

1. The client creates or loads its Iroh secret key.
2. It authenticates to the Rendezvous control plane using its existing
   IronMesh client certificate.
3. It calls the relay-ticket endpoint with the corresponding public endpoint
   ID and cluster ID.
4. Rendezvous returns its relay origin, a short-lived signed ticket, and the
   expiration timestamp.
5. The Iroh client proves possession of the endpoint secret key during the
   standard Iroh relay handshake.
6. The relay verifies the ticket signature, lifetime, cluster context, and
   exact endpoint-ID binding before admitting the connection.

The ticket therefore combines two independent proofs:

- Rendezvous authorization to obtain the ticket; and
- possession of the Iroh endpoint secret key named by the ticket.

Copying a ticket does not grant access from another Iroh endpoint. Ticket
signing keys are generated in memory at Rendezvous startup. Restarting the
service invalidates existing tickets, after which authorized clients obtain
new ones through the normal refresh path.

Tickets are checked when a connection is established. The embedded relay also
disconnects an admitted connection when its ticket expires. Clients refresh
their ticket before expiration and update the live Iroh relay configuration,
so normal operation does not wait for the forced disconnect.

## Client and Server-Node Lifecycle

Server nodes already own a persistent Iroh key. Their Rendezvous heartbeat
requests a ticket for that key and reconciles the returned relay configuration.
The regular heartbeat naturally refreshes tickets and recovers after a
Rendezvous restart.

Client applications generate their local Iroh key before creating the
endpoint. When a direct-QUIC target was discovered through Rendezvous, the
client obtains an endpoint-bound ticket before enabling the relay and runs a
small refresh task for the lifetime of that endpoint.

Discovery responses advertise only the relay origin. They never expose the
server node's endpoint-bound ticket to another client.

Custom, operator-provided Iroh relay URLs and tokens remain supported by the
transport SDK. They are independent of the embedded same-port relay and do not
gain IronMesh ticket semantics automatically.

## Configuration

The default production setup needs no relay-specific variables. It derives the
relay origin from `IRONMESH_RENDEZVOUS_PUBLIC_URL`, uses the existing TCP bind
address for Rendezvous and `/relay`, and reuses the TLS identity for Iroh QUIC
Address Discovery on UDP `7842`.

Advanced controls are:

- `IRONMESH_IROH_RELAY_ENABLED` — defaults to `true`; set to `false` to disable
  the embedded relay.
- `IRONMESH_IROH_RELAY_TICKET_TTL_SECS` — ticket and admitted-connection
  lifetime.
- `IRONMESH_IROH_RELAY_CLIENT_RX_BYTES_PER_SECOND` — per-connection receive
  rate.
- `IRONMESH_IROH_RELAY_CLIENT_RX_MAX_BURST_BYTES` — per-connection burst size.
- `IRONMESH_IROH_RELAY_QUIC_BIND` — QAD UDP bind address; defaults to
  `0.0.0.0:7842` when a Rendezvous TLS identity is present.
- `IRONMESH_IROH_RELAY_QUIC_PUBLIC_PORT` — advertised QAD UDP port; defaults
  to the bind port.
- `IRONMESH_IROH_RELAY_QUIC_TLS_CERT` and
  `IRONMESH_IROH_RELAY_QUIC_TLS_KEY` — optional dedicated QAD identity for
  deployments that cannot reuse Rendezvous TLS.

There is intentionally no separate relay public URL or static authentication
token. The QAD endpoint only reports the caller's observed UDP address; relay
admission remains protected by endpoint-bound tickets on `/relay`.

## Security Properties and Limits

- Same-port routing does not weaken Rendezvous authorization: protected
  control handlers still require a valid client certificate.
- Relay admission is endpoint-bound and time-bounded.
- Ticket signatures are verified in constant time by HMAC.
- Ticket contents are authenticated but not encrypted; they contain identifiers
  and timestamps, never private keys or certificate material.
- Receive limits apply per relay connection. They complement, but do not
  replace, host-level bandwidth and connection monitoring.
- Anyone can reach `/relay` on the public port, but an unauthenticated or
  incorrectly bound upgrade is rejected before it becomes a relay client.
- In explicit insecure-HTTP development mode, the control plane cannot prove a
  client certificate. That mode is not a production security boundary.

## Acceptance Criteria

- Rendezvous control traffic and Iroh relay traffic work through the same
  public origin and port.
- No relay-specific public listener or reverse proxy is required.
- An authenticated client can obtain and use a ticket for its own endpoint.
- A missing, expired, modified, or differently bound ticket is rejected.
- Discovery never leaks another endpoint's ticket.
- Server nodes and clients refresh tickets without operator action.
- Existing non-Iroh Rendezvous and relay-tunnel behavior remains unchanged.
