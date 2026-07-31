# Cluster Configuration Strategy

## Purpose

IronMesh needs a small set of cluster-owned settings that every server node can
resolve consistently. The first use case is a rendezvous contact list that an
authenticated client can download after reaching any node, directly or through
a relay. Further candidates include replication policy and selected UI/runtime
settings.

This document chooses an object-backed approach so that cluster configuration
does not grow a second independent replication subsystem beside the existing
versioned object store.

## Existing foundations

Two existing mechanisms inform this strategy:

- The normal object store has confirmed object versions, a version graph,
  metadata synchronization across online nodes, snapshots, and ordinary data
  replication.
- The gallery map configuration is already a cluster-replicated configuration
  object at `sys/maps/gallery-map-config.json`.
- The S3 control plane has its own persistent fan-out/pull implementation.
  It is useful operationally, but its local `generation` counter is not a
  persistent cluster-wide revision and it should not be copied for each new
  configuration domain.

Object metadata synchronization is intentionally independent of the normal
replication factor: online nodes exchange known object/version subjects and
import the corresponding version graph and manifests. Object chunks continue
to follow ordinary data replication.

## Target model

Cluster settings are separate, typed documents stored under a reserved system
namespace. They share object storage and replication, but not one mutable
catch-all JSON document.

```text
sys/cluster-config/
├── rendezvous-contacts.json
├── replication-policy.json        # future
├── gallery-map-config.json        # may move here in a later migration
└── ...
```

Each document has:

- a stable key and an independently evolving schema version;
- a typed server-side model and validation, rather than arbitrary JSON;
- confirmed object versions; the storage `version_id` is the cache and change
  token for clients;
- an explicit access class: authenticated client-readable, admin-only, or
  node-internal;
- an explicit apply mode: immediately reloadable or restart-required.

Documents are replaced atomically. The object version graph preserves a
history and makes an accidental concurrent write visible as a branch instead
of silently discarding it. Cluster operators coordinate administrative edits;
this strategy does not add leader election or a monotonically increasing
global revision counter.

## Rendezvous contact list

The first document is
`sys/cluster-config/rendezvous-contacts.json`:

```json
{
  "schema_version": 1,
  "rendezvous_urls": [
    "https://rendezvous.example:19080",
    "https://fallback.example:19080"
  ]
}
```

It is written through an admin-only endpoint and exposed through an
authenticated client endpoint. A client may retrieve it over either a direct
node connection or the existing relay transport; both terminate at the same
client API. The list is deliberately separate from a node's local listener,
TLS, and bootstrap configuration. Those settings must exist before a client
can make its first authenticated connection.

Clients retain their original bootstrap contacts as recovery anchors. The
managed client refreshes this document only after it has a signed client
identity and can call the authenticated cluster API. It accepts the response
over either direct or relay transport, validates `schema_version`, and stores
the returned `version_id` with the contact URLs. At the next startup, cached
contacts are attempted before the immutable bootstrap contacts; the latter are
never replaced.

A missing object (`stored: false`) is deliberately not treated as an empty
update, since the selected node may not have received the replicated metadata
yet. An explicitly stored empty list is a valid update and leaves the client
with only its bootstrap recovery contacts. The managed SDK offers a durable
persistence callback and a pending-bootstrap-update API for platform-owned
storage. The CLI bootstrap file and Android preferences use the callback; the
iOS FFI exposes the update for the Swift/App Group owner to persist.

## Implemented reference scope

The reference implementation now includes:

1. typed, validated object content;
2. confirmed object writes from the Admin Web UI/API;
3. registration with the existing object metadata and ordinary replication
   path;
4. authenticated direct and relay client reads;
5. display of the stored object version in the Admin Web UI.
6. managed-client retrieval, versioned local persistence, and reuse on later
   starts while preserving immutable bootstrap fallbacks.

It intentionally does **not** add a new synchronization loop, control-plane
priority, all-node chunk fan-out, or dynamic reconfiguration of the standalone
rendezvous service.

## Future delivery priority

Ordinary object replication may be delayed by large transfers and normally
uses the configured replication factor. Cluster configuration needs a stronger
delivery policy, but it should be added once for all configuration documents,
not as a rendezvous-only exception.

The proposed policy classes are:

| Class | Intended contents | Delivery behaviour |
| --- | --- | --- |
| `control-plane` | small configuration documents and their manifests | metadata first, immediate all-online-node fan-out, persistent retry and per-node acknowledgement |
| `standard` | ordinary user objects | normal replication-factor placement and repair |
| `bulk` | large map packages and other deferrable artifacts | normal placement with lower scheduling priority |

The scheduler should give `control-plane` work its own bounded worker lane so
a currently running multi-gigabyte transfer cannot block a configuration
update. A size limit and a registered-document allow-list prevent the priority
class from being abused for arbitrary data.

## Security and configuration boundaries

Not every setting belongs in client-readable object storage.

- Rendezvous contacts and replication policy can be ordinary typed cluster
  configuration documents, with endpoint-specific authorization.
- The admin password is stored only as a password verifier today, but it is
  still security-sensitive. Before it is cluster-synchronized, the shared
  configuration layer needs a node-internal sealed-record facility. The
  verifier must never be returned by a read API or put into audit details.
- Node bind addresses, private keys, certificate files, disk paths, and
  hardware-specific controls remain node-local. They are startup or host
  configuration, not cluster-wide desired state.

## Migration approach

1. Establish the rendezvous document as the reference implementation.
2. Add a small registry describing document key, schema, access class, apply
   mode, and later delivery class. Do not introduce a generic unvalidated JSON
   endpoint.
3. Move non-secret replication policy to its own document once its runtime
   reload/restart semantics are explicit.
4. Add sealed node-internal records before migrating the admin-password
   verifier or S3 access-key material.
5. Evaluate migration of the S3 control-plane state after the generic path has
   proven equivalent operational behaviour.
