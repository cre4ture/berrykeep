# Facet Thumbnail Backend Swap Plan

## Status

Proposal only.

This note turns the high-level Facet integration strategy into a concrete plan
for a full thumbnail-backend swap across both repositories.

Target outcome:

- Facet no longer treats its own `photos.thumbnail` BLOB as the authoritative
  photo thumbnail source.
- IronMesh becomes the authoritative provider for shared photo thumbnails.
- IronMesh exposes a second, higher-resolution thumbnail profile for analysis
  workloads in addition to the current `grid` profile.
- Facet keeps local fallbacks only where they remain necessary during migration
  or for unsupported/offline cases.

This plan does **not** attempt to replace Facet's full photo-file access model.
Facet still expects local originals for scanning, RAW decode, full-image
viewing, EXIF, and several model pipelines.

## Current Baseline

### IronMesh today

Current media-cache behavior:

- one thumbnail profile only: `grid`
- `grid` max dimension: `256`
- thumbnail artifacts stored at
  `state/media_cache/thumbnails/<content_fingerprint>/<profile>.jpg`
- thumbnail metadata embedded in `CachedMediaMetadata`
- `/media/thumbnail` returns the generated JPEG payload

Relevant code:

- `crates/server-node-sdk/src/storage/media_cache.rs`
- `crates/server-node-sdk/src/lib.rs`
- `docs/server-node-media-cache.md`

### Facet today

Facet uses thumbnails in three different roles:

1. Viewer display
2. Derived-analysis input
3. Face-crop fallback source

Important current coupling:

- the `photos` table has a `thumbnail BLOB` column
- the scorer writes generated `640px` JPEG thumbnails directly into SQLite
- the thumbnail API reads `photos.thumbnail` directly
- several recompute commands decode thumbnails from SQLite instead of refetching
  originals
- face-thumbnail fallback logic crops from the stored photo thumbnail
- export/maintenance tasks downsize or migrate stored thumbnails

Relevant code:

- `/tmp/facet/db/schema.py`
- `/tmp/facet/processing/scorer.py`
- `/tmp/facet/api/routers/thumbnails.py`
- `/tmp/facet/facet.py`
- `/tmp/facet/db/maintenance.py`

## Scope

In scope:

- add an IronMesh `analysis` thumbnail profile
- add profile-aware thumbnail requests and metadata
- create a Facet thumbnail backend abstraction that supports IronMesh-backed
  fetches
- introduce path/key mapping between Facet paths and IronMesh keys
- migrate Facet viewer and recompute code off direct `photos.thumbnail` reads
- update export code that currently assumes local thumbnail BLOBs
- define cutover, fallback, and rollback behavior

Out of scope for this plan:

- replacing Facet's full local-file access for originals
- replacing face embeddings, CLIP embeddings, or other non-thumbnail BLOBs
- moving Facet's live SQLite database into IronMesh
- importing all Facet relational state into IronMesh

## Assumptions

1. A dedicated Facet worker continues to exist.
2. The worker has local access to original photo files for full scans and RAW
   decode.
3. IronMesh and Facet can both run on the same machine or at least inside the
   same low-latency environment for thumbnail fetches.
4. The migration may require coordinated changes in both repos before a clean
   cutover is possible.
5. During migration, dual-read or dual-write behavior is acceptable.

## Success Criteria

The migration is complete when all of these are true:

1. Facet gallery thumbnails are fetched from IronMesh by default.
2. Facet analysis paths that currently rely on `photos.thumbnail` can instead
   consume IronMesh `analysis` thumbnails with no material quality regression.
3. Path rename or copy events do not duplicate thumbnails unnecessarily because
   IronMesh thumbnail storage remains keyed by `content_fingerprint`.
4. Facet export paths no longer require the thumbnail BLOB to exist locally in
   SQLite.
5. A degraded or unavailable IronMesh thumbnail service has a bounded fallback
   path and does not silently corrupt Facet state.

## Major Design Decisions

### 1. Two thumbnail profiles in IronMesh

IronMesh should expose at least:

- `grid`
  - current UI-oriented low-cost profile
  - keep around `256px`
- `analysis`
  - new higher-resolution JPEG profile for downstream ML and richer UI uses
  - target starting point: `640px`

Why:

- current `grid` is good for list/grid browsing but too small for several Facet
  analysis paths
- Facet today generates `640px` thumbnails, so matching that scale reduces
  behavior change during migration

