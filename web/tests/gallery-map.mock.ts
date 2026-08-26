import { filterMockStoreEntriesToPrefix } from "./store-index.mock";

type GalleryMapMockEntry = {
  path: string;
  entry_type: "prefix" | "key";
  modified_at_unix?: number;
  media?: Record<string, unknown>;
};

type GalleryMapGps = {
  latitude: number;
  longitude: number;
};

type GalleryMapViewport = {
  south: number;
  west: number;
  north: number;
  east: number;
};

export class GalleryMapMockSession<T extends GalleryMapMockEntry> {
  private querySequence = 0;
  private readonly entriesByToken = new Map<string, Map<string, T[]>>();

  clusters(entries: T[], searchParams: URLSearchParams) {
    const queryToken = `mock-gallery-map-${++this.querySequence}`;
    const prefix = searchParams.get("prefix") ?? "";
    const depth = Math.max(1, Number(searchParams.get("depth") ?? "1") || 1);
    const mediaFilter = searchParams.get("media_filter") ?? "all";
    const requestedZoom = Number(
      searchParams.get("zoom_precise") ?? searchParams.get("zoom") ?? "1"
    );
    const zoom = Number.isFinite(requestedZoom)
      ? Math.max(0, Math.min(20, requestedZoom))
      : 1;
    const gridZoom = Math.ceil(zoom * 2) / 2;
    const resolution = Math.ceil(4 * 2 ** gridZoom);
    const viewport = {
      south: Number(searchParams.get("south") ?? "-90"),
      west: Number(searchParams.get("west") ?? "-180"),
      north: Number(searchParams.get("north") ?? "90"),
      east: Number(searchParams.get("east") ?? "180")
    };
    const scopedEntries = filterMockStoreEntriesToPrefix(entries, prefix).filter(
      (entry) =>
        entry.entry_type === "key" &&
        matchesMediaFilter(entry, mediaFilter) &&
        relativeDepth(entry.path, prefix) <= depth
    );
    const entriesByCluster = new Map<string, T[]>();
    for (const entry of scopedEntries) {
      const gps = galleryGps(entry);
      if (!gps || !gpsInViewport(gps, viewport)) {
        continue;
      }
      const [x, y] = webMercatorPosition(gps.latitude, gps.longitude);
      const clusterId = `${Math.min(resolution - 1, Math.floor(x * resolution))}_${Math.min(
        resolution - 1,
        Math.floor(y * resolution)
      )}`;
      const clusterEntries = entriesByCluster.get(clusterId) ?? [];
      clusterEntries.push(entry);
      entriesByCluster.set(clusterId, clusterEntries);
    }
    this.entriesByToken.set(queryToken, entriesByCluster);

    const clusters = [...entriesByCluster.entries()].map(([clusterId, clusterEntries]) => {
      const positions = clusterEntries.map((entry) => galleryGps(entry)!);
      const latitudes = positions.map((position) => position.latitude);
      const longitudes = positions.map((position) => position.longitude);
      return {
        cluster_id: clusterId,
        count: clusterEntries.length,
        latitude: latitudes.reduce((sum, value) => sum + value, 0) / latitudes.length,
        longitude: longitudes.reduce((sum, value) => sum + value, 0) / longitudes.length,
        bounds: {
          south: Math.min(...latitudes),
          west: Math.min(...longitudes),
          north: Math.max(...latitudes),
          east: Math.max(...longitudes)
        },
        entry: clusterEntries.length === 1 ? clusterEntries[0] : undefined
      };
    });

    return {
      prefix,
      depth,
      zoom: Math.floor(zoom),
      resolution,
      total_entry_count: scopedEntries.length,
      visible_geotagged_count: [...entriesByCluster.values()].reduce(
        (count, clusterEntries) => count + clusterEntries.length,
        0
      ),
      media_summary: summarizeMediaEntries(scopedEntries),
      query_token: queryToken,
      clusters
    };
  }

