# Iroh Relay Companion Operations

Direct QUIC uses an iroh-compatible relay only for QUIC connectivity assistance.
It is not interchangeable with the BerryKeep HTTPS/WebSocket relay tunnel: that
existing tunnel remains the authenticated fallback when UDP is unavailable.

## Configure a server node

Deploy and health-check an iroh relay using its own operational runbook, then
set the same canonical URL on every server node that should advertise it:

```bash
export IRONMESH_DIRECT_QUIC_RELAY_URLS=https://iroh-relay.example.net
```

Multiple comma-separated URLs are accepted for a controlled migration, for
example `https://relay-a.example.net,https://relay-b.example.net`. Empty items
and duplicate URLs are ignored. Invalid URLs prevent only the Direct QUIC
endpoint from starting; the node and the existing relay tunnel continue to run.

At startup the node logs its Direct QUIC endpoint ID and configured relay URLs.
The same relay URL is published in its authenticated Rendezvous `direct_quic`
candidate metadata. Clients consume this metadata; mobile applications must
never embed a relay URL.

## Rollout checks

1. Check the relay's own health endpoint and UDP reachability from a network
   outside the server's LAN.
2. Confirm each node logs `server node direct QUIC endpoint started` with the
   expected `direct_quic_relay_urls` value.
3. Inspect the node's Rendezvous discovery response: the `direct_quic`
   candidate must carry the endpoint ID, ALPN, and `relay_url`.
4. Use a managed client route snapshot. It distinguishes `direct_quic` from
   `relay_tunnel`; after a Direct QUIC connection, `hole_punching_mode` reports
   `direct`, `relay`, or `unknown`.
5. Block UDP temporarily and verify that the client completes the same request
   through direct HTTPS or the existing `relay_tunnel` route.

Do not remove direct HTTPS or the existing relay tunnel as part of this rollout.
They are the recovery paths for relay outages, UDP-hostile networks, and a
partially deployed iroh relay companion.