### 2. Facet should migrate to a fetch abstraction, not to ad hoc HTTP calls

Do not patch individual call sites one by one to hit IronMesh directly.

Instead:

- define one thumbnail provider interface in Facet
- route viewer reads, recompute reads, and export reads through that interface
- allow provider implementations:
  - SQLite/local legacy provider
  - IronMesh remote provider
  - hybrid provider with fallback

### 3. Facet should stop using path text as the thumbnail-storage identity

Path remains important in Facet, but thumbnail lookup needs a stable bridge to
IronMesh identity.

Required bridge data per photo:

- `ironmesh_key`
- `content_fingerprint`
- optionally `version_id`
- thumbnail freshness marker or last-known manifest hash if needed

This can live in:

- new columns on `photos`, or
- a dedicated mapping table

Recommended first shape:

- a dedicated mapping table so the migration does not overload the existing
  `photos` row with transport-specific semantics.

## Workstreams

### Workstream A: IronMesh thumbnail-profile expansion

Goal:

- make IronMesh capable of serving both `grid` and `analysis` thumbnails.

Tasks:

1. Introduce thumbnail profile definitions in `server-node-sdk` instead of
   hard-coding `grid`.
2. Add a new `analysis` profile with explicit max dimension and JPEG quality.
3. Extend `CachedThumbnailInfo` / metadata handling to represent multiple
   profile variants cleanly.
4. Update media-cache persistence layout to support multiple profile payloads
   per `content_fingerprint`.
5. Extend `/media/thumbnail` to accept a `profile` query parameter.
6. Keep `grid` as the default for backwards compatibility.
7. Update store-index thumbnail metadata so clients can request the intended
   profile intentionally.

Questions to settle:

- whether `CachedMediaMetadata.thumbnail` becomes:
  - a list of thumbnails, or
  - a primary thumbnail plus profile registry
- whether `GET /store/index` advertises only `grid` or can advertise multiple
  profiles

Acceptance criteria:

- requesting `profile=analysis` returns a valid `640px`-class JPEG when the
  source is an image
- `grid` behavior remains unchanged for existing clients
- cached profile files coexist under one content fingerprint without conflict

### Workstream B: IronMesh thumbnail-generation policy and backfill

Goal:

- make `analysis` thumbnails operationally usable, not just theoretically
  available.

Tasks:

1. Decide when `analysis` thumbnails are generated:
   - lazy on first request,
   - background-generated for current replicas,
   - or metadata-first with profile-on-demand
2. Add generation and caching tests for `analysis`.
3. Ensure remote media artifact import can carry the requested profile.
4. Validate storage growth and CPU cost of the second profile.
5. Add admin visibility for per-profile cache occupancy if needed.

Acceptance criteria:

- repeated `analysis` thumbnail requests hit cache after first generation
- remote profile import behavior is correct for non-local replicas

### Workstream C: Facet thumbnail-provider abstraction

Goal:

- remove direct `photos.thumbnail` coupling from Facet read paths.

Tasks:

1. Introduce a provider interface, for example:
   - `get_photo_thumbnail(path, profile)`
   - `get_face_base_thumbnail(path)`
2. Add provider implementations:
   - legacy SQLite/local provider
   - IronMesh provider
   - hybrid provider with fallback
3. Migrate viewer thumbnail endpoint code to the provider.
4. Migrate face-crop fallback code to the provider.
5. Migrate dimension backfill and maintenance helpers to the provider where
   feasible.

Important call sites to convert:

- `api/routers/thumbnails.py`
- face crop fallback in the same router
- `api/db_helpers.py::backfill_image_dimensions`

Acceptance criteria:

- no viewer read path depends directly on `SELECT thumbnail FROM photos`
- provider selection is configurable and testable

### Workstream D: Path/key mapping bridge

Goal:

- let Facet resolve a photo path to the correct IronMesh thumbnail identity.

Tasks:

1. Define a mapping table, for example:
   - `photo_transport_map(photo_path, ironmesh_key, content_fingerprint, version_id, updated_at)`
2. Define population rules:
   - at scan/import time,
   - via a separate sync task,
   - or via an exported IronMesh index snapshot
3. Define invalidation rules:
   - when Facet path no longer resolves,
   - when content fingerprint changes,
   - when the local file is replaced in place
4. Decide whether version pinning is necessary for thumbnail correctness or
   whether current head is sufficient.
