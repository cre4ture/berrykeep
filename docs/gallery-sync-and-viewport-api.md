# Gallery sync and viewport API

The current gallery index is the source for both full cache bootstrap and incremental updates.
The public routes use normal client authorization; equivalent `/auth/...` routes require the
same admin authorization as the existing admin store index.

## Cache bootstrap and delta feed

Request a current, paginated gallery view through `GET /api/v1/store/index` (or
`GET /api/v1/auth/store/index`). Gallery fast-path responses backed by persistent metadata include an opaque
`sync_token`. Store the response and token atomically on the client. The token is versioned but
must not be parsed by clients. It binds the persistent server history and the normalized query
membership (`prefix`, `depth`, media filter, captured sort, and optional viewport). Offset and
limit are intentionally excluded, so pages from the same bootstrap have byte-identical tokens.

When a full bootstrap needs multiple offset/limit pages, all pages must carry the same
`sync_token`. If any page returns a different token, the underlying current projection changed
while offsets were being traversed and the client must restart the full bootstrap. Only persist
the common token after every page has been stored. This avoids silently missing an unchanged row
that moved across an offset boundary during the scan.

Resume with:

```text
GET /api/v1/store/index/delta?token=<sync_token>&limit=500
GET /api/v1/auth/store/index/delta?token=<sync_token>&limit=500
```

The response is:

```json
{
  "next_token": "g2_...",
  "has_more": false,
  "upserts": [],
  "removals": []
}
```

Apply `upserts` by `path`, delete every path in `removals`, and persist `next_token` in the same
client transaction. When `has_more` is true, immediately request the next page with
`next_token`; do not reuse the previous token. A page may coalesce multiple retained changes to
the same path into its latest current representation. Delta payloads are latest-state
reconciliation, not an audit log: if an earlier retained upsert is requested after that path has
already been deleted, it is returned as a removal. Applying every page through the final
`has_more: false` token converges the cache to the current projection.

The raw retained-change limit is applied before scope filtering. A valid page can therefore have
empty `upserts` and `removals` while `next_token` advances, including with `has_more: true`.
Clients must persist that token and continue until `has_more` is false; an empty payload is not an
end-of-feed signal.

The delta feed reconciles the complete membership scope captured in the token, independent of
the bootstrap's pagination. Changes outside that prefix/depth/media/viewport scope advance the
token but are omitted. An entry that newly matches the scope is an upsert; an entry that was in
the cache but is deleted or no longer matches the filter or viewport is a removal. Captured sort
is part of token identity so mixed-query pages cannot be mistaken for one bootstrap, although
clients remain responsible for ordering their local materialized set.

The persistent metadata projection and its revision log are updated in the same metadata
transaction. Object adds, replacements, removals, manifest/version metadata changes, and material media-cache changes
(including thumbnail metadata, capture time, and GPS) advance the revision. Rewriting identical
metadata does not advance it. The full-index token is captured at the start of the metadata read
snapshot: rows committed later are intentionally absent from that response and are replayed by
the delta feed. A persistent history UUID and revisions survive process restarts.

The server retains the newest 100,000 gallery changes. A malformed token returns HTTP `400` with
`code: "store_index_delta_invalid_token"`. A token older than retained history or ahead of this
server's history returns HTTP `409` with:

```json
{
  "code": "store_index_delta_reset_required",
  "reset": true,
  "message": "...",
  "current_token": "g2_..."
}
```

Tokens from another server-node history (including a per-request SDK failover to a node with a
different local history) receive the same `409` reset response even when their numeric revision
would otherwise be in range. On any reset response, discard incremental assumptions and repeat
the full bootstrap against the selected endpoint. The `current_token` is diagnostic and is not a
replacement for a token obtained with the new full response.

## Viewport queries

The existing store-index routes accept all four bounds together:

```text
south=<latitude>&west=<longitude>&north=<latitude>&east=<longitude>
```

