export type GalleryMapViewport = {
  south: number;
  west: number;
  north: number;
  east: number;
};

const WORLD_LONGITUDE_SPAN = 360;
const MIN_LATITUDE = -90;
const MAX_LATITUDE = 90;

/**
 * Expands a visible map viewport to twice its width and height for a cluster-query buffer.
 * The result keeps antimeridian wrapping and never extends beyond the poles or the full world.
 */
export function galleryMapPrefetchViewport(viewport: GalleryMapViewport): GalleryMapViewport {
  if (!galleryMapViewportIsFinite(viewport)) {
    return viewport;
  }

  const [south, north] = expandBoundedInterval(
    viewport.south,
    viewport.north,
    MIN_LATITUDE,
    MAX_LATITUDE
  );
  const longitudeSpan = galleryMapLongitudeSpan(viewport);
  if (longitudeSpan >= WORLD_LONGITUDE_SPAN / 2) {
    return { south, west: -180, north, east: 180 };
  }

  const doubledLongitudeSpan = longitudeSpan * 2;
  const center = normalizeLongitude(viewport.west + longitudeSpan / 2);
  return {
    south,
    west: normalizeLongitude(center - doubledLongitudeSpan / 2),
    north,
    east: normalizeLongitude(center + doubledLongitudeSpan / 2)
  };
}

function galleryMapViewportIsFinite(viewport: GalleryMapViewport): boolean {
  return Object.values(viewport).every(Number.isFinite) && viewport.south <= viewport.north;
}

function expandBoundedInterval(
  lower: number,
  upper: number,
  minimum: number,
  maximum: number
): [number, number] {
  const targetSpan = Math.min(maximum - minimum, (upper - lower) * 2);
  const center = (lower + upper) / 2;
  let expandedLower = Math.max(minimum, center - targetSpan / 2);
  let expandedUpper = Math.min(maximum, expandedLower + targetSpan);
  expandedLower = Math.max(minimum, expandedUpper - targetSpan);
  return [expandedLower, expandedUpper];
}

function galleryMapLongitudeSpan(viewport: GalleryMapViewport): number {
  const rawSpan = viewport.east - viewport.west;
  if (Math.abs(rawSpan) >= WORLD_LONGITUDE_SPAN) {
    return WORLD_LONGITUDE_SPAN;
  }
  return ((rawSpan % WORLD_LONGITUDE_SPAN) + WORLD_LONGITUDE_SPAN) % WORLD_LONGITUDE_SPAN;
}

function normalizeLongitude(longitude: number): number {
  const wrapped = ((longitude + 180) % WORLD_LONGITUDE_SPAN + WORLD_LONGITUDE_SPAN) %
    WORLD_LONGITUDE_SPAN -
    180;
  return wrapped === -180 && longitude > 0 ? 180 : wrapped;
}
