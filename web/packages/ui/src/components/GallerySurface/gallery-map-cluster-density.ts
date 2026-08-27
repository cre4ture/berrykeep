const DEFAULT_GALLERY_MAP_CLUSTER_CELL_SIZE_PX = 32;
const GALLERY_MAP_CLUSTER_CELL_SIZE_OPTIONS_PX = [16, 24, 32, 48, 64] as const;
const GALLERY_MAP_REFERENCE_VIEWPORT_AREA_PX = 1_024 * 768;

/**
 * Selects a bounded CSS-pixel cluster target from the visible map area. Smaller maps keep
 * bubbles legible, while larger displays can request a denser grid. The server repeats the
 * bounding and quantization before using this hint.
 */
export function galleryMapClusterCellSizeForViewport(width: number, height: number): number {
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) {
    return DEFAULT_GALLERY_MAP_CLUSTER_CELL_SIZE_PX;
  }

  const area = width * height;
  const requestedSize = DEFAULT_GALLERY_MAP_CLUSTER_CELL_SIZE_PX * Math.sqrt(
    GALLERY_MAP_REFERENCE_VIEWPORT_AREA_PX / area
  );
  return GALLERY_MAP_CLUSTER_CELL_SIZE_OPTIONS_PX.reduce((closestSize, candidateSize) =>
    Math.abs(candidateSize - requestedSize) < Math.abs(closestSize - requestedSize)
      ? candidateSize
      : closestSize
  );
}
