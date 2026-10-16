# Optional multimedia on server nodes

Each server node has an independent **Hardware → Multimedia functionality** setting.
Disable it on constrained hosts to dedicate their resources to object storage,
replication, automatic repair, and scrubbing. The default is enabled, preserving
the behavior of existing nodes.

Connection priority and multimedia eligibility solve different problems. Priority
is a soft client routing preference; even a low-priority node may receive requests.
Disabling multimedia is enforced by the server. A client priority override cannot
enable it again.

## Configuration

The authenticated node-local admin API uses the same configuration pattern as
connection priority:

```text
GET /api/v1/auth/node-multimedia
PUT /api/v1/auth/node-multimedia
Content-Type: application/json

{"enabled": false}
```

An enrollment-based node writes the setting to its enrollment package. Other
nodes report `persistence_source: "runtime_only"`; configure their startup source
to retain the setting after a restart:

- Environment-based startup: `BERRYKEEP_NODE_MULTIMEDIA_ENABLED=false`.
- Bootstrap or enrollment JSON: set `bootstrap.labels.multimedia_enabled` to
  the string `"false"` (or `labels.multimedia_enabled` in a standalone bootstrap).

Both accept `true` or `false`; an empty environment value is treated as unset.
A missing label means enabled. The label is
advertised with the node descriptor and Rendezvous presence, independently of
`client_connection_priority`.

## Disabled behavior

- No startup media metadata backfill or media warmup after uploads.
- Media workers reject new metadata extraction and thumbnail generation, even
  when a caller retained a worker before the setting changed.
- Thumbnail, gallery map, map serving/configuration/import, and multimedia
  operation start endpoints return HTTP 503 with code `node_multimedia_disabled`.
  This applies to versioned and legacy HTTP endpoints and multiplexed transports.
- Cache clearing and operation history remain available. Clearing the cache does
  not restart multimedia backfill while multimedia is disabled.
- Original media and map datasets remain ordinary replicated objects. File
  upload/download, directory browsing, replication, repair, and scrubbing continue.
  Directory listings may still expose metadata already cached on disk.

The client SDK already retries HTTP 503 on another route. Configure at least one
reachable node with multimedia enabled for convenience features. A browser using
a node's admin UI directly must connect to an enabled node to use those features.
This release does not add capability-based client route scoring; a disabled node
may receive an initial request before failover. Priority can reduce those attempts.

When an enabled node has only metadata and a known storage-only source holds the original,
it first tries other enabled peers for an existing derivative. If none is available,
it uses the existing bounded chunk-recovery path to cache the original chunks in
its storage pool and derive metadata/thumbnails locally. Storage-only peers serve
ordinary chunks without decoding media. The capable node therefore needs storage
capacity for recovered source content as well as derivative caches. Existing
incomplete-artifact retry backoff still applies when recovery cannot complete.
Unrelated storage-only nodes do not trigger recovery for other nodes' media.

Changes affect new work immediately. Work already admitted may finish; the switch
does not forcibly terminate a decoder or map import. Backfill and operation batches
stop at their next eligibility check. Re-enabling starts metadata backfill and
resumes a pending map import.

## Memory isolation

Disabling multimedia removes its decode and map-serving memory demand from a weak
node. It does not bound memory used by storage, concurrent transfers, or other
processes on the host, and cannot guarantee that the operating system never kills
the service.

Image decoding currently runs inside the server process; video tools are child
processes. Decoder limits and media build admission reduce peaks, but are not an
operating-system memory boundary.

A future isolation layer should move optional processing behind a bounded job
interface into a separately supervised worker. On Linux the worker needs its own
cgroup/service with a memory limit, CPU/concurrency limits, timeouts, and bounded
IPC results. The storage service should retain ownership of the database and
publish validated derivatives atomically. Worker failure must fail only the
optional request, with bounded retries. Map queries/imports and media extraction
both need coverage; moving only image codecs leaves other optional allocations
inside storage.

Merely spawning another process in the same service does not provide this
boundary. A cgroup memory limit contains cgroup-local OOM kills, while host-wide
memory exhaustion can still affect other services. See the
[Linux cgroup v2 memory controller documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory).
Worker process isolation is not implemented by this setting.
