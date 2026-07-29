const GALLERY_QUERY_ROOT = ["gallery"] as const;

/**
 * Shared gallery query names. Individual applications still own separate
 * QueryClients, which keeps their authenticated data isolated.
 */
export const galleryQueryKeys = {
  all: GALLERY_QUERY_ROOT,
  mapConfiguration: () => [...GALLERY_QUERY_ROOT, "map-configuration"] as const
};

/**
 * The map configuration changes infrequently and the server refreshes its
 * metadata on a short cadence. Keep it warm in memory without making it a
 * source of cross-application state.
 */
export const galleryMapConfigurationQueryPolicy = {
  staleTime: 15_000,
  gcTime: 5 * 60_000,
  refetchInterval: 15_000,
  refetchOnWindowFocus: false,
  retry: false
} as const;
