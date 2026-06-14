# Facet Integration Strategy

## Status

Proposal only.

This note evaluates how [`ncoevoet/facet`](https://github.com/ncoevoet/facet)
can fit into IronMesh and turns the earlier feasibility check into a concrete
integration plan.

Facet repo state inspected for this note:

- branch: `master`
- commit: `772a80a`

Key upstream files inspected:

- `README.md`
- `db/schema.py`
- `db/connection.py`
- `api/database.py`
- `api/routers/search.py`
- `api/routers/thumbnails.py`
- `api/path_validation.py`
- `storage/__init__.py`
- `docs/CONFIGURATION.md`
- `docs/DEPLOYMENT.md`

Relevant IronMesh context:

- `docs/persistent-storage-strategy.md`
- `docs/server-node-media-cache.md`
- `docs/gallery-map-view-design-note.md`

Related follow-up implementation note:

- `docs/facet-thumbnail-backend-swap-plan.md`

## Decision Summary

Facet should be integrated with IronMesh as a sidecar application first.

Recommended shape:

1. IronMesh remains the system of record for original photo files.
2. A dedicated Facet worker node keeps the target photo library locally
   available through IronMesh sync or a fully hydrated local mirror.
3. Facet scans those local files and keeps its live SQLite database and
   derivative storage on local disk outside the synced IronMesh namespace.
4. IronMesh optionally imports a small, path-independent summary of Facet
   outputs keyed by `content_fingerprint`.

Do not attempt to make Facet use the IronMesh object store directly in the
first iteration.

## Why The Direct Integration Is Expensive

IronMesh is object- and version-centric:

- content-addressed chunk storage,
- immutable manifests,
- snapshot/version reads,
- media cache keyed by `content_fingerprint`.

Facet is path- and filesystem-centric:

- `photos.path` is the primary key,
- many tables reference path text directly,
- the scanner repeatedly reopens original files from disk,
- the viewer serves originals from local disk paths,
- the app expects a local SQLite database with WAL and SQLite-specific
  extensions.

Those are compatible through a local filesystem view, but they do not line up
cleanly enough for a direct backend swap.

## What Facet Needs

### Data-access needs

| Need | What Facet expects | IronMesh implication |
| --- | --- | --- |
| Original photo access | Real local files that can be opened repeatedly by path | A Facet worker needs a local replica or a fully hydrated mirror, not just remote object reads |
| Stable lookup key | Path strings like `/photos/2025/trip/img_001.cr3` | IronMesh must present a stable local path view for the worker |
| Recursive scanning | Directory walks over mounted/local trees | Best fit is a synced directory or materialized mount |
| RAW decode | Direct byte access to CR2/CR3/NEF/ARW/etc. | Placeholder-only access is a poor fit unless files are hydrated first |
| EXIF extraction | `exiftool` over local paths | Paths must resolve to actual files on disk |
| Full image serving | Viewer resolves DB path to a local file and opens it | The viewer host must see the same files locally or through configured path mapping |
| Rename/copy visibility | Path changes are meaningful application events | Path churn should be minimized or reconciled during rescans |

### Database needs

Facet expects a writable local SQLite database with:

- WAL mode,
- `busy_timeout`,
- `mmap_size`,
- SQLite foreign keys,
- FTS5,
- optional `sqlite-vec` loadable extension,
- large BLOB support,
- frequent read/write access from the app process.

Important consequence:

- the live Facet database should not be stored as ordinary synchronized content
  inside IronMesh.

Reasons:

- live SQLite WAL databases are single-system application state,
- they change at high frequency,
- they are not a good semantic match for IronMesh's file/object replication
  model,
- accidental multi-node sharing of the same live DB would be fragile.

### Derivative storage needs

Facet stores or can store:

- photo thumbnails,
- CLIP or SigLIP embeddings,
- face embeddings,
- face thumbnails,
- histograms,
- captions,
- tags,
- FTS index tables,
- optional `sqlite-vec` vector table,
- albums and share tokens,
- per-user preferences,
- pairwise comparison and learned-score data,
- scan run bookkeeping.

Storage shape upstream today:

- default: in SQLite BLOB columns,
- optional alternate mode: filesystem storage for thumbnails and embeddings
  under `storage/`.

### Operational needs

Facet assumes these are local to the worker host:

- Python runtime and ML dependencies,
- `exiftool`,
- optional GPU access,
- optional `rawpy` / ONNX / PyTorch acceleration,
- optional `sqlite-vec`.

## Recommended IronMesh Architecture

### 1. Topology

Use one designated Facet worker for each library scope.

Example layout:

- IronMesh cluster stores original photos under logical prefixes like
  `photos/family/` or `photos/archive/`.
- One worker node keeps those prefixes locally available.
- Facet runs only on that worker.
- Facet's live state lives under a worker-local directory such as:
  - `/var/lib/facet/photo_scores_pro.db`
  - `/var/lib/facet/storage/`

### 2. Local file source for Facet

Preferred source for the Facet scanner:

- an IronMesh-managed local replica of the target tree.

Acceptable variants:

- a sync-agent-managed local mirror,
- a FUSE mount that is pre-hydrated into local files before scanning,
- a Windows/macOS native sync root if the data is fully materialized.

Not recommended for initial production use:

- scanning a purely lazy placeholder tree where large numbers of RAW files
  would hydrate on demand during scoring.

Facet's workload is bulk sequential analysis, so it benefits from:

- local byte availability,
- predictable directory traversal,
- low-latency reopen of the same files.

### 3. Path contract

The Facet worker should treat the local replica path as canonical for a given
library.

Example:

- IronMesh logical key: `photos/family/2026/trip/img_001.cr3`
- local worker path: `/srv/ironmesh-facet/photos/family/2026/trip/img_001.cr3`
- Facet `photos.path`: `/srv/ironmesh-facet/photos/family/2026/trip/img_001.cr3`

That keeps upstream Facet behavior intact.

## Sync And Scan Flow

### Phase A: file availability

1. IronMesh replicates the target prefixes to the Facet worker.
2. The worker ensures the target files are locally available before scan.
3. A scan job starts only after the local mirror is in a stable state.

### Phase B: Facet processing

1. Facet walks the local tree.
2. Facet extracts EXIF and image metadata from local files.
3. Facet computes thumbnails, embeddings, scores, tags, faces, and optional
   captions.
4. Facet writes all live application state to its local SQLite database and
   optional local storage directory.

### Phase C: optional export back into IronMesh

1. An export task reads selected Facet results.
2. The exporter resolves each Facet photo path back to the current IronMesh key
   and current `content_fingerprint`.
3. The exporter produces path-independent annotation records.
4. IronMesh imports those records into a dedicated annotation store keyed by
   `content_fingerprint`.

## What IronMesh Should Import

Do not try to import all of Facet's relational state into IronMesh at first.

Recommended first import set:

- aggregate score,
- per-dimension scores needed for gallery ranking or filtering,
- category,
- tags,
- caption,
- face count,
- blink flag,
- burst/duplicate summary flags,
- optional lightweight person summary,
- provenance metadata saying the record came from Facet.

Do not import initially:

- full CLIP/SigLIP embeddings,
- full face embeddings,
- Facet albums,
- Facet share tokens,
- pairwise comparison history,
- weight-optimization history,
- all Facet-specific cache internals.

Those remain Facet-local application data.

## Why Import By `content_fingerprint`

IronMesh already uses `content_fingerprint` as the path-independent media cache
identity.

That gives the right deduplication behavior for imported annotations:

- rename should not duplicate metadata,
- copies of identical bytes can reuse derived annotations,
- re-uploads of identical bytes can reuse prior annotation records.

The export/import bridge should still record the observed key and version used
at export time, but the durable lookup key should be `content_fingerprint`.

## Proposed Annotation Shapes

### 1. Summary store inside IronMesh metadata

Add a dedicated annotation table or equivalent metadata store keyed by:

- `content_fingerprint`
- `source`
- `schema_version`

Example summary payload:

```json
{
  "source": "facet",
  "schema_version": 1,
  "content_fingerprint": "cfp-...",
  "observed_key": "photos/family/2026/trip/img_001.cr3",
  "observed_version_id": "ver-123",
  "aggregate": 8.74,
  "category": "landscape",
  "tags": ["sunset", "mountain", "lake"],
  "caption": "A mountain lake at sunset.",
  "face_count": 0,
  "is_blink": false,
  "is_burst_lead": true,
  "is_duplicate_lead": true,
  "updated_at_unix": 1781184000
}
```

This record is intentionally thin enough to support:

- gallery ranking,
- filter pills,
- search facets,
- future annotation badges in the UI.

### 2. Optional raw Facet export objects

If the project later wants durable, replicated exports of richer Facet output,
store them as versioned objects under a system prefix such as:

- `sys/media-annotations/facet/<library-id>/<content_fingerprint>.json`

That is useful for:

- auditability,
- re-import,
- offline cluster propagation,
- keeping the live Facet DB out of the replicated namespace.

This export object should still be a derived artifact, not the live upstream DB.

## Interaction With Existing IronMesh Media Cache

The current media cache already covers:

- MIME/media type,
- dimensions,
- orientation,
- capture time,
- GPS,
- one thumbnail profile.

Facet-derived annotation data should not be shoved into the existing media cache
record as an unbounded blob.

Better shape:

- keep the current media cache focused on lightweight, decoder-derived media
  facts,
- add a separate annotation store for higher-level analysis products.

The web layer can then compose:

- media cache data,
- annotation summary data,
- ordinary store index data.

## Backup And Durability Guidance

For the Facet worker:

- keep the live DB on local disk,
- back it up with a SQLite-aware tool or local snapshot process,
- optionally export derived annotation objects into IronMesh for replicated
  durability.

Do not rely on synchronizing the live `photo_scores_pro.db` through IronMesh as
the primary durability mechanism.

## What To Avoid

Avoid these designs in the first iteration:

- storing the live Facet SQLite DB in the shared IronMesh namespace,
- running multiple Facet writers against the same DB,
- trying to map IronMesh object IDs directly into Facet without a filesystem
  view,
- pushing full vector embeddings into IronMesh before a concrete search use case
  exists,
- scanning large placeholder-only trees that hydrate file-by-file during
  scoring.

## Phased Implementation Plan

### Phase 1: sidecar-only integration

Goal:

- prove that Facet adds value on top of IronMesh-managed files.

Work:

1. Stand up a dedicated Facet worker against an IronMesh-managed local mirror.
2. Keep Facet DB and `storage/` local-only.
3. Document the worker deployment contract.
4. Do not import metadata back into IronMesh yet.

Success criteria:

- Facet scans and serves the library reliably,
- RAW decode and EXIF extraction perform well enough,
- no dependency on direct IronMesh object reads inside Facet.

### Phase 2: summary import into IronMesh

Goal:

- let IronMesh surface Facet value in its own gallery.

Work:

1. Build an exporter from Facet DB rows to annotation summary records.
2. Resolve each exported row to current IronMesh `content_fingerprint`.
3. Add a dedicated server-side annotation metadata store.
4. Extend `GET /store/index` or a sibling endpoint to expose imported summary
   fields.

Success criteria:

- IronMesh gallery can filter or rank on imported Facet summaries,
- rename/copy do not create redundant imported records.

### Phase 3: richer UI composition

Goal:

- use Facet as an analysis engine while IronMesh remains the primary storage
  and browsing product.

Work:

1. Show imported badges, scores, and tags in the IronMesh gallery.
2. Add server-side selection/filter support over imported summaries.
3. Decide whether any Facet-local concepts should become native IronMesh
   features.

Candidates:

- top-picks filter,
- richer quality ranking,
- caption overlay,
- duplicate or burst hints.

### Phase 4: evaluate deeper integration only if still justified

Possible future work:

- native IronMesh annotation pipelines,
- direct annotation jobs on server nodes,
- clustered distribution of heavier ML work,
- richer person/faces integration.

This phase should only start if the sidecar path proves product value first.

## Recommendation

The right first move is:

- treat Facet as a local analysis engine attached to an IronMesh-managed photo
  replica,
- keep Facet's live database and heavy derived state local,
- import only path-independent summaries back into IronMesh,
- delay any deeper storage-model unification until the sidecar path has proven
  worth the complexity.
