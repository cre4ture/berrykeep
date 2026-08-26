package io.ironmesh.android.ui

import androidx.compose.runtime.Immutable
import io.ironmesh.android.data.FolderSyncNetworkPolicy

@Immutable
data class SyncScreenActions(
    val runNow: () -> Unit,
    val updateNewProfileLabel: (String) -> Unit,
    val updateNewProfilePrefix: (String) -> Unit,
    val updateNewProfileLocalFolder: (String) -> Unit,
    val updateNewProfileAllowWifi: (Boolean) -> Unit,
    val updateNewProfileAllowCellular: (Boolean) -> Unit,
    val updateNewProfileAllowOtherConnections: (Boolean) -> Unit,
    val updateNewProfileAllowRoaming: (Boolean) -> Unit,
    val updateNewProfileAllowedWifiSsids: (String) -> Unit,
    val addProfile: () -> FolderSyncNetworkPolicy?,
    val toggleHistory: (String) -> Unit,
    val setProfileEnabled: (String, Boolean) -> Unit,
    val removeProfile: (String) -> Unit,
    val setHistoryFilter: (String, FolderSyncActivityFilter) -> Unit,
    val loadMoreHistory: (String) -> Unit,
    val updateProfileNetworkPolicy: (String, FolderSyncNetworkPolicy) -> Boolean,
)

@Immutable
data class LibraryScreenActions(
    val updateVisiblePages: (Set<Int>) -> Unit,
    val refresh: () -> Unit,
    val updateViewMode: (GalleryViewMode) -> Unit,
    val updateSort: (GallerySortOption) -> Unit,
    val updateShowSensitiveContent: (Boolean) -> Unit,
    val navigateToRoot: () -> Unit,
    val navigateUp: () -> Unit,
    val navigateToBreadcrumb: (Int) -> Unit,
    val openDirectory: (GalleryDirectoryItem) -> Unit,
    val retryPage: (Int) -> Unit,
    val itemAt: (Int) -> GalleryImageItem?,
    val ensureItemLoaded: (Int) -> Unit,
    val pinItem: (Int?) -> Unit,
    val toggleMediaLabel: (GalleryImageItem, String) -> Unit,
)
