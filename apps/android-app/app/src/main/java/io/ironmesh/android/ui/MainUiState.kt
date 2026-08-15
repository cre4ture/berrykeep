package io.ironmesh.android.ui

import android.net.Uri
import androidx.compose.runtime.Immutable
import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.data.FolderSyncConfig
import io.ironmesh.android.data.FolderSyncModificationRecord
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.TitleLatencyMonitorSettings
import io.ironmesh.android.data.TitleLatencyProbeStatus
import io.ironmesh.android.ui.theme.DEFAULT_IRONMESH_ACCENT_COLOR_HEX

enum class GallerySortOption {
    CREATION_TIME,
    NAME,
}

enum class GalleryViewMode {
    FLATTENED_ALL_IMAGES,
    CURRENT_DIRECTORY,
}

enum class MainSection {
    HOME,
    CONNECTIVITY,
    REQUEST_TIMINGS,
    SYNC,
    LIBRARY,
    GALLERY_MAP,
    SETTINGS,
}

enum class FolderSyncActivityFilter {
    ALL,
    UPLOADS,
    DOWNLOADS,
    DELETES,
}

@Immutable
data class FolderSyncHistoryState(
    val expanded: Boolean = false,
    val records: List<FolderSyncModificationRecord> = emptyList(),
    val nextBeforeId: Long? = null,
    val filter: FolderSyncActivityFilter = FolderSyncActivityFilter.ALL,
    val loading: Boolean = false,
    val error: String? = null,
    val lastLoadedElapsedRealtimeMs: Long = 0L,
)

@Immutable
data class GalleryImageItem(
    val documentUri: Uri,
    val displayName: String,
    val remotePath: String,
    val mimeType: String,
    val createdAtUnixMs: Long? = null,
    val width: Int? = null,
    val height: Int? = null,
    val thumbnailStatus: String? = null,
)

@Immutable
data class GalleryDirectoryItem(
    val documentId: String,
    val displayName: String,
    val pathLabel: String,
)

@Immutable
data class GalleryBreadcrumbItem(
    val documentId: String,
    val label: String,
    val pathLabel: String,
)

@Immutable
data class GalleryCollectionState(
    val totalItemCount: Int,
    val pageSize: Int,
    val pageCount: Int,
)

enum class GalleryPageStatus {
    LOADING,
    READY,
    ERROR,
}

@Immutable
data class GalleryPageState(
    val status: GalleryPageStatus,
    val items: List<GalleryImageItem> = emptyList(),
    val error: String? = null,
)

enum class GalleryLoadErrorKind {
    TIMEOUT,
    REQUEST_FAILED,
}

@Immutable
data class GalleryLoadError(
    val kind: GalleryLoadErrorKind,
    val technicalDetail: String? = null,
)

@Immutable
data class MainUiState(
    val persistedStateLoaded: Boolean = false,
    val deviceIdentity: DeviceIdentityUiState = DeviceIdentityUiState(),
    val bootstrapInput: String = "",
    val deviceLabelInput: String = "",
    val enrollmentDiagnostics: List<EnrollmentDiagnosticStep> = emptyList(),
    val key: String = "demo-key",
    val payload: String = "hello from android",
    val status: String = "Ready",
    val objectBody: String = "",
    val syncProfiles: List<FolderSyncConfig> = emptyList(),
    val folderSyncStatus: FolderSyncServiceStatus = FolderSyncServiceStatus(),
    val globalFolderSyncStatus: GlobalFolderSyncStatus = GlobalFolderSyncStatus(),
    val appConnectionStatus: AppConnectionStatus = AppConnectionStatus(),
    val titleLatencyMonitorSettings: TitleLatencyMonitorSettings = TitleLatencyMonitorSettings(),
    val titleLatencyStatus: TitleLatencyProbeStatus = TitleLatencyProbeStatus(),
    val folderSyncHistory: Map<String, FolderSyncHistoryState> = emptyMap(),
    val newSyncLabel: String = "",
    val newSyncPrefix: String = "",
    val newSyncLocalFolder: String = "",
    val newSyncLocalFolderTreeUri: String? = null,
    val newSyncAllowWifi: Boolean = true,
    val newSyncAllowCellular: Boolean = true,
    val newSyncAllowOtherConnections: Boolean = true,
    val newSyncAllowRoaming: Boolean = false,
    val newSyncAllowedWifiSsids: String = "",
    val selectedSection: MainSection = MainSection.HOME,
    val connectionRoutes: ConnectionRouteSnapshot? = null,
    val nodePriorityOverrides: Map<String, Int> = emptyMap(),
    val connectionRoutesLoading: Boolean = false,
    val connectionRoutesError: String? = null,
    val connectionRoutesLastLoadedUnixMs: Long = 0L,
    val timingMeasurementResetting: Boolean = false,
    val timingStoreIndexTestRunning: Boolean = false,
    val timingMeasurementStartedUnixMs: Long? = null,
    val webUiSession: EmbeddedWebUiSession? = null,
    val galleryMode: GalleryViewMode = GalleryViewMode.FLATTENED_ALL_IMAGES,
    val galleryCollection: GalleryCollectionState? = null,
    val galleryPages: Map<Int, GalleryPageState> = emptyMap(),
    val galleryDirectories: List<GalleryDirectoryItem> = emptyList(),
    val galleryBreadcrumbs: List<GalleryBreadcrumbItem> = emptyList(),
    val galleryCurrentDirectoryDocumentId: String = "dir:",
    val galleryCurrentDirectoryPath: String = "/",
    val gallerySort: GallerySortOption = GallerySortOption.CREATION_TIME,
    val themeAccentColorHex: String = DEFAULT_IRONMESH_ACCENT_COLOR_HEX,
    val galleryLoading: Boolean = false,
    val galleryError: GalleryLoadError? = null,
    val loading: Boolean = false,
)