  clusterEntries(searchParams: URLSearchParams) {
    const queryToken = searchParams.get("query_token") ?? "";
    const clusterId = searchParams.get("cluster_id") ?? "";
    const offset = Math.max(0, Number(searchParams.get("offset") ?? "0") || 0);
    const limit = Math.max(1, Number(searchParams.get("limit") ?? "100") || 100);
    const clusterEntries = [...(this.entriesByToken.get(queryToken)?.get(clusterId) ?? [])].sort(
      (left, right) =>
        (right.modified_at_unix ?? mediaTakenAt(right)) -
          (left.modified_at_unix ?? mediaTakenAt(left)) || left.path.localeCompare(right.path)
    );
    const entries = clusterEntries.slice(offset, offset + limit);
    return {
      cluster_id: clusterId,
      entry_count: entries.length,
      total_entry_count: clusterEntries.length,
      offset,
      limit,
      has_more: offset + entries.length < clusterEntries.length,
      query_token: queryToken,
      entries
    };
  }
}

function matchesMediaFilter(entry: GalleryMapMockEntry, mediaFilter: string): boolean {
  if (!entry.media) {
    return false;
  }
  return mediaFilter === "all" || entry.media.media_type === mediaFilter;
}

function summarizeMediaEntries(entries: GalleryMapMockEntry[]) {
  return entries.reduce(
    (summary, entry) => {
      const media = entry.media;
      if (!media) {
        return summary;
      }
      if (media.status === "ready") {
        summary.ready_count += 1;
      } else if (media.status === "pending") {
        summary.pending_count += 1;
      } else {
        summary.incomplete_count += 1;
      }
      if (media.media_type === "image") {
        summary.image_count += 1;
      }
      if (media.media_type === "video") {
        summary.video_count += 1;
      }
      if (galleryGps(entry)) {
        summary.geotagged_count += 1;
      }
      return summary;
    },
    {
      ready_count: 0,
      pending_count: 0,
      incomplete_count: 0,
      image_count: 0,
      video_count: 0,
      geotagged_count: 0
    }
  );
}

function relativeDepth(path: string, prefix: string): number {
  const normalizedPrefix = prefix.replace(/^\/+|\/+$/g, "");
  const relativePath = normalizedPrefix
    ? path === normalizedPrefix
      ? ""
      : path.slice(normalizedPrefix.length + 1)
    : path;
  return relativePath ? relativePath.split("/").filter(Boolean).length : 0;
}

function galleryGps(entry: GalleryMapMockEntry): GalleryMapGps | null {
  const gps = entry.media?.gps;
  if (!gps || typeof gps !== "object") {
    return null;
  }
  const latitude = (gps as Record<string, unknown>).latitude;
  const longitude = (gps as Record<string, unknown>).longitude;
  return typeof latitude === "number" && typeof longitude === "number"
    ? { latitude, longitude }
    : null;
}

function mediaTakenAt(entry: GalleryMapMockEntry): number {
  const takenAt = entry.media?.taken_at_unix;
  return typeof takenAt === "number" ? takenAt : 0;
}

function gpsInViewport(gps: GalleryMapGps, viewport: GalleryMapViewport): boolean {
  const longitudeMatches =
    viewport.west <= viewport.east
      ? gps.longitude >= viewport.west && gps.longitude <= viewport.east
      : gps.longitude >= viewport.west || gps.longitude <= viewport.east;
  return gps.latitude >= viewport.south && gps.latitude <= viewport.north && longitudeMatches;
}

function webMercatorPosition(latitude: number, longitude: number): [number, number] {
  const x = Math.min(1 - Number.EPSILON, Math.max(0, (longitude + 180) / 360));
  const clampedLatitude = Math.min(85.0511287798066, Math.max(-85.0511287798066, latitude));
  const radians = (clampedLatitude * Math.PI) / 180;
  const y = Math.min(
    1 - Number.EPSILON,
    Math.max(
      0,
      0.5 - Math.log((1 + Math.sin(radians)) / (1 - Math.sin(radians))) / (4 * Math.PI)
    )
  );
  return [x, y];
}
