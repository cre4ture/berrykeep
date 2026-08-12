# Embedded Iroh Relay Operations

Direct QUIC uses an Iroh-compatible relay for NAT traversal and, when a direct
path cannot be established, encrypted packet forwarding. It is not
interchangeable with the BerryKeep HTTPS/WebSocket relay tunnel. That tunnel
remains the independent fallback for UDP-hostile networks and mixed-version
deployments.

`ironmesh-rendezvous-service` runs the upstream Iroh relay protocol on its
existing public origin and listener. Rendezvous control requests and Iroh
`GET /relay` upgrades therefore use the same host, port, TLS certificate, and
TCP firewall rule. Iroh QUIC Address Discovery (QAD) uses UDP port `7842` with
the same TLS identity; that UDP port must also be reachable for NAT hole
punching. No relay-specific reverse proxy route, public hostname, or static
shared token is required.

The implementation and security model are summarized in
[Embedded Iroh Relay: Same-Port Access Design](embedded-iroh-relay-same-port-design.md).

## Default behavior

The embedded relay is enabled automatically. Its public origin is derived from
`IRONMESH_RENDEZVOUS_PUBLIC_URL`, and its TLS behavior follows the Rendezvous
listener.

When Rendezvous TLS is configured, QAD is enabled automatically on
`0.0.0.0:7842` and that port is advertised in authenticated relay tickets.
Installations without a TLS identity keep QAD disabled unless they provide a
dedicated identity, which is primarily useful for isolated development and
system-test networks.

Authenticated clients request short-lived relay tickets through the Rendezvous
control API. Every ticket is bound to one Iroh endpoint ID. The relay checks
that binding during Iroh's endpoint-key handshake and disconnects the
connection when the ticket expires. Server nodes and client SDKs refresh their
own tickets automatically; discovery advertises the origin but never another
endpoint's ticket.

Rendezvous protects two distinct ticket protocols. Endpoint-bound Iroh tickets
are revocable leases; duplicate requests for the same endpoint return the same
lease, and active Iroh relay connections have a separate per-client limit.
BerryKeep WebSocket relay-tunnel tickets are instead short-lived, single-use
pairing capabilities. Duplicate requests for the same source/target route
reuse one outstanding ticket until it is consumed. Both protocols return HTTP
`429 Too Many Requests` with `Retry-After` when the authenticated client's
lease/outstanding or rolling issuance limit is reached. Clients explicitly
release abandoned tickets through the authenticated release endpoints; expiry
is the crash-safety fallback.

Plain HTTP is accepted only with
`IRONMESH_RENDEZVOUS_ALLOW_INSECURE_HTTP=true` for local development and tests.
It does not provide the production mTLS authorization boundary.

## Optional controls

No relay-specific configuration is required for a normal production
deployment. Advanced settings are:

```bash
# Disable only when the deployment intentionally provides no embedded relay.
IRONMESH_IROH_RELAY_ENABLED=true

# Defaults: one-hour tickets, 16 MiB/s receive rate, 32 MiB burst.
IRONMESH_IROH_RELAY_TICKET_TTL_SECS=3600
IRONMESH_IROH_RELAY_CLIENT_RX_BYTES_PER_SECOND=16777216
IRONMESH_IROH_RELAY_CLIENT_RX_MAX_BURST_BYTES=33554432

# Global TCP/TLS admission defaults for every Rendezvous listener mode.
IRONMESH_RENDEZVOUS_MAX_CONNECTIONS=512
IRONMESH_RENDEZVOUS_MAX_TLS_HANDSHAKES=64

# Defaults: 10 outstanding leases/tickets, 10 ticket issues per minute,
# and 10 simultaneously connected Iroh relay endpoints per client.
IRONMESH_IROH_RELAY_MAX_TICKET_LEASES_PER_CLIENT=10
IRONMESH_IROH_RELAY_MAX_TICKET_ISSUES_PER_MINUTE=10
IRONMESH_IROH_RELAY_MAX_ACTIVE_CONNECTIONS_PER_CLIENT=10

# Optional when the public UDP port differs from the default.
IRONMESH_IROH_RELAY_QUIC_BIND=0.0.0.0:7842
IRONMESH_IROH_RELAY_QUIC_PUBLIC_PORT=7842

# Optional dedicated QAD identity; normally Rendezvous TLS is reused.
IRONMESH_IROH_RELAY_QUIC_TLS_CERT=/etc/ironmesh/qad.pem
IRONMESH_IROH_RELAY_QUIC_TLS_KEY=/etc/ironmesh/qad.key
```

Ticket lifetime must be between 300 and 86400 seconds. Receive limits are
applied per Iroh relay connection. The ticket lease and rolling issue settings
also protect WebSocket relay-tunnel ticket issuance on the same Rendezvous
service; active-connection counting applies specifically to Iroh relay
connections because WebSocket connections are protected independently at the
listener. The QAD certificate must cover every host
in the advertised relay origins. Bind and public ports must be non-zero; use
the public port setting when host-level port forwarding changes the UDP port.
The TCP connection limit is held for the full served connection, including
upgraded relay connections. The TLS limit is held only through the handshake
and must not exceed the connection limit. If the connection limit is set below
64 without an explicit TLS value, the TLS default is reduced to match it.
These defaults leave descriptor headroom when the service runs with
`RLIMIT_NOFILE=1024`.

`IRONMESH_DIRECT_QUIC_RELAY_URLS` and
`IRONMESH_DIRECT_QUIC_RELAY_AUTH_TOKEN` remain supported on server nodes for an
operator-managed external relay. Those settings are independent of the
embedded same-port relay and remain authoritative for overlapping URLs.

## Health and rollout checks

1. Check the normal Rendezvous `/health` endpoint and the Iroh-compatible
   `/ping` probe on the same origin. The health response's
   `iroh_relay_public_urls` field should contain the Rendezvous public origin.
   Confirm that UDP `7842` (or the configured public port) is reachable.
2. Confirm a node logs `reconciled direct QUIC endpoint from
   rendezvous-provided iroh relays`.
3. Inspect authenticated `/control/discovery`: a `direct_quic` candidate should
   contain the endpoint ID, ALPN, and `relay_url`. It must not contain a relay
   ticket.
4. Test from outside the server LAN. The route snapshot distinguishes
   `direct_quic` from `relay_tunnel`; after connection,
   `hole_punching_mode` reports `direct`, `relay`, or `unknown`.
5. Block UDP and confirm direct HTTPS or the BerryKeep `relay_tunnel` still
   completes the same request.

Do not remove direct HTTPS or the existing relay tunnel during rollout. They
remain the recovery paths for relay outages, restrictive networks, and
partially upgraded installations.
