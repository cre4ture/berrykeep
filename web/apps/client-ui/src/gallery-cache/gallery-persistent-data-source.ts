import { getClientCacheContext } from "@ironmesh/api";
import type {
  GalleryDataSource,
  GalleryDataUpdate,
  GalleryLoadEntriesOptions,
  GalleryPayload,
  GallerySnapshot
} from "@ironmesh/ui";
import type { QueryClient, QueryKey } from "@tanstack/react-query";
import {
  GalleryPersistentCache,
  type GalleryCacheRecordKind
} from "./gallery-persistent-cache";

type CacheContext = {
  scope: string;
};

type ResourceDescriptor =
  | { kind: "snapshots" }
  | {
      kind: "entries";
      prefix: string;
      depth: number;
      snapshotId: string | null;
      view: "raw" | "tree";
      offset: number | null;
      limit: number | null;
      sort: GalleryLoadEntriesOptions["sort"] | null;
      mediaFilter: GalleryLoadEntriesOptions["mediaFilter"] | null;
    };

type ResourceConfig<T> = {
  descriptor: ResourceDescriptor;
  fetcher: () => Promise<T>;
  validate: (payload: unknown) => payload is T;
};

const CACHE_SCOPE_PATTERN = /^[a-f0-9]{64}$/;

/**
 * Adds persistent stale-while-revalidate behavior without moving transport or
 * authentication concerns into the shared GallerySurface.
 */
export function createPersistentGalleryDataSource(
  queryClient: QueryClient,
  liveDataSource: GalleryDataSource,
  cache = new GalleryPersistentCache()
): GalleryDataSource {
  const listeners = new Set<(update: GalleryDataUpdate) => void>();
  const revalidationGeneration = new Map<GalleryCacheRecordKind, number>([
    ["snapshots", 0],
    ["entries", 0]
  ]);
  const revalidatedKeys = new Map<string, number>();
  const networkRequests = new Map<string, Promise<unknown>>();
  const pendingUpdates = new Map<string, GalleryDataUpdate>();
  let updateScheduled = false;
  const cacheContextPromise = resolveCacheContext();

  function emitUpdate(descriptor: ResourceDescriptor, payload: unknown) {
    const update = galleryDataUpdate(descriptor, payload);
    if (!update) {
      return;
    }
    pendingUpdates.set(JSON.stringify(descriptor), update);
    if (updateScheduled) {
      return;
    }
    updateScheduled = true;
    queueMicrotask(() => {
      updateScheduled = false;
      const updates = [...pendingUpdates.values()];
      pendingUpdates.clear();
      for (const update of updates) {
        for (const listener of listeners) {
          listener(update);
        }
      }
    });
  }

  async function loadResource<T>(config: ResourceConfig<T>): Promise<T> {
    const context = await cacheContextPromise;
    if (!context) {
      return config.fetcher();
    }

    const cacheKey = JSON.stringify(config.descriptor);
    const queryKey: QueryKey = ["gallery", "persistent", context.scope, config.descriptor];
    const generation = revalidationGeneration.get(config.descriptor.kind) ?? 0;
    const memoryValue = queryClient.getQueryData(queryKey);
    if (config.validate(memoryValue)) {
      revalidateInBackground(config, context, cacheKey, queryKey, generation);
      return memoryValue;
    }
    if (memoryValue !== undefined) {
      queryClient.removeQueries({ queryKey, exact: true });
    }

    const cached = await cache.read(context.scope, cacheKey, config.validate);
    if (cached) {
      queryClient.setQueryData(queryKey, cached.payload, { updatedAt: cached.updatedAt });
      revalidateInBackground(config, context, cacheKey, queryKey, generation);
      return cached.payload;
    }

    revalidatedKeys.set(cacheKey, generation);
    return fetchAndPersist(config, context, cacheKey, queryKey);
  }

  function revalidateInBackground<T>(
    config: ResourceConfig<T>,
    context: CacheContext,
    cacheKey: string,
    queryKey: QueryKey,
    generation: number
  ) {
    if ((revalidatedKeys.get(cacheKey) ?? -1) >= generation) {
      return;
    }
    revalidatedKeys.set(cacheKey, generation);
    void fetchAndPersist(config, context, cacheKey, queryKey)
      .then((payload) => emitUpdate(config.descriptor, payload))
      .catch(() => {
        // SWR keeps the last validated payload visible while offline. A user
        // refresh increments the generation and permits an explicit retry.
      });
  }

  function fetchAndPersist<T>(
    config: ResourceConfig<T>,
    context: CacheContext,
    cacheKey: string,
    queryKey: QueryKey
  ): Promise<T> {
    const existing = networkRequests.get(cacheKey) as Promise<T> | undefined;
    if (existing) {
      return existing;
    }

    const request = queryClient
      .fetchQuery<T>({
        queryKey,
        staleTime: 0,
        queryFn: async () => {
          const payload = await config.fetcher();
          if (!config.validate(payload)) {
            throw new Error("Gallery endpoint returned an invalid payload");
          }
          await cache.write(context.scope, cacheKey, config.descriptor.kind, payload);
          return payload;
        }
      })
      .finally(() => networkRequests.delete(cacheKey));
    networkRequests.set(cacheKey, request);
    return request;
  }

  return {
    ...liveDataSource,
    loadSnapshots: () =>
      loadResource({
        descriptor: { kind: "snapshots" },
        fetcher: liveDataSource.loadSnapshots,
        validate: isGallerySnapshotList
      }),
    loadEntries: (prefix, depth, snapshotId, options) =>
      loadResource({
        descriptor: entryDescriptor(prefix, depth, snapshotId, options),
        fetcher: () => liveDataSource.loadEntries(prefix, depth, snapshotId, options),
        validate: isGalleryPayload
      }),
    requestRevalidation: (kind) => {
      revalidationGeneration.set(kind, (revalidationGeneration.get(kind) ?? 0) + 1);
    },
    subscribeToUpdates: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    }
  };
}

