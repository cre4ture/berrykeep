export type CursorPagedSource = {
  nextOffset: number;
  hasMore: boolean;
  totalEntryCount: number | null;
};

export type ClusterDialogPageRequest<TSource> = {
  source: TSource;
  limit: number;
};

export function rotatingClusterDialogPageRequests<TSource extends CursorPagedSource>(
  sources: TSource[],
  startIndex: number,
  pageSize: number,
  maxSources: number,
  estimatedTotalEntryCount: (source: TSource) => number
): {
  requests: ClusterDialogPageRequest<TSource>[];
  nextSourceIndex: number;
} {
  // A screen-space bubble can contain a large number of server clusters.
  // Rotate a bounded subset through each dialog page so opening the bubble
  // stays responsive while every contained source remains reachable.
  const selectedSourceIndexes: number[] = [];
  for (let offset = 0; offset < sources.length; offset += 1) {
    const sourceIndex = (startIndex + offset) % sources.length;
    const source = sources[sourceIndex];
    if (!source?.hasMore) {
      continue;
    }
    selectedSourceIndexes.push(sourceIndex);
    if (selectedSourceIndexes.length >= maxSources) {
      break;
    }
  }

  const selectedSources = selectedSourceIndexes.map((sourceIndex) => sources[sourceIndex]!);
  const remainingEntries = selectedSources.map((source) =>
    Math.max(1, (source.totalEntryCount ?? estimatedTotalEntryCount(source)) - source.nextOffset)
  );
  const limits = selectedSources.map(() => 0);
  let remainingPageCapacity = Math.min(
    pageSize,
    remainingEntries.reduce((total, count) => total + count, 0)
  );

  while (remainingPageCapacity > 0) {
    let addedEntry = false;
    for (let index = 0; index < selectedSources.length && remainingPageCapacity > 0; index += 1) {
      if (limits[index]! >= remainingEntries[index]!) {
        continue;
      }
      limits[index]! += 1;
      remainingPageCapacity -= 1;
      addedEntry = true;
    }
    if (!addedEntry) {
      break;
    }
  }

  const lastSelectedSourceIndex = selectedSourceIndexes.at(-1);
  return {
    requests: selectedSources.flatMap((source, index) => {
      const limit = limits[index] ?? 0;
      return limit > 0 ? [{ source, limit }] : [];
    }),
    nextSourceIndex:
      lastSelectedSourceIndex === undefined ? startIndex : (lastSelectedSourceIndex + 1) % sources.length
  };
}
