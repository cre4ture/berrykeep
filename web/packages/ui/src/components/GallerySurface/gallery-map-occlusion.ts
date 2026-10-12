export type GalleryMapOcclusionTransform<TLocation> = {
  isLocationOccluded?: (location: TLocation) => boolean;
};

export type GalleryMapOcclusionAdapter<TLocation> = {
  transform?: GalleryMapOcclusionTransform<TLocation>;
  _camera?: {
    transform?: GalleryMapOcclusionTransform<TLocation>;
  };
};

export function isGalleryMapLocationOccluded<TLocation>(
  map: GalleryMapOcclusionAdapter<TLocation>,
  location: TLocation
): boolean | null {
  // MapLibre moved its transform from Map to Map._camera in v6.
  const transform = map._camera?.transform ?? map.transform;
  const isLocationOccluded = transform?.isLocationOccluded;
  return isLocationOccluded ? isLocationOccluded(location) : null;
}
