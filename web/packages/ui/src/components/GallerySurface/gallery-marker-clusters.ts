export type ClusterableScreenPoint<T> = {
  id: string;
  x: number;
  y: number;
  item: T;
};

export type ScreenPointCluster<T> = {
  id: string;
  x: number;
  y: number;
  minX: number;
  maxX: number;
  minY: number;
  maxY: number;
  points: ClusterableScreenPoint<T>[];
};

type MutableScreenPointCluster<T> = {
  gridCellX: number;
  gridCellY: number;
  xTotal: number;
  yTotal: number;
  minX: number;
  maxX: number;
  minY: number;
  maxY: number;
  points: ClusterableScreenPoint<T>[];
};

/** Groups nearby projected points while preserving the underlying items for click handling. */
export function clusterScreenPoints<T>(
  points: ClusterableScreenPoint<T>[],
  radius: number
): ScreenPointCluster<T>[] {
  if (points.length === 0) {
    return [];
  }

  const safeRadius = Math.max(1, radius);
  const radiusSquared = safeRadius * safeRadius;
  const grid = new Map<string, MutableScreenPointCluster<T>[]>();
  const clusters: MutableScreenPointCluster<T>[] = [];

  for (const point of points) {
    const cellX = Math.floor(point.x / safeRadius);
    const cellY = Math.floor(point.y / safeRadius);
    let bestCluster: MutableScreenPointCluster<T> | null = null;
    let bestDistanceSquared = Number.POSITIVE_INFINITY;

    for (let deltaX = -1; deltaX <= 1; deltaX += 1) {
      for (let deltaY = -1; deltaY <= 1; deltaY += 1) {
        const candidates = grid.get(clusterGridKey(cellX + deltaX, cellY + deltaY));
        if (!candidates) {
          continue;
        }

        for (const candidate of candidates) {
          const centroidX = candidate.xTotal / candidate.points.length;
          const centroidY = candidate.yTotal / candidate.points.length;
          const distanceSquared =
            (point.x - centroidX) * (point.x - centroidX) +
            (point.y - centroidY) * (point.y - centroidY);
          if (distanceSquared > radiusSquared || distanceSquared >= bestDistanceSquared) {
            continue;
          }

          bestCluster = candidate;
          bestDistanceSquared = distanceSquared;
        }
      }
    }

    if (!bestCluster) {
      const nextCluster: MutableScreenPointCluster<T> = {
        gridCellX: cellX,
        gridCellY: cellY,
        xTotal: point.x,
        yTotal: point.y,
        minX: point.x,
        maxX: point.x,
        minY: point.y,
        maxY: point.y,
        points: [point]
      };
      clusters.push(nextCluster);
      addClusterToGrid(grid, nextCluster);
      continue;
    }

    bestCluster.points.push(point);
    bestCluster.xTotal += point.x;
    bestCluster.yTotal += point.y;
    bestCluster.minX = Math.min(bestCluster.minX, point.x);
    bestCluster.maxX = Math.max(bestCluster.maxX, point.x);
    bestCluster.minY = Math.min(bestCluster.minY, point.y);
    bestCluster.maxY = Math.max(bestCluster.maxY, point.y);
    moveClusterInGrid(grid, bestCluster, safeRadius);
  }

  return clusters.map((cluster) => {
    const x = cluster.xTotal / cluster.points.length;
    const y = cluster.yTotal / cluster.points.length;
    return {
      id: cluster.points
        .map((point) => point.id)
        .sort()
        .join("|"),
      x,
      y,
      minX: cluster.minX,
      maxX: cluster.maxX,
      minY: cluster.minY,
      maxY: cluster.maxY,
      points: cluster.points
    };
  });
}

function addClusterToGrid<T>(
  grid: Map<string, MutableScreenPointCluster<T>[]>,
  cluster: MutableScreenPointCluster<T>
): void {
  const key = clusterGridKey(cluster.gridCellX, cluster.gridCellY);
  const cellClusters = grid.get(key) ?? [];
  cellClusters.push(cluster);
  grid.set(key, cellClusters);
}

function moveClusterInGrid<T>(
  grid: Map<string, MutableScreenPointCluster<T>[]>,
  cluster: MutableScreenPointCluster<T>,
  radius: number
): void {
  const centroidX = cluster.xTotal / cluster.points.length;
  const centroidY = cluster.yTotal / cluster.points.length;
  const nextCellX = Math.floor(centroidX / radius);
  const nextCellY = Math.floor(centroidY / radius);
  if (nextCellX === cluster.gridCellX && nextCellY === cluster.gridCellY) {
    return;
  }

  const previousKey = clusterGridKey(cluster.gridCellX, cluster.gridCellY);
  const previousCellClusters = grid.get(previousKey) ?? [];
  const clusterIndex = previousCellClusters.indexOf(cluster);
  if (clusterIndex >= 0) {
    previousCellClusters.splice(clusterIndex, 1);
  }
  if (previousCellClusters.length === 0) {
    grid.delete(previousKey);
  }

  cluster.gridCellX = nextCellX;
  cluster.gridCellY = nextCellY;
  addClusterToGrid(grid, cluster);
}

function clusterGridKey(cellX: number, cellY: number): string {
  return `${cellX}:${cellY}`;
}
