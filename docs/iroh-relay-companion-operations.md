# Embedded Iroh Relay Operations

Direct QUIC uses an iroh-compatible relay for NAT traversal and, when a direct
path cannot be established, encrypted packet forwarding. It is not
interchangeable with the BerryKeep HTTPS/WebSocket relay tunnel. That tunnel
remains the independent fallback for UDP-hostile networks and mixed-version
deployments.

`ironmesh-rendezvous-service` can run the upstream iroh relay protocol in the
same process. It deliberately uses a separate internal listener: iroh upgrades
and Rendezvous mTLS have different connection-level requirements. A reverse
proxy terminates public TLS and forwards the relay hostname to this listener.
The Rendezvous process owns relay startup, failure detection, rate limits, and
shutdown.

## Enable the embedded relay

Set the following on the Rendezvous service:

```bash
IRONMESH_IROH_RELAY_BIND=127.0.0.1:19091
IRONMESH_IROH_RELAY_PUBLIC_URLS=https://iroh-relay.example.net
IRONMESH_IROH_RELAY_AUTH_TOKEN=<at-least-32-random-characters>
```

Generate and persist the token in the deployment secret store, for example
with `openssl rand -hex 32`. It is used only for relay admission and is never
logged. Rendezvous returns it over its authenticated control API to registered
nodes and enrolled clients. Do not put the token into a mobile application or
bootstrap JSON.

The listener is plain HTTP by design because public TLS is normally terminated
by the reverse proxy. Keep it on loopback or a protected container network. A
dedicated relay hostname can proxy all paths:

```nginx
location / {
    proxy_pass http://127.0.0.1:19091;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_buffering off;
    proxy_read_timeout 1d;
    proxy_send_timeout 1d;
}
```

The public URL must use HTTPS. Plain HTTP is accepted only together with
`IRONMESH_RENDEZVOUS_ALLOW_INSECURE_HTTP=true` for local tests. Multiple
comma-separated public URLs may point to the same listener during DNS or proxy
migrations.

Per-client ingress defaults to 16 MiB/s with a 32 MiB burst. Override these
limits when required:

```bash
IRONMESH_IROH_RELAY_CLIENT_RX_BYTES_PER_SECOND=16777216
IRONMESH_IROH_RELAY_CLIENT_RX_MAX_BURST_BYTES=33554432
```

Partial configuration, a short/missing token, a non-origin public URL, or a
bind collision with the Rendezvous listener fails startup. This prevents a
misconfigured service from silently becoming an open relay.

## Automatic node and client configuration

No relay setting is required on ordinary server nodes. Each successful
authenticated presence registration returns the active embedded relay
advertisement. The node reconciles additions, token rotations, and removals in
its live iroh endpoint and republishes its current Direct QUIC candidate.
Authenticated discovery adds the matching token to client-side candidate
metadata. The shared client SDK consumes it when creating the ephemeral mobile
iroh endpoint.

`IRONMESH_DIRECT_QUIC_RELAY_URLS` remains supported for an external relay or a
controlled migration. If that external relay requires the same static bearer
token, set `IRONMESH_DIRECT_QUIC_RELAY_AUTH_TOKEN` on the node. Explicit node
configuration remains authoritative when its URL overlaps a discovered relay.

## Health and rollout checks

1. Check `https://iroh-relay.example.net/healthz`.
2. Check the Rendezvous `/health` response; `iroh_relay_public_urls` must list
   the configured public origins.
3. Confirm a node logs `reconciled direct QUIC endpoint from
   rendezvous-provided iroh relays`.
4. Inspect authenticated `/control/discovery`: a `direct_quic` candidate must
   carry the endpoint ID, ALPN, `relay_url`, and a non-empty
   `relay_auth_token`.
5. Test from outside the server LAN. The route snapshot distinguishes
   `direct_quic` from `relay_tunnel`; after a connection,
   `hole_punching_mode` reports `direct`, `relay`, or `unknown`.
6. Block UDP and confirm direct HTTPS or the BerryKeep `relay_tunnel` still
   completes the same request.

Do not remove direct HTTPS or the existing relay tunnel during rollout. They
are the recovery paths for relay outages, restrictive networks, and
partially-upgraded installations.
