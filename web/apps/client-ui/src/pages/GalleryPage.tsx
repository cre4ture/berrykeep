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
  MOBILE_VIEWER_THUMBNAIL_PROFILE,
  PageHeader,
  withMediaThumbnailProfile,
  type GalleryDataSource,
  type GalleryEntry,
  type GalleryMediaRequests,
  type GallerySurfaceViewMode
} from "@ironmesh/ui";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { createPersistentGalleryDataSource } from "../gallery-cache/gallery-persistent-data-source";

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
  const mapConfigurationError = mapConfigurationQuery.isError
    ? mapConfigurationErrorMessage(mapConfigurationQuery.error)
    : null;
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
        const original = {
          url: binaryMediaUrl(entry.path, snapshotId, versionId)
        };
        return {
          thumbnail: thumbnailUrl
            ? {
                url: thumbnailUrl
              }
            : null,
          fullscreen:
            thumbnailUrl && entry.media?.media_type !== "video"
              ? {
                  url: withMediaThumbnailProfile(thumbnailUrl, MOBILE_VIEWER_THUMBNAIL_PROFILE)
                }
              : null,
          original,
          download: original,
          share: immutableMediaShareRequest(entry, snapshotId, versionId)
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

function immutableMediaShareRequest(
  entry: GalleryEntry,
  snapshotId: string | null,
  versionId?: string | null
): GalleryMediaRequests["share"] {
  const snapshot = snapshotId?.trim() || null;
  const version = snapshot ? null : versionId?.trim() || entry.version?.trim() || null;
  if (!snapshot && !version) {
    return null;
  }

  return {
    key: entry.path,
    snapshotId: snapshot,
    versionId: version,
    fileName: entry.path.split(/[\\/]/).filter(Boolean).at(-1) || "original",
    mimeType: entry.media?.mime_type ?? null,
    sizeBytes: entry.size_bytes ?? null
  };
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
