package io.ironmesh.android.ui

import androidx.compose.runtime.Immutable
import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.data.FolderSyncConfig
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.TitleLatencyMonitorSettings

@Immutable
data class HomeUiState(
    val isEnrolled: Boolean,
    val syncProfileCount: Int,
    val folderSyncStatus: FolderSyncServiceStatus,
    val globalFolderSyncStatus: GlobalFolderSyncStatus,
    val appConnectionStatus: AppConnectionStatus,
)

internal fun MainUiState.toHomeUiState(): HomeUiState =
    HomeUiState(
        isEnrolled = deviceIdentity.isEnrolled,
        syncProfileCount = syncProfiles.size,
        folderSyncStatus = folderSyncStatus,
        globalFolderSyncStatus = globalFolderSyncStatus,
        appConnectionStatus = appConnectionStatus,
    )

@Immutable
data class SyncUiState(
    val profiles: List<FolderSyncConfig>,
    val folderSyncStatus: FolderSyncServiceStatus,
    val globalFolderSyncStatus: GlobalFolderSyncStatus,
    val historyByProfileId: Map<String, FolderSyncHistoryState>,
    val newProfileLabel: String,
    val newProfilePrefix: String,
    val newProfileLocalFolder: String,
    val newProfileAllowWifi: Boolean,
    val newProfileAllowCellular: Boolean,
    val newProfileAllowOtherConnections: Boolean,
    val newProfileAllowRoaming: Boolean,
    val newProfileAllowedWifiSsids: String,
)

internal fun MainUiState.toSyncUiState(): SyncUiState =
    SyncUiState(
        profiles = syncProfiles,
        folderSyncStatus = folderSyncStatus,
        globalFolderSyncStatus = globalFolderSyncStatus,
        historyByProfileId = folderSyncHistory,
        newProfileLabel = newSyncLabel,
        newProfilePrefix = newSyncPrefix,
        newProfileLocalFolder = newSyncLocalFolder,
        newProfileAllowWifi = newSyncAllowWifi,
        newProfileAllowCellular = newSyncAllowCellular,
        newProfileAllowOtherConnections = newSyncAllowOtherConnections,
        newProfileAllowRoaming = newSyncAllowRoaming,
        newProfileAllowedWifiSsids = newSyncAllowedWifiSsids,
    )

@Immutable
data class LibraryUiState(
    val mode: GalleryViewMode,
    val collection: GalleryCollectionState?,
    val pages: Map<Int, GalleryPageState>,
    val directories: List<GalleryDirectoryItem>,
    val breadcrumbs: List<GalleryBreadcrumbItem>,
    val currentDirectoryPath: String,
    val sort: GallerySortOption,
    val loading: Boolean,
    val error: GalleryLoadError?,
)

internal fun MainUiState.toLibraryUiState(): LibraryUiState =
    LibraryUiState(
        mode = galleryMode,
        collection = galleryCollection,
        pages = galleryPages,
        directories = galleryDirectories,
        breadcrumbs = galleryBreadcrumbs,
        currentDirectoryPath = galleryCurrentDirectoryPath,
        sort = gallerySort,
        loading = galleryLoading,
        error = galleryError,
    )

@Immutable
data class ConnectivityUiState(
    val routes: ConnectionRouteSnapshot?,
    val loading: Boolean,
    val error: String?,
)

internal fun MainUiState.toConnectivityUiState(): ConnectivityUiState =
    ConnectivityUiState(
        routes = connectionRoutes,
        loading = connectionRoutesLoading,
        error = connectionRoutesError,
    )

@Immutable
data class RequestTimingsUiState(
    val routes: ConnectionRouteSnapshot?,
    val loading: Boolean,
    val error: String?,
    val measurementResetting: Boolean,
    val storeIndexTestRunning: Boolean,
    val measurementStartedUnixMs: Long?,
)

internal fun MainUiState.toRequestTimingsUiState(): RequestTimingsUiState =
    RequestTimingsUiState(
        routes = connectionRoutes,
        loading = connectionRoutesLoading,
        error = connectionRoutesError,
        measurementResetting = timingMeasurementResetting,
        storeIndexTestRunning = timingStoreIndexTestRunning,
        measurementStartedUnixMs = timingMeasurementStartedUnixMs,
    )

@Immutable
data class SettingsUiState(
    val deviceIdentity: DeviceIdentityUiState,
    val themeAccentColorHex: String,
    val titleLatencyMonitorSettings: TitleLatencyMonitorSettings,
    val key: String,
    val payload: String,
)

internal fun MainUiState.toSettingsUiState(): SettingsUiState =
    SettingsUiState(
        deviceIdentity = deviceIdentity,
        themeAccentColorHex = themeAccentColorHex,
        titleLatencyMonitorSettings = titleLatencyMonitorSettings,
        key = key,
        payload = payload,
    )

@Immutable
data class GalleryMapUiState(
    val webUiSession: EmbeddedWebUiSession?,
    val loading: Boolean,
    val status: String,
)

internal fun MainUiState.toGalleryMapUiState(): GalleryMapUiState =
    GalleryMapUiState(
        webUiSession = webUiSession,
        loading = loading,
        status = status,
    )

@Immutable
data class OnboardingUiState(
    val bootstrapInput: String,
    val deviceLabelInput: String,
    val enrollmentDiagnostics: List<EnrollmentDiagnosticStep>,
    val loading: Boolean,
)

internal fun MainUiState.toOnboardingUiState(): OnboardingUiState =
    OnboardingUiState(
        bootstrapInput = bootstrapInput,
        deviceLabelInput = deviceLabelInput,
        enrollmentDiagnostics = enrollmentDiagnostics,
        loading = loading,
    )
