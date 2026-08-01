import {
  getClientGalleryMapConfiguration,
  getBinaryObjectStreamUrl,
  getVersionGraph,
  listSnapshots,
  listStoreEntries,
  restoreStoreVersion,
  retryStoreMediaCacheEntry
} from "@ironmesh/api";
import {
  GallerySurface,
  galleryBasemapsFromConfiguration,
  galleryMapConfigurationQueryPolicy,
  galleryQueryKeys,
  PageHeader,
  type GalleryDataSource,
  type GallerySurfaceViewMode
} from "@ironmesh/ui";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { createPersistentGalleryDataSource } from "../gallery-cache/gallery-persistent-data-source";

const MOBILE_VIEWER_THUMBNAIL_PROFILE = "mobile_viewer";

type GalleryPageProps = {
  initialViewMode?: GallerySurfaceViewMode;
};

type ClientGalleryMapConfiguration = Awaited<ReturnType<typeof getClientGalleryMapConfiguration>>;

export function GalleryPage({ initialViewMode }: GalleryPageProps = {}) {
  const queryClient = useQueryClient();
  const mapConfigurationQuery = useQuery<ClientGalleryMapConfiguration>({
    queryKey: galleryQueryKeys.mapConfiguration(),
    queryFn: getClientGalleryMapConfiguration,
    ...galleryMapConfigurationQueryPolicy,
    structuralSharing: retainEquivalentMapConfiguration
  });
  const mapConfiguration = mapConfigurationQuery.data ?? null;
  const mapConfigurationError = mapConfigurationErrorMessage(mapConfigurationQuery.error);
  const basemaps = useMemo(
    () => galleryBasemapsFromConfiguration(mapConfiguration?.configuration.variants ?? []),
    [mapConfiguration]
  );
  const liveGalleryDataSource = useMemo<GalleryDataSource>(
    () => ({
      loadSnapshots: () => listSnapshots(),
      loadEntries: (prefix, depth, snapshotId, options) =>
        listStoreEntries(prefix, depth, snapshotId, options),
      getMediaRequests: (entry, snapshotId, versionId) => {
        const thumbnailUrl = entry.media?.thumbnail?.url ?? null;
        return {
          thumbnail: thumbnailUrl
            ? {
                url: thumbnailUrl
              }
            : null,
          fullscreen:
            thumbnailUrl && entry.media?.media_type !== "video"
              ? {
                  url: withThumbnailProfile(thumbnailUrl, MOBILE_VIEWER_THUMBNAIL_PROFILE)
                }
              : null,
          original: {
            url: binaryMediaUrl(entry.path, snapshotId, versionId)
          }
        };
      },
      loadVersions: getVersionGraph,
      restoreVersion: (key, versionId, targetPath) =>
        restoreStoreVersion(key, versionId, targetPath),
      retryMediaEntry: (entry, snapshotId) =>
        retryStoreMediaCacheEntry(entry.path, {
          snapshot: snapshotId,
          version: typeof entry.version === "string" ? entry.version : null
        })
    }),
    []
  );
  const galleryDataSource = useMemo(
    () => createPersistentGalleryDataSource(queryClient, liveGalleryDataSource),
    [liveGalleryDataSource, queryClient]
  );

  return (
    <>
      <PageHeader
        title="Gallery"
        description="Browse photo and movie objects through the client web backend with shared media-aware gallery tooling."
      />
      <GallerySurface
        previewHint="Only indexed thumbnail URLs are used for gallery cards and movie posters. Missing thumbnails stay visible in the UI so pending or failed media processing is obvious."
        initialViewMode={initialViewMode}
        allowedMediaKinds={["image", "video"]}
        basemaps={basemaps}
        preferredBasemapId={mapConfiguration?.configuration.active_variant_id}
        basemapConfigurationLoading={mapConfigurationQuery.isLoading}
        basemapConfigurationError={mapConfigurationError}
        retryBasemapConfiguration={() => void mapConfigurationQuery.refetch()}
        dataSource={galleryDataSource}
      />
    </>
  );
}

function mapConfigurationErrorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }

  return "The gallery map configuration could not be loaded.";
}

function sameMapConfiguration(
  current: ClientGalleryMapConfiguration | null,
  next: ClientGalleryMapConfiguration
): boolean {
  // `stored` communicates server-side initialization state only; it has no
  // effect on gallery rendering. The API serializes configuration fields in a
  // stable order, so this also compares nested variants without an additional
  // dependency for deep equality.
  return (
    current !== null &&
    JSON.stringify(current.configuration) === JSON.stringify(next.configuration)
  );
}

function retainEquivalentMapConfiguration(current: unknown, next: unknown): unknown {
  // TanStack intentionally accepts unknown cache values here because callers
  // may set query data manually. This query's fetcher always returns the
  // client map-configuration response.
  const currentConfiguration = (current as ClientGalleryMapConfiguration | undefined) ?? null;
  const nextConfiguration = next as ClientGalleryMapConfiguration;
  return sameMapConfiguration(currentConfiguration, nextConfiguration) ? current : next;
}

function binaryMediaUrl(
  key: string,
  snapshotId: string | null,
  versionId?: string | null
): string {
  return getBinaryObjectStreamUrl(key, snapshotId, versionId);
}

function withThumbnailProfile(url: string, profile: string): string {
  const baseOrigin = typeof window === "undefined" ? "http://localhost" : window.location.origin;
  const resolved = new URL(url, baseOrigin);
  resolved.searchParams.set("profile", profile);
  return `${resolved.pathname}${resolved.search}${resolved.hash}`;
}
