import { expect, test } from "@playwright/test";
import {
  isGalleryMapLocationOccluded
} from "../packages/ui/src/components/GallerySurface/gallery-map-occlusion";

test("gallery map uses the MapLibre v6 camera transform for globe occlusion", () => {
  const backSideLocation = { longitude: 180, latitude: 0 };
  const map = {
    _camera: {
      transform: {
        isLocationOccluded: (location: typeof backSideLocation) => location === backSideLocation
      }
    }
  };

  expect(isGalleryMapLocationOccluded(map, backSideLocation)).toBe(true);
});

test("gallery map retains the MapLibre v5 transform fallback for globe occlusion", () => {
  const backSideLocation = { longitude: 180, latitude: 0 };
  const map = {
    transform: {
      isLocationOccluded: (location: typeof backSideLocation) => location === backSideLocation
    }
  };

  expect(isGalleryMapLocationOccluded(map, backSideLocation)).toBe(true);
});
