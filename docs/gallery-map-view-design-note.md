# Gallery Map View Design Note

## Status

Partially implemented.

Current state:

- the shared `Grid` / `Map` gallery switch exists,
- the gallery already supports a self-hosted satellite raster basemap,
- server-side raster XYZ serving from split MBTiles already exists,
- a first self-hosted vector `Street` basemap path now exists,
- hybrid composition remains the next major map slice after the standalone vector mode.

This note assumes the general storage-layer solution for cluster-wide metadata visibility and
read-through chunk caching is already the accepted foundation for large self-hosted map data.

See:

- `docs/read-through-chunk-cache-proposal.md`
- `docs/read-through-chunk-cache-implementation-plan.md`

This document is therefore intentionally limited to gallery-map-specific product and UI decisions.

## Feature Goal

Add a switch in the shared web gallery that changes the current thumbnail grid into a world-map
view.

Requirements:

- use the existing shared web gallery surface,
- only place images that actually have GPS metadata,
- show thumbnails on the map,
- keep the basemap self-hostable on Ironmesh-operated infrastructure,
- keep the normal grid view available as the default fallback.

## Existing Foundation

The media pipeline already exposes the metadata needed by a future map view:

- image classification,
- thumbnail URLs,
- optional EXIF GPS coordinates.

See:

- `docs/server-node-media-cache.md`

The general storage-layer work is documented separately and should not be redesigned here:

- cluster-wide metadata visibility,
- read-through chunk caching for large objects,
- non-replica reads for large self-hosted basemap data.

## Map Stack Choice

### Browser-side map rendering

`MapLibre GL JS` remains the preferred browser-side map library.

Why:

- open and self-hostable,
- good fit for a web gallery with a custom thumbnail marker layer,
- flexible enough for marker clustering, popups, and selection sync with the lightbox.

### Basemap serving

The current preference remains:

- self-hosted `PMTiles` when a simple static basemap is sufficient,
- `Martin` only if Ironmesh later needs a more standard or more dynamic tile-service setup.

This note assumes the storage layer can already serve large basemap assets through the general
read-through path when needed.

## Transitional Split-MBTiles Packaging

For the currently prepared `maptiler-satellite-2017-11-02-planet.mbtiles` dataset, a practical
transitional packaging is:

- keep the already uploaded split files under `sys/maps/`,
- add one JSON manifest next to them,
- let the eventual map-serving layer treat that manifest as the authoritative logical-file layout.

This does not change the longer-term map-serving choice. It is only a packaging and discovery
format for a manually distributed large basemap artifact.

### Immediate cluster location proposal

For the dataset already uploaded to the cluster, store the manifest at:

- `sys/maps/maptiler-satellite-2017-11-02-planet.mbtiles.manifest.json`

Keep the existing part objects as they are:

- `sys/maps/maptiler-satellite-2017-11-02-planet.mbtiles-part-aa`
- `sys/maps/maptiler-satellite-2017-11-02-planet.mbtiles-part-ab`
- ...
- `sys/maps/maptiler-satellite-2017-11-02-planet.mbtiles-part-as`

An example manifest matching the current uploaded files is included in the repo at:

- `docs/examples/maptiler-satellite-2017-11-02-planet.mbtiles.manifest.json`

For the prepared OpenMapTiles vector dataset, the corresponding cluster location should be:

- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles.manifest.json`

with part objects:

- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-aa`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-ab`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-ac`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-ad`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-ae`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-af`
- `sys/maps/maptiler-osm-2020-02-10-v3.11-planet.mbtiles-part-ag`

An example manifest matching the current local split files is included in the repo at:

- `docs/examples/maptiler-osm-2020-02-10-v3.11-planet.mbtiles.manifest.json`

### Preferred long-term layout

For future large map artifacts, prefer a dedicated logical directory instead of a flat file set:

- `sys/maps/<map-id>/manifest.json`
- `sys/maps/<map-id>/parts/<part-id>`

For example:

- `sys/maps/maptiler-satellite-2017-11-02-planet/manifest.json`
- `sys/maps/maptiler-satellite-2017-11-02-planet/parts/aa`
- `sys/maps/maptiler-satellite-2017-11-02-planet/parts/ab`

Why:

- cleaner discovery,
- easier coexistence of multiple basemaps,
- easier to attach additional metadata later such as attribution, bounds, preferred zooms, or
  alternate profiles.

### Manifest structure

The split-file manifest should minimally contain:

- manifest version,
- logical format, for example `mbtiles`,
- logical object key,
- total logical byte size,
- regular part size,
- ordered list of parts with:
  - stable `part_id`,
  - cluster object key,
  - logical byte offset,
  - part size.

The map-serving layer should use the manifest rather than infer part membership by directory
listing or filename guessing.

### Important note

`MapLibre GL JS` does not directly consume a set of split MBTiles files in the browser.

The production Gallery instead uses server-side MBTiles lookup. The browser asks its local client
web backend for metadata, tiles, and glyphs; that backend proxies the authenticated server map API
over the selected direct or multiplex/relay client transport. The server resolves the split-file
manifest, reads its local/read-through cache, and returns only the requested raster or vector tile.
The current Gallery configuration therefore does not reconstruct MBTiles or retain raw map chunks
in the browser.

The logical-file endpoint remains available for generic range consumers, but Gallery does not use
it as a basemap fallback. A server map API error is returned to the Gallery unchanged.

## Vector Street Basemap First

The preferred rollout is now:

1. make a full vector `Street` basemap work first,
2. keep the existing satellite raster mode,
3. combine them later into a `Hybrid` mode once the vector label stack is proven.

Why this order:

- a standalone street map is useful by itself,
- labels, country names, roads, and borders naturally belong to the vector stack,
- hybrid mode then becomes a composition problem instead of a first-principles rendering problem.

### Minimal first vector slice

The first vector slice should stay intentionally small:

- serve vector tiles from the split OpenMapTiles MBTiles via server-side tile endpoints,
- serve glyph PBFs from a configured font directory,
- use an Ironmesh-owned minimal MapLibre style,
- avoid sprite/icon dependencies in the first pass,
- expose `Satellite` and `Street` as separate basemap choices in the gallery.

This first style should rely on a single OpenMapTiles-compatible vector source and cover only:

- background,
- water,
- landcover / landuse,
- roads,
- administrative borders,
- buildings,
- place labels.

### Asset assumptions

The local `maptiler-server-map-styles-and-samples-3.15` bundle is useful as a reference for:

- style structure,
- glyph directory layout,
- sprite layout,
- expected OpenMapTiles layer names.

However, the sample bundle should not be treated as a drop-in Ironmesh deployment artifact.
It contains MapTiler-Server-specific placeholders and may carry license restrictions that differ
from Ironmesh deployment needs.

The first implementation should therefore:

- own the actual style JSON in Ironmesh,
- treat sprite usage as optional and deferred,
- treat the font directory as an external/configured asset root.

### Backend shape

The backend side of the vector slice should expose:

- an MBTiles metadata endpoint,
- a vector tile endpoint,
- a glyph endpoint.

The vector tile endpoint should:

- resolve split logical-file manifests the same way as raster MBTiles,
- query the MBTiles database server-side,
- return protobuf vector tile bytes,
- preserve gzip content encoding when tiles are stored compressed in MBTiles.

The glyph endpoint should:

- serve `{fontstack}/{range}.pbf`,
- validate path segments conservatively,
- read from a configured font root,
- avoid baking sample-bundle assumptions into the frontend.

### Frontend shape

The frontend side should:

- allow multiple self-hosted basemap definitions,
- persist the selected basemap choice locally in the browser,
- render raster and vector basemaps through the same shared gallery map surface,
- keep marker overlay behavior identical across basemap modes.

The first vector style should live in Ironmesh frontend code rather than depend on an external
style JSON server.

## Remaining Map-Specific Decisions

The main open questions are now product and UI questions rather than storage questions:

- how the gallery switches between `Grid` and `Map`,
- whether the map opens at the global view or auto-fits the current visible images,
- how thumbnail markers should look at different zoom levels,
- when to switch from single thumbnails to clustering,
- how marker selection interacts with the existing fullscreen image view,
- how prefix/depth filtering in the gallery should affect the visible marker set,
- what the first self-hosted basemap profile should be.

## Recommended First Implementation

1. Add a `Grid` / `Map` switch to the shared gallery surface.
2. Show only GPS-bearing images on the map.
3. Reuse existing thumbnail URLs for marker visuals and preview popups.
4. Keep selection synchronized between map markers and the existing fullscreen image viewer.
5. Start with a basic self-hosted basemap and add clustering once the interaction model feels
   right.

## Current Recommendation

Treat the storage-layer foundation as an existing dependency, not as part of this feature note.

The next design and implementation work for the gallery map should focus on:

- vector street basemap delivery,
- shared gallery UX for switching between self-hosted basemap modes,
- marker rendering and clustering,
- fullscreen-view integration,
- practical basemap packaging and deployment for self-hosted use,
- hybrid composition once both raster and vector basemap modes are stable.