function galleryDataUpdate(
  descriptor: ResourceDescriptor,
  payload: unknown
): GalleryDataUpdate | null {
  if (descriptor.kind === "snapshots") {
    return { kind: "snapshots" };
  }
  if (!isGalleryPayload(payload)) {
    return null;
  }
  return {
    kind: "entries",
    prefix: descriptor.prefix,
    depth: descriptor.depth,
    snapshotId: descriptor.snapshotId,
    options: {
      view: descriptor.view,
      ...(descriptor.offset === null ? {} : { offset: descriptor.offset }),
      ...(descriptor.limit === null ? {} : { limit: descriptor.limit }),
      ...(descriptor.sort === null ? {} : { sort: descriptor.sort }),
      ...(descriptor.mediaFilter === null ? {} : { mediaFilter: descriptor.mediaFilter })
    },
    payload
  };
}

async function resolveCacheContext(): Promise<CacheContext | null> {
  try {
    const context = await getClientCacheContext();
    if (context.schema_version !== 1 || !context.scope || !CACHE_SCOPE_PATTERN.test(context.scope)) {
      return null;
    }
    return { scope: context.scope };
  } catch {
    return null;
  }
}

function entryDescriptor(
  prefix: string,
  depth: number,
  snapshotId: string | null,
  options: GalleryLoadEntriesOptions = {}
): ResourceDescriptor {
  return {
    kind: "entries",
    prefix: prefix.trim(),
    depth: Math.max(1, Math.floor(depth)),
    snapshotId: snapshotId?.trim() || null,
    view: options.view ?? "tree",
    offset:
      typeof options.offset === "number" && Number.isFinite(options.offset)
        ? Math.max(0, Math.floor(options.offset))
        : null,
    limit:
      typeof options.limit === "number" && Number.isFinite(options.limit)
        ? Math.max(1, Math.floor(options.limit))
        : null,
    sort: options.sort ?? null,
    mediaFilter: options.mediaFilter ?? null
  };
}

function isGallerySnapshotList(payload: unknown): payload is GallerySnapshot[] {
  return (
    Array.isArray(payload) &&
    payload.every(
      (snapshot) =>
        typeof snapshot === "object" &&
        snapshot !== null &&
        typeof (snapshot as { id?: unknown }).id === "string"
    )
  );
}

function isGalleryPayload(payload: unknown): payload is GalleryPayload {
  if (typeof payload !== "object" || payload === null) {
    return false;
  }
  const candidate = payload as Partial<GalleryPayload>;
  return (
    typeof candidate.prefix === "string" &&
    typeof candidate.depth === "number" &&
    typeof candidate.entry_count === "number" &&
    Array.isArray(candidate.entries) &&
    candidate.entries.every(
      (entry) =>
        typeof entry === "object" &&
        entry !== null &&
        typeof entry.path === "string" &&
        typeof entry.entry_type === "string"
    )
  );
}
