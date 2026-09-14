# Compatibility contracts

This ledger tracks protocol and data-format compatibility that is independent
of product naming. Add an entry when introducing a temporary reader, alias, or
wire-format fallback, and remove the entry in the same change that removes the
implementation.

| Contract | Canonical form | Compatibility handling |
| --- | --- | --- |
| Client API routes | `/api/v1/...` routes | Temporary unversioned server routes remain aliases for external callers while clients move to the versioned API. |
| Direct connection option | `--server-base-url` | `--server-url` is accepted only as a command-line compatibility alias. |
| CA override option | `--server-ca-pem-file` | The Windows `--server-ca-cert` spelling remains an input alias. |
| Enrollment labels | `device_label` | Bootstrap and enrollment payload readers accept `label` from older payloads. |
| Desktop JSON stores | Explicit `version: 1` marker | Readers accept stores that predate the version marker. |
| SQLite state caches | Explicit `schema_version` row | Readers migrate databases that predate the row. |

## Maintenance rules

- New code and release-facing documentation use the canonical form.
- Prefer a narrowly scoped reader or alias over a second write path.
- Canonical input wins when both forms are present.
- Remove the ledger row and implementation together when the support window
  closes.
