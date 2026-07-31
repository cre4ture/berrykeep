# Gallery sync and viewport API

The current gallery index is the source for both full cache bootstrap and incremental updates.
The public routes use normal client authorization; equivalent `/auth/...` routes require the
same admin authorization as the existing admin store index.

## Cache bootstrap and delta feed

Request a current, paginated gallery view through `GET /api/v1/store/index` (or
`GET /api/v1/auth/store/index`). SQLite-backed gallery fast-path responses include an opaque
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

The SQLite projection and its revision log are updated in the same metadata transaction. Object
adds, replacements, removals, manifest/version metadata changes, and material media-cache changes
(including thumbnail metadata, capture time, and GPS) advance the revision. Rewriting identical
metadata does not advance it. The full-index token is captured at the start of the SQLite read
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

## Metadata backend capability

SQLite is the default server-node metadata backend and owns the persistent gallery projection,
viewport index, and revision log. The optional `turso-metadata` backend already uses the generic
store-index fallback and does not maintain that projection. It therefore returns deterministic
HTTP `501 Not Implemented` responses for viewport and delta requests instead of returning partial
or non-durable results. Clients can capability-detect this response and continue using ordinary
full store-index requests. Turso parity requires first porting the gallery projection itself; the
API contracts above remain suitable for that future implementation.