5. Implement lookup helpers used by the IronMesh provider.

Recommended principle:

- resolve by `content_fingerprint` when possible for reuse
- retain `ironmesh_key` for request routing
- retain optional version or manifest hash only when correctness requires
  pinning

Acceptance criteria:

- given a Facet photo path, the provider can reliably request the matching
  IronMesh thumbnail
- rename and copy scenarios behave predictably

### Workstream E: Facet recompute and analysis migration

Goal:

- make analysis paths consume IronMesh `analysis` thumbnails rather than local
  thumbnail BLOBs.

Tasks:

1. Identify all thumbnail-driven analysis paths and convert them to the
   provider abstraction.
2. Update IQA recompute paths.
3. Update composition recompute paths.
4. Update RAM++ thumbnail-driven tagging.
5. Update caption-generation paths that currently prefer stored thumbnails.
6. Update thumbnail-rotation tools or explicitly retire them if they become
   obsolete under IronMesh-provided normalized thumbnails.
7. Decide whether any analysis path still needs a local cached copy for
   performance.

Primary migration targets:

- `processing/scorer.py`
- `facet.py`

Important semantic choice:

- if IronMesh thumbnails are already orientation-corrected, Facet paths that
  assume raw stored thumbnail orientation may need to stop doing their own fixup
  work.

Acceptance criteria:

- recompute commands work with provider-backed thumbnails
- output quality remains within agreed tolerance versus the legacy
  `photos.thumbnail` path
- performance regression is measured and accepted or mitigated

### Workstream F: Facet write-path and schema cleanup

Goal:

- stop writing photo thumbnail BLOBs as the primary artifact.

Tasks:

1. Add a feature flag or config switch for thumbnail authority:
   - `legacy_local`
   - `hybrid`
   - `ironmesh`
2. In hybrid mode, continue writing local thumbnails for rollback safety.
3. In IronMesh-authoritative mode:
   - stop treating `photos.thumbnail` as required
   - optionally stop generating local photo thumbnail BLOBs altogether
4. Preserve face-thumbnail generation if still needed locally.
5. Update storage migration scripts and docs accordingly.
6. Decide whether `photos.thumbnail` remains:
   - fully removed later,
   - nullable compatibility column,
   - or small local cache only

Recommended cutover sequence:

- do not remove the column early
- first make it optional
- then stop depending on it
- only later consider schema cleanup

Acceptance criteria:

- new scans can complete with `photos.thumbnail` absent or empty
- the app remains operational in hybrid mode during rollout

### Workstream G: Export and viewer-DB tooling update

Goal:

- make export paths work when local thumbnail BLOBs are no longer guaranteed.

Tasks:

1. Update viewer DB export logic to fetch from the provider when thumbnails are
   needed.
2. Decide whether exported lightweight viewer DBs should embed downsized
   thumbnails or store URLs/references.
3. Update maintenance scripts that migrate local thumbnail storage.
4. Update any JSON/CSV export path that implies local thumbnail availability.
5. Add explicit behavior for offline export when IronMesh is unavailable.

Important code areas:

- `db/maintenance.py`
- any export helpers that downsize or migrate `photos.thumbnail`

Acceptance criteria:

- viewer DB export still works after the swap
- export behavior is deterministic in both connected and disconnected states

### Workstream H: Testing, rollout, and rollback

Goal:

- make the migration safe across two repos.

Tasks:

1. Add IronMesh tests for:
   - multiple thumbnail profiles
   - profile-specific cache persistence
   - profile query handling
   - remote import with multiple profiles
2. Add Facet tests for:
   - provider abstraction
   - hybrid fallback behavior
   - recompute paths using provider thumbnails
   - mapping-table freshness logic
3. Define rollout stages:
   - stage 0: legacy
   - stage 1: hybrid read
   - stage 2: hybrid read/write
   - stage 3: IronMesh-authoritative read
   - stage 4: optional local-write disable
4. Define rollback:
   - switch provider back to local
   - keep legacy thumbnails intact during the transition

Acceptance criteria:

- rollback is possible without rescanning the full library
- a partially migrated deployment does not dead-end the worker

## Phases

### Phase 0: contract and schema design

Deliverables:

- agreed thumbnail-profile API
- agreed Facet provider interface
- agreed mapping-table schema
- explicit migration modes and flags

Exit criteria:

- no open ambiguity about profile names, dimensions, identity fields, or
  fallback behavior

### Phase 1: IronMesh profile support

