package io.ironmesh.android.ui

import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.TitleLatencyProbeStatus
import io.ironmesh.android.ui.screens.connectionRouteSnapshotsUiEquivalent

internal data class FolderSyncPollResult(
    val folderSyncStatus: FolderSyncServiceStatus,
    val globalFolderSyncStatus: GlobalFolderSyncStatus,
)

internal fun canApplyFolderSyncPollResult(
    stateSnapshot: MainUiState,
    currentState: MainUiState,
): Boolean {
    return currentState.syncProfiles == stateSnapshot.syncProfiles &&
        currentState.deviceIdentity == stateSnapshot.deviceIdentity &&
        currentState.globalFolderSyncStatus == stateSnapshot.globalFolderSyncStatus
}

internal fun MainUiState.withFolderSyncPollResult(result: FolderSyncPollResult): MainUiState {
    val stableFolderSyncStatus = result.folderSyncStatus.withStableTimestampFrom(folderSyncStatus)
    if (
        folderSyncStatus == stableFolderSyncStatus &&
        globalFolderSyncStatus == result.globalFolderSyncStatus
    ) {
        return this
    }
    return copy(
        folderSyncStatus = stableFolderSyncStatus,
        globalFolderSyncStatus = result.globalFolderSyncStatus,
    )
}

private fun FolderSyncServiceStatus.withStableTimestampFrom(
    previous: FolderSyncServiceStatus,
): FolderSyncServiceStatus {
    return if (copy(updatedUnixMs = 0L) == previous.copy(updatedUnixMs = 0L)) {
        copy(updatedUnixMs = previous.updatedUnixMs)
    } else {
        this
    }
}

internal fun MainUiState.withTitleLatencyPollResult(
    result: TitleLatencyProbeStatus,
): MainUiState {
    return if (titleLatencyStatus == result) this else copy(titleLatencyStatus = result)
}

internal fun MainUiState.withConnectionRoutePollResult(
    result: ConnectionRouteSnapshot,
    loadedAtUnixMs: Long,
): MainUiState {
    if (
        connectionRouteSnapshotsUiEquivalent(connectionRoutes, result) &&
        connectionRoutes?.generatedAtUnixMs?.toDisplayedMinute() ==
        result.generatedAtUnixMs.toDisplayedMinute() &&
        !connectionRoutesLoading &&
        connectionRoutesError == null
    ) {
        return this
    }
    return copy(
        connectionRoutes = result,
        connectionRoutesLoading = false,
        connectionRoutesError = null,
        connectionRoutesLastLoadedUnixMs = loadedAtUnixMs,
    )
}

private fun Long.toDisplayedMinute(): Long = this / 60_000L
