# Global Rendezvous Operations Checklist

Status: Phase-2 operating contract. The global service API, server-node
registration flow, and end-to-end coverage are implemented on this branch;
they become available on `main` after review and merge.

This checklist applies to an Internet-facing global rendezvous/relay operating
with Option 1. Its security boundary is defined in
`docs/security-architecture.md`: the service handles outer metadata and an
encrypted inner TLS stream, not application payloads or inner TLS termination.

## Service setup

- [ ] Deploy the service behind HTTPS with a valid service certificate and
  private key. Keep this service identity separate from all registered cluster
  CAs and their private keys.
- [ ] Put the registry on durable storage and set
  `BERRYKEEP_RENDEZVOUS_GLOBAL_CLUSTER_REGISTRY` to its persistent path. The
  parent directory must already exist, the path must not contain `..`, and the
  process account must have only the permissions required to read and update
  that path.
- [ ] Back up the registry and audit records on a schedule, protect backup
  access, and test restoration before enabling public registration.
- [ ] Set `BERRYKEEP_RENDEZVOUS_GLOBAL_REGISTRATION_ENABLED=false` until the
  HTTPS endpoint, persistence, audit logging, and abuse controls are verified.

## Phase-2 configuration contract

- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_CLUSTER_REGISTRY` names the persistent
  cluster registry path.
- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_REGISTRATION_ENABLED` is the global,
  service-side self-registration gate.
- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_ADMIN_TOKEN` protects global registry
  administration, including suspend and resume. Store it in secret storage;
  never log it or place it in a command line.
- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_REGISTRATION_RATE_LIMIT_PER_MINUTE` sets the
  registration attempt limit. Set an explicit conservative value and alert on
  repeated rate-limit hits.
- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_CHALLENGE_TTL_SECS` sets proof challenge
  lifetime. Set an explicit short operational value and reject expired or
  reused challenges.
- [ ] `BERRYKEEP_RENDEZVOUS_GLOBAL_MAX_PENDING_CHALLENGES` bounds in-memory
  outstanding registration challenges. Set an explicit conservative value and
  alert when the service rejects registrations because the bound is exhausted.
- [ ] `BERRYKEEP_GLOBAL_RENDEZVOUS_REGISTRATION_ENABLED` is the server-node
  opt-in. Enable it only on nodes intended to self-register and only after the
  service-side gate is enabled.

These names are the Phase-2 contract. The service and node must not silently
substitute alternate names or treat node opt-in as permission to bypass the
global service gate.

## Per-service registration policy

The Server Admin Control Plane page stores a **Global rendezvous service**
setting beside each operator-managed URL and each cluster contact. Enable it
only for a service that accepts self-registration of independent clusters.
The service-side registration gate must also be enabled.

The node configuration API accepts `editable_services`, and the replicated
contact configuration accepts `rendezvous_services`. Each entry contains:

```json
{
  "url": "https://rendezvous.example.com/",
  "global_registration_enabled": true
}
```

Node-local entries are persisted in the node enrollment package. Cluster
contacts carry their policy in the versioned contact object and replicate with
it. A node-local entry takes precedence when the same URL is also a cluster
contact. The embedded managed rendezvous always remains cluster-specific.

The URL-only API fields remain available for existing clients and bootstraps.
Old entries default to cluster-specific. For startup URLs without an explicit
per-service policy, `BERRYKEEP_GLOBAL_RENDEZVOUS_REGISTRATION_ENABLED=true`
retains its legacy meaning of enabling global registration. An explicit
per-service setting overrides that default, including `false`; selecting a
global service in the UI does not require setting this environment variable.

Enabling a new global entry requires HTTPS, active rendezvous participation,
rendezvous mTLS, a readable and valid internal TLS client identity, and the
service's trusted CA material.
Retaining a replicated global entry does not require changing a node's legacy
local mTLS switch. Issued node bootstraps require mTLS whenever they include a
global service.

The rendezvous trust setting is node-wide and accepts a concatenated PEM CA
bundle with the trust roots for all configured services. When private and global
services use independent CAs, include both roots. Configure a dedicated bundle
file using `BERRYKEEP_RENDEZVOUS_CA_CERT`, or supply the bundle in the enrollment
package's `trust_roots.rendezvous_ca_pem`. Keep the cluster identity CA file
separate; adding service trust roots must not change the cluster ownership CA.
Selecting a global role does not automatically install that service's CA.

Nodes first publish presence using their existing cluster identity. If the
service rejects presence, a node holding the cluster CA signing key and an
internal TLS identity can perform the ownership proof, then retry presence.
Other nodes keep attempting ordinary presence. Changes apply live; failures
are reported and retried per endpoint without blocking other services or node
startup. Healthy presence refreshes a 60-second grace period against repeated
proofs. Persistent failures after that period can recover a lost registry entry;
failed proofs back off from 5 seconds to a maximum of 5 minutes. Enrollment
requests keep their own timeout budget, separate from presence requests, and
read current trust material for each proof attempt. This recovery does not
replace an existing cluster CA: Option 1 still rejects cluster CA rotation.

## Registry and admission

- [ ] Enforce exactly one active P-256 CA per `cluster_id`. The cluster SAN on
  a presented certificate selects that CA alone; never fall back to trying all
  registered CAs.
- [ ] Require the versioned canonical challenge proof for a new registration.
  A valid repeat with the same CA fingerprint is idempotent. Reject a different
  CA for an existing `cluster_id`; Option 1 has no CA rotation or CA rewrite.
- [ ] Record accepted, rejected, rate-limited, suspended, and resumed actions
  with timestamp, actor or source, cluster id when known, and reason. Do not
  log tokens, private keys, or proof signatures.

## Monitoring and response

- [ ] Monitor HTTPS certificate expiry, service availability, registry load or
  write failures, registration outcomes, proof failures, rate-limit events,
  and suspended-cluster activity.
- [ ] Alert on repeated invalid proofs, unexpected registration volume,
  registry persistence failures, admin-token failures, and relay or
  authentication errors that could indicate cross-tenant access attempts.
- [ ] Suspend a suspected cluster through the authenticated global admin
  control, retain the audit reason and timestamp, and verify that the service
  enforces the suspension before resuming it. Resume only after the incident
  review is recorded.
- [ ] For an active incident, first disable new registration with
  `BERRYKEEP_RENDEZVOUS_GLOBAL_REGISTRATION_ENABLED=false`, suspend affected
  clusters, preserve registry and audit snapshots, rotate exposed service or
  admin secrets, and restore only from a verified registry backup if recovery
  is required.

See `docs/global-rendezvous-relay-e2e-plan.md` for the delivery status and
protocol rules, and `docs/nat-traversal-implementation-checklist.md` for the
repository-wide implementation status.
