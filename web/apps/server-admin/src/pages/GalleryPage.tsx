import {
  getAdminGalleryMapConfiguration,
  getAdminVersionGraph,
  listAdminSnapshots,
  listAdminStoreEntries,
  restoreAdminStoreVersion,
  retryAdminMediaCacheEntry
} from "@ironmesh/api";
import {
  GallerySurface,
  galleryBasemapsFromConfiguration,
  galleryMapConfigurationQueryPolicy,
  galleryQueryKeys,
  type GalleryDataSource
} from "@ironmesh/ui";
import { Stack, Tabs } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { MapDatasetImportCard } from "../components/MapDatasetImportCard";
import { MapVariantConfigurationCard } from "../components/MapVariantConfigurationCard";
import { useAdminAccess } from "../lib/admin-access";

const MOBILE_VIEWER_THUMBNAIL_PROFILE = "mobile_viewer";

export function GalleryPage() {
  const { sessionStatus, sessionLoading } = useAdminAccess();
  const loginRequired = sessionStatus?.login_required ?? true;
  const canInspectMapConfiguration =
    !sessionLoading && (!loginRequired || Boolean(sessionStatus?.authenticated));
  const mapConfigurationQuery = useQuery({
    queryKey: galleryQueryKeys.mapConfiguration(),
    queryFn: () => getAdminGalleryMapConfiguration(),
    ...galleryMapConfigurationQueryPolicy,
    enabled: canInspectMapConfiguration
  });
  const mapConfiguration = mapConfigurationQuery.data ?? null;
  const mapConfigurationError = errorMessage(mapConfigurationQuery.error);
  const basemaps = galleryBasemapsFromConfiguration(
    mapConfiguration?.configuration.variants ?? []
  );

  const galleryDataSource = useMemo<GalleryDataSource>(
    () => ({
      loadSnapshots: () => listAdminSnapshots(),
      loadEntries: (prefix, depth, snapshotId, options) =>
        listAdminStoreEntries(prefix, depth, snapshotId, undefined, options),
      loadVersions: (key) => getAdminVersionGraph(key),
      getMediaRequests: (entry, snapshotId, versionId) => {
        const thumbnailUrl = entry.media?.thumbnail?.url ?? null;
        const original = {
          url: adminBinaryObjectUrl(entry.path, snapshotId, versionId)
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
                  url: withThumbnailProfile(thumbnailUrl, MOBILE_VIEWER_THUMBNAIL_PROFILE)
                }
              : null,
          original
        };
      },
      restoreVersion: (key, versionId, targetPath) =>
        restoreAdminStoreVersion(key, versionId, targetPath),
      retryMediaEntry: (entry, snapshotId) =>
        retryAdminMediaCacheEntry(entry.path, undefined, {
          snapshot: snapshotId,
          version: typeof entry.version === "string" ? entry.version : null
        })
    }),
    []
  );
  return (
    <Tabs defaultValue="gallery">
      <Tabs.List aria-label="Gallery sections">
        <Tabs.Tab value="gallery">Gallery</Tabs.Tab>
        <Tabs.Tab value="map-configuration">Map configuration</Tabs.Tab>
      </Tabs.List>

      <Tabs.Panel value="gallery" pt="lg">
        <GallerySurface
          intro="Browse the node-side store index through admin-authenticated snapshot, index, and media routes. The gallery stays shared with the client surface and uses the current admin session for protected previews."
          previewHint="Only indexed thumbnail URLs are used for gallery cards and movie posters. Missing thumbnails stay visible in the UI so pending or failed media processing is obvious."
          allowedMediaKinds={["image", "video"]}
          basemaps={basemaps}
          preferredBasemapId={mapConfiguration?.configuration.active_variant_id}
          basemapConfigurationLoading={mapConfigurationQuery.isLoading}
          basemapConfigurationError={mapConfigurationError}
          retryBasemapConfiguration={() => void mapConfigurationQuery.refetch()}
          dataSource={galleryDataSource}
        />
      </Tabs.Panel>

      <Tabs.Panel value="map-configuration" pt="lg">
        <Stack gap="lg">
          <MapVariantConfigurationCard
            configuration={mapConfiguration?.configuration ?? null}
            stored={mapConfiguration?.stored ?? null}
            loading={mapConfigurationQuery.isLoading}
            error={mapConfigurationQuery.error}
          />
          <MapDatasetImportCard />
        </Stack>
      </Tabs.Panel>
    </Tabs>
  );
}

function errorMessage(error: unknown): string | null {
  return error instanceof Error && error.message.trim() ? error.message : null;
}

function adminBinaryObjectUrl(
  key: string,
  snapshotId: string | null,
  versionId?: string | null
): string {
  const query = new URLSearchParams();
  if (snapshotId) {
    query.set("snapshot", snapshotId);
  }
  if (versionId?.trim()) {
    query.set("version", versionId.trim());
  }
  const suffix = query.toString() ? `?${query.toString()}` : "";
  return `/api/v1/auth/store/${encodeURIComponent(key)}${suffix}`;
}

function withThumbnailProfile(url: string, profile: string): string {
  const baseOrigin = typeof window === "undefined" ? "http://localhost" : window.location.origin;
  const resolved = new URL(url, baseOrigin);
  resolved.searchParams.set("profile", profile);
  return `${resolved.pathname}${resolved.search}${resolved.hash}`;
}