Bounds are available for current, paginated gallery queries that use a media filter and
`captured_asc` or `captured_desc` ordering. Latitude must satisfy
`-90 <= south <= north <= 90`; longitude must be between `-180` and `180`. `west <= east` is a
normal interval. `west > east` means the viewport crosses the antimeridian and matches longitudes
from `west` through `180` or from `-180` through `east`. Entries without valid finite GPS values
are excluded. Prefix/depth, media filter, captured ordering, offset, limit, and authorization are
applied by the same persistent gallery projection, so the server does not materialize the whole
library.

## Server-side map clustering

The interactive map does not traverse the full gallery index. It requests a bounded set of
server-side spatial clusters for the current camera:

```text
GET /api/v1/store/map/clusters?prefix=&depth=64&media_filter=all&south=-90&west=-180&north=90&east=180&zoom=1
GET /api/v1/auth/store/map/clusters?prefix=&depth=64&media_filter=all&south=-90&west=-180&north=90&east=180&zoom=1
```

All four viewport bounds are required and use the same antimeridian rules as viewport index
queries. `zoom` is clamped to `0..20`. The Gallery UI starts with the world viewport and the
maximum supported UI depth (`64`), then issues a new request after each map movement. Map
clustering is available for current data; selecting an immutable snapshot switches the Gallery to
grid view.

Each metadata backend persists normalized Web Mercator `x`/`y` values next to valid GPS metadata
and maintains a B-tree spatial index. The cluster query restricts that index to the viewport and
groups matching rows into a zoom-dependent Web Mercator grid. It initially uses cells equivalent
to 64 screen pixels. If the requested grid would return more than 512 clusters, the server halves
the effective grid resolution until the response is bounded. This makes response size independent
of total library size while still allowing the client to refine the result by zooming or panning.
Latitude/longitude bounds remain as an exact check around the indexed projected range.

A response has this shape:

```json
{
  "prefix": "",
  "depth": 64,
  "zoom": 1,
  "resolution": 8,
  "total_entry_count": 125000,
  "visible_geotagged_count": 84231,
  "media_summary": {
    "ready_count": 120000,
    "pending_count": 4500,
    "incomplete_count": 500,
    "image_count": 118000,
    "video_count": 7000,
    "geotagged_count": 84231
  },
  "query_token": "gm1_...",
  "clusters": [
    {
      "cluster_id": "4_2",
      "count": 731,
      "latitude": 47.2,
      "longitude": 8.4,
      "bounds": {
        "south": 46.8,
        "west": 7.9,
        "north": 47.7,
        "east": 9.0
      }
    }
  ]
}
```

`total_entry_count` and `media_summary` describe the complete prefix/depth/media-filter scope;
`visible_geotagged_count` describes the current viewport. A one-entry cluster additionally carries
the complete store-index `entry`, so its marker can open without another list query. Multi-entry
clusters carry a centroid and exact member bounds. Clients normally zoom to those bounds. When a
cluster remains dense at high zoom or has identical coordinates, its members are available through
the bounded leaf endpoint:

```text
GET /api/v1/store/map/cluster-entries?query_token=<query_token>&cluster_id=4_2&offset=0&limit=100
GET /api/v1/auth/store/map/cluster-entries?query_token=<query_token>&cluster_id=4_2&offset=0&limit=100
```

The default leaf page size is 100 and the server caps it at 500. Entries are ordered by capture
time descending and path ascending. The opaque map query token binds the persistent gallery
history/revision, normalized scope, viewport, media filter, and effective grid resolution. Clients
must not parse it. If the gallery revision changes before a leaf page is read, the server rejects
the page with HTTP `409`:

```json
{
  "code": "gallery_map_cluster_stale",
  "reset": true,
  "message": "the gallery changed; reload map clusters before opening this cluster"
}
```

On that response the client reloads clusters for its current camera before attempting to open the
cluster again. This prevents offset pagination from combining members from different spatial
snapshots.

## Metadata backend capability

SQLite is the default server-node metadata backend. The optional `turso-metadata` backend provides
the same durable gallery projection, viewport and Web Mercator indexes, spatial clustering, and
revision log. Both maintain changes as part of their metadata updates, and preserve the history
identifier and revision across a restart. Gallery queries, viewport queries, map cluster queries,
and delta requests therefore have the same API contract on both backends.