Deliverables:

- `analysis` profile generation in IronMesh
- profile-aware media-thumbnail requests
- tests for multi-profile cache behavior

Exit criteria:

- IronMesh can generate and serve `grid` and `analysis` correctly

### Phase 2: Facet read-path abstraction

Deliverables:

- Facet thumbnail provider abstraction
- viewer and face-crop paths moved off direct `photos.thumbnail` reads
- hybrid provider operational

Exit criteria:

- Facet can browse using IronMesh-backed thumbnails while legacy local fallback
  still works

### Phase 3: mapping and recompute migration

Deliverables:

- path/key/fingerprint bridge
- recompute commands updated to provider-backed reads
- performance baselines for key commands

Exit criteria:

- all important thumbnail-driven analysis commands run against the new backend

### Phase 4: write-path and export migration

Deliverables:

- optional disablement of local photo-thumbnail writes
- provider-backed export and viewer-db generation
- updated operational docs

Exit criteria:

- scans and exports work without relying on `photos.thumbnail` as authoritative

### Phase 5: cutover and cleanup

Deliverables:

- production-ready IronMesh-authoritative mode
- clear rollback instructions
- optional schema deprecation plan for local photo-thumbnail BLOBs

Exit criteria:

- the system is stable in IronMesh-authoritative mode across a representative
  library

## Fallback And Failure Policy

During migration, Facet should follow this order:

1. Try IronMesh `analysis` thumbnail if the path is in a migrated library.
2. If mapping exists but IronMesh request fails transiently, fall back to local
   thumbnail when present.
3. If local thumbnail is absent but local original file is available, optionally
   regenerate a local emergency thumbnail for the specific task.
4. If neither backend is usable, surface an explicit error and do not silently
   write partial or misleading results.

This fallback policy should be configurable because:

- viewer browsing tolerates softer degradation,
- analysis/recompute commands may require strict correctness.

## Risks

### Quality risk

Some Facet analysis pipelines may be sensitive to:

- thumbnail resolution,
- JPEG quality,
- orientation normalization,
- color-management differences.

Mitigation:

- make `analysis` profile match current Facet thumbnail behavior as closely as
  practical before changing recompute paths

### Performance risk

Remote or HTTP-mediated thumbnail fetches may be slower than SQLite BLOB reads.

Mitigation:

- colocate worker and IronMesh endpoint where possible
- add local provider-level caching in Facet
- benchmark recompute commands before cutover

### Identity risk

Path-to-key mapping drift can cause wrong thumbnail selection.

Mitigation:

- key mapping by current path plus fingerprint verification
- explicit invalidation when content changes

### Rollout risk

If local thumbnail writes are removed too early, rollback becomes expensive.

Mitigation:

- keep hybrid mode long enough to build confidence

## Suggested Milestones

1. `M1`: IronMesh serves `analysis` thumbnails behind a profile parameter.
2. `M2`: Facet viewer reads thumbnails through a provider abstraction.
3. `M3`: Facet path-to-IronMesh mapping table is populated reliably.
4. `M4`: IQA/composition/RAM++ recompute paths succeed on provider-backed
   thumbnails.
5. `M5`: Viewer DB export and maintenance tools work without local thumbnail
   authority.
6. `M6`: IronMesh-authoritative mode is validated on a real library.

## Rough Effort

If implemented carefully across both repos, this is closer to a
medium-to-large project than a single feature branch.

Ballpark:

- design and contract work: `2-4 days`
- IronMesh multi-profile support: `4-7 days`
- Facet provider abstraction and viewer migration: `4-7 days`
- path/key mapping plus recompute migration: `5-10 days`
- export/tooling/cutover/testing: `4-8 days`

Total rough range:

- `3-6 engineer-weeks`

That assumes:

- one engineer comfortable working in both codebases,
- no major surprises from model-quality regressions,
- no requirement to replace Facet's original-file access model.

## Recommendation

Before committing to this full swap, the team should still complete a cheaper
intermediate milestone:

- implement the Facet thumbnail provider abstraction first,
- wire it to IronMesh for viewer reads,
- benchmark a prototype `analysis` profile,
- then decide whether the recompute and export migration earns its complexity.

That prototype reduces the risk of discovering too late that:

- `256px` or even `640px` thumbnails are not good enough for some Facet
  analytics,
- network fetch cost dominates recompute time,
- or the path/key mapping is more fragile than expected.
