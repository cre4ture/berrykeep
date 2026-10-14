import { expect, test } from "@playwright/test";
import { rotatingClusterDialogPageRequests } from "../packages/ui/src/components/GallerySurface/gallery-map-cluster-pagination";

type TestSource = {
  id: number;
  count: number;
  nextOffset: number;
  hasMore: boolean;
  totalEntryCount: number | null;
};

test("paginates dense merged map clusters without dropping sources", () => {
  const sources: TestSource[] = Array.from({ length: 13 }, (_, id) => ({
    id,
    count: 1,
    nextOffset: 0,
    hasMore: true,
    totalEntryCount: null
  }));
  const page = (currentSources: TestSource[], startIndex: number) =>
    rotatingClusterDialogPageRequests(currentSources, startIndex, 100, 6, (source) => source.count);
  const sourceIds = (requests: ReturnType<typeof page>["requests"]) =>
    requests.map(({ source }) => source.id);
  const markLoaded = (
    currentSources: TestSource[],
    requests: ReturnType<typeof page>["requests"]
  ) =>
    currentSources.map((source) =>
      requests.some((request) => request.source.id === source.id)
        ? { ...source, hasMore: false }
        : source
    );

  const firstPage = page(sources, 0);
  expect(sourceIds(firstPage.requests)).toEqual([0, 1, 2, 3, 4, 5]);
  expect(firstPage.nextSourceIndex).toBe(6);

  const secondPage = page(markLoaded(sources, firstPage.requests), firstPage.nextSourceIndex);
  expect(sourceIds(secondPage.requests)).toEqual([6, 7, 8, 9, 10, 11]);
  expect(secondPage.nextSourceIndex).toBe(12);

  const thirdPage = page(
    markLoaded(markLoaded(sources, firstPage.requests), secondPage.requests),
    secondPage.nextSourceIndex
  );
  expect(sourceIds(thirdPage.requests)).toEqual([12]);
  expect(thirdPage.nextSourceIndex).toBe(0);
});
