import maplibregl, { type GeoJSONSource } from "maplibre-gl";

const GALLERY_MAP_CLUSTER_GRID_SOURCE_ID = "ironmesh-gallery-cluster-grid";
const GALLERY_MAP_CLUSTER_GRID_FILL_LAYER_ID = "ironmesh-gallery-cluster-grid-fill";
const GALLERY_MAP_CLUSTER_GRID_LINE_LAYER_ID = "ironmesh-gallery-cluster-grid-line";

type GalleryMapClusterGridPayload = {
  resolution: number;
  clusters: Array<{
    cluster_id: string;
    count: number;
  }>;
};

export function updateGalleryMapClusterGrid(
  map: maplibregl.Map,
  payload: GalleryMapClusterGridPayload | null
) {
  if (!payload) {
    removeGalleryMapClusterGrid(map);
    return;
  }

  const gridData = galleryMapClusterGridData(payload);
  const source = map.getSource(GALLERY_MAP_CLUSTER_GRID_SOURCE_ID) as GeoJSONSource | undefined;
  if (source) {
    source.setData(gridData);
    return;
  }

  map.addSource(GALLERY_MAP_CLUSTER_GRID_SOURCE_ID, {
    type: "geojson",
    data: gridData
  });
  map.addLayer({
    id: GALLERY_MAP_CLUSTER_GRID_FILL_LAYER_ID,
    type: "fill",
    source: GALLERY_MAP_CLUSTER_GRID_SOURCE_ID,
    paint: {
      "fill-color": "#22d3ee",
      "fill-opacity": 0.08
    }
  });
  map.addLayer({
    id: GALLERY_MAP_CLUSTER_GRID_LINE_LAYER_ID,
    type: "line",
    source: GALLERY_MAP_CLUSTER_GRID_SOURCE_ID,
    paint: {
      "line-color": "#67e8f9",
      "line-width": 1.5,
      "line-opacity": 0.9
    }
  });
}

function galleryMapClusterGridData(
  payload: GalleryMapClusterGridPayload
): Parameters<GeoJSONSource["setData"]>[0] {
  const resolution = Math.floor(payload.resolution);
  if (!Number.isFinite(resolution) || resolution < 1) {
    return { type: "FeatureCollection", features: [] };
  }

  return {
    type: "FeatureCollection",
    features: payload.clusters.flatMap((cluster) => {
      const cell = galleryMapClusterGridCell(cluster.cluster_id, resolution);
      if (!cell) {
        return [];
      }

      return [
        {
          type: "Feature",
          properties: { clusterId: cluster.cluster_id, count: cluster.count },
          geometry: {
            type: "Polygon",
            coordinates: [
              [
                [cell.west, cell.south],
                [cell.east, cell.south],
                [cell.east, cell.north],
                [cell.west, cell.north],
                [cell.west, cell.south]
              ]
            ]
          }
        }
      ];
    })
  };
}

function galleryMapClusterGridCell(clusterId: string, resolution: number) {
  const [cellX, cellY, ...extraParts] = clusterId.split("_");
  if (extraParts.length > 0) {
    return null;
  }

  const x = Number(cellX);
  const y = Number(cellY);
  if (
    !Number.isInteger(x) ||
    !Number.isInteger(y) ||
    x < 0 ||
    y < 0 ||
    x >= resolution ||
    y >= resolution
  ) {
    return null;
  }

  return {
    west: (x / resolution) * 360 - 180,
    south: webMercatorLatitudeForGridY((y + 1) / resolution),
    east: ((x + 1) / resolution) * 360 - 180,
    north: webMercatorLatitudeForGridY(y / resolution)
  };
}

function webMercatorLatitudeForGridY(y: number): number {
  return (Math.atan(Math.sinh(Math.PI * (1 - 2 * y))) * 180) / Math.PI;
}

function removeGalleryMapClusterGrid(map: maplibregl.Map) {
  if (map.getLayer(GALLERY_MAP_CLUSTER_GRID_LINE_LAYER_ID)) {
    map.removeLayer(GALLERY_MAP_CLUSTER_GRID_LINE_LAYER_ID);
  }
  if (map.getLayer(GALLERY_MAP_CLUSTER_GRID_FILL_LAYER_ID)) {
    map.removeLayer(GALLERY_MAP_CLUSTER_GRID_FILL_LAYER_ID);
  }
  if (map.getSource(GALLERY_MAP_CLUSTER_GRID_SOURCE_ID)) {
    map.removeSource(GALLERY_MAP_CLUSTER_GRID_SOURCE_ID);
  }
}
