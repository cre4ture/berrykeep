# Retained Content Scrub and Recovery

## Contract

Scrub, availability, replication planning and garbage collection share a retained-content
catalog: current objects, **every retained version** (including history behind deletion),
and snapshot-only references. Retention policy defines reachability; the recovery worker does
not extend version history or recreate expired namespace entries.

A path/version describes why bytes are retained. The manifest hash identifies a recovery task,
and chunk hashes identify transferable bytes. Snapshot-only references use the internal
`cas-manifest:<hash>` subject rather than pretending to be a current path.

## Metadata, ownership, availability and verification

These are different facts:

- Metadata visibility says which immutable content is referenced; it does not require a full
  local replica.
- Placement assigns local durability obligations. Existing owned replicas remain protected
  during handoff. A periodic audit queues assigned gaps even if no peer advertises them.
- Availability advertises owned, locally complete retained content, including historical
  versions. A remembered historical replica claim is only a source hint, not a healthy replica.
  Cache-only content can serve a hash request but does not satisfy durable replication.
- Scrub verifies hashes as well as sizes. Pending repairs suppress availability until final
  verification succeeds. Ordinary availability refresh checks manifest integrity and chunk
  existence/size; it does not rehash the entire data set on every refresh.

Missing cache entries on an unassigned, metadata-only node are normal. Scrub records their count
as `chunks_not_required_locally`, without creating corruption findings or filling that node.
Missing chunks on an owned or assigned replica are repairable findings. Present corrupt cache
entries are checked and repaired without acquiring replica ownership.

## Recovery engine

The same chunk recovery implementation serves scrub repair, replication pulls and read-through:

1. Resolve retained metadata to the expected manifest hash. Reuse a locally valid manifest,
   otherwise request `GET /cluster/v2/replication/manifest/<hash>` from authenticated peers.
   Older peers can supply an exact-version export, but only matching manifest bytes are accepted.
2. Validate the manifest hash, structure, total size and consistent chunk sizes. Do not
   reconstruct a manifest or accept a peer-selected substitute hash.
3. Persist repair intent and chunk pins before installing repaired content.
4. Reuse each valid local chunk. Request only missing/corrupt hashes, deduplicated within the
   request, with four concurrent chunk recoveries and a ten-second timeout per peer request.
5. Try preferred/advertised sources, then remembered replicas and other online registered peers.
   A 404 or stale advertisement is not proof that another peer lacks the hash. Complementary
   partial peers can jointly recover a file even when no complete peer replica exists.
6. Verify each response's BLAKE3 hash and expected size before atomic installation. An invalid
   response does not prevent trying another source.
7. Reverify the repaired manifest and required chunks, establish ownership when appropriate,
   then remove repair intent and refresh availability.

Scrub-driven recovery changes only content bytes, ownership and repair bookkeeping. It does
**not** import peer version metadata, move a path, replace a preferred head, resurrect a deletion,
or otherwise rewrite the namespace. Normal replication continues to use its metadata import
workflow, with the shared chunk recovery engine underneath it.

## Durable scheduling and garbage collection

SQLite and Turso persist `content_repair_tasks`, keyed by manifest hash. Each task records its
retained reference, full-replica versus cache-repair intent, protected chunk identities, retry
count, retry time, cumulative recovered chunks, source-set fingerprint and last error.

Scrub persists repair intent before publishing completion, even when automatic execution is
disabled. The background worker resumes tasks after restart without another scrub or manual request.
Retries use capped exponential backoff (maximum one hour), not permanent abandonment when the
legacy transfer retry budget is exhausted. A changed online source set or peer address allows
an earlier retry; ordinary heartbeat timestamp changes do not defeat backoff.

A durable task pins its manifest and chunks. A shared GC gate serializes pin registration/release
against cleanup snapshots; network waits do not hold that gate. Already recovered chunks remain
protected through interrupted work and process restart. Final verified ownership is established
before the pin is released. Tasks whose manifest is no longer retained are discarded when
resumed, without recreating metadata.

The periodic placement audit and task resumption respect the existing automatic-repair enabled
setting. On-demand repair remains available through the existing repair workflow.

## Findings and outcomes

| Finding | Automatic action |
| --- | --- |
| Missing, unreadable, corrupt or inconsistent manifest | Fetch bytes matching the metadata's manifest hash; never invent replacement metadata |
| Missing required chunk, unreadable chunk, size or hash mismatch | Recover that hash from any contributing authenticated peer |
| Missing unassigned cache entry | No repair; count as not required locally |
| Manifest/path disagreement | Detect only; namespace reconciliation needs separate authority |

Scrub history remains `issues_detected` when it discovered defects, even if a follow-on task
repairs them. Repair history reports the outcome of the attempt:

- `completed`: all work in that attempt succeeded;
- `partially_repaired`: progress was made but work remains;
- `waiting_for_source`: recovery is queued or no peer supplied valid bytes;
- `unresolved`: local installation/verification failed or work could not be resolved;
- `skipped_no_gaps`: no repair was necessary.

Detailed repair logs include manifest identity, recovered chunk counts, verification timestamps,
and the next retry time. Pending work is not advertised as a healthy replica. If no valid copy
exists anywhere, the task stays visible and retries; no algorithm can reconstruct arbitrary lost
bytes without another copy or independent redundancy.

## Regression coverage

Storage and runtime tests run on both metadata backends:

- retained historical availability after deletion, using the real production inspector;
- assigned versus metadata-only missing chunks, and no eager cache hydration;
- snapshot-only reachability after version-index removal;
- complementary partial peers, absent/stale availability and hash-level fallback;
- invalid peer bytes, request deduplication and reuse of already fetched chunks;
- partial recovery, restart, peer reconnect and retry-budget exhaustion;
- cancellation while a request is in flight, batch limits preserving all queued intent,
  and local installation failures distinct from missing sources;
- concurrent GC, durable pins, verification and ownership-before-unpin ordering;
- automatic discovery of an assigned gap with no advertised source;
- stale historical claims and exportable metadata not counting as complete replicas;
- existing scrub corruption classes and exact historical repair without namespace changes.

No automatic metadata reconciliation, manual production repair, or rollout is part of this change.
