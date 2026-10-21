# Default global rendezvous discovery

Status: proposed catalog and consumer contract. This change publishes the data
format first; the initial-setup lookup and opt-out UI are a separate change.
The catalog becomes reachable on `main` after this PR is merged.

## Stable lookup location

The first-node setup implementation should compile in this catalog URL:

```text
https://raw.githubusercontent.com/cre4ture/berrykeep/main/discovery/global-rendezvous.json
```

The compiled constant identifies the catalog, not an individual rendezvous
host. Maintainers can update service addresses through a normal repository PR
without rebuilding every Server Node. The matching schema is
[`discovery/global-rendezvous.schema.json`](../discovery/global-rendezvous.schema.json).
Its `$schema` hint is for tooling; consumers validate the supported format
locally rather than downloading or executing schemas.

## Catalog format

```json
{
  "$schema": "https://raw.githubusercontent.com/cre4ture/berrykeep/main/discovery/global-rendezvous.schema.json",
  "schema_version": 1,
  "rendezvous_services": [
    {
      "id": "berrykeep-default",
      "name": "BerryKeep Global Rendezvous",
      "url": "https://rendezvous.example.com",
      "global_registration_enabled": true
    }
  ]
}
```

The example hostname is illustrative and must not be published as an active
default. The initial real catalog deliberately contains an empty list until
an operating service and its public HTTPS address have been confirmed.

- `schema_version` is the integer `1`. Unsupported versions are rejected.
- `rendezvous_services` contains zero to sixteen entries in display order.
  An empty list is valid and means that no default service is advertised.
- `id` is a stable lowercase identifier with optional hyphen-separated parts.
  IDs must be unique within a catalog; they are not cluster IDs.
- `name` is the service label shown during initial setup.
- `url` is the service's absolute HTTPS origin. A port and an optional root
  slash are allowed. Path prefixes, userinfo, query strings and fragments are
  forbidden in v1. A host and, when supplied, a valid port are required.
  Canonically equivalent URLs must not appear twice.
- `global_registration_enabled` must be `true`. It maps directly to the
  corresponding per-service registration policy when the service is selected.

Catalog validation also rejects duplicate IDs and canonical URLs, unknown
fields, and malformed entries. The schema covers field shapes; consumers must
add the cross-entry uniqueness and parsed-host/port checks. This document and
the schema form the v1 contract; format changes require a new schema version.

## Initial-setup consumer contract

1. Offer **Use the default global rendezvous services** enabled by default
   when creating a new cluster on its first node. The user can opt out before
   submitting setup. Opt-out skips both the catalog request and service
   registration; local/private cluster setup remains available.
2. If selected, fetch the compiled-in URL over verified HTTPS when setup is
   submitted. Use a bounded request timeout of five seconds and a maximum
   response size of 64 KiB. Reject redirects to another origin and validate
   the entire catalog before using any entry.
3. An unavailable catalog, including HTTP 404 before publication, an empty
   catalog, or an invalid response must not prevent local/private setup.
   Report the lookup result and let the user retry, opt out, or configure a
   service manually. Do not substitute an unverified hardcoded service host.
4. Selected services require publicly trusted HTTPS certificates. Catalog v1
   does not distribute replacement trust roots or disable TLS verification.
   The consumer implementation must first add public-root trust for
   catalog-selected services to the global-registration HTTP client: the current
   client requires an explicit service CA and disables built-in roots. Keep that
   explicit-CA mode for manually configured private services. Merely copying a
   catalog URL into the existing client is insufficient.
   Cluster identities and CA ownership proofs remain part of the existing
   authenticated global-registration protocol; the catalog contains no
   credentials, tokens, cluster private keys, or executable instructions.
5. Persist the selected URLs and `global_registration_enabled` policy in the
   cluster's rendezvous contact configuration, with the usual per-entry admin
   controls. Persist the initial-setup choice so a restart or setup recovery
   does not turn an opt-out back on. The cluster's signing node performs
   registration; other nodes use their existing cluster identities.
6. Joining an existing cluster uses that cluster's established configuration.
   Restarts, recovery, and normal operation must not re-fetch defaults or
   overwrite existing contacts. Subsequent catalog edits affect future
   cluster creation, not a previously configured cluster.

The per-service configuration implementation and this catalog can be reviewed
independently. Merge the catalog first, verify the raw URL returns valid JSON,
then ship the setup consumer. No running deployment changes when this catalog
PR is merged.

## Repository validation

The `Rendezvous catalog` workflow validates catalog changes on pull requests
and pushes to `main`. It checks the JSON Schema, host and port validity, unique
service IDs, canonical origin uniqueness, and duplicate JSON object keys. The
same validator runs offline locally:

```bash
python3 -m pip install jsonschema==4.26.0
python3 -m unittest discover -s scripts -p test_validate_global_rendezvous_catalog.py -v
python3 scripts/validate_global_rendezvous_catalog.py
```

Canonical origin comparison normalizes host case, IPv6 spelling, an omitted
HTTPS port versus port 443, a terminal DNS dot, and an optional root slash.
The setup consumer must enforce these same checks; CI validation complements
runtime validation rather than replacing it.
