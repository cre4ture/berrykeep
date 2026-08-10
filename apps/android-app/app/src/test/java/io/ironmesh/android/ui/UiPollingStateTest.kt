package io.ironmesh.android.ui

import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.TitleLatencyProbeStatus
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertSame
import org.junit.Test

class UiPollingStateTest {
    @Test
    fun folderSyncPollTimestampAloneDoesNotCreateNewUiState() {
        val state = MainUiState(
            folderSyncStatus = FolderSyncServiceStatus(updatedUnixMs = 1_000L),
            globalFolderSyncStatus = GlobalFolderSyncStatus(updatedUnixMs = 1_000L),
        )

        val result = state.withFolderSyncPollResult(
            FolderSyncPollResult(
                folderSyncStatus = state.folderSyncStatus.copy(updatedUnixMs = 2_000L),
                globalFolderSyncStatus = state.globalFolderSyncStatus,
            ),
        )

        assertSame(state, result)
    }

    @Test
    fun identicalTitleLatencyPollResultDoesNotCreateNewUiState() {
        val status = TitleLatencyProbeStatus(
            state = "ready",
            latencyMs = 12.5,
            checkedAtUnixMs = 1_000L,
        )
        val state = MainUiState(titleLatencyStatus = status)

        assertSame(state, state.withTitleLatencyPollResult(status))
    }

    @Test
    fun routePollTimestampAloneDoesNotCreateNewUiState() {
        val state = MainUiState(
            connectionRoutes = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                endpoints = emptyList(),
            ),
            connectionRoutesLastLoadedUnixMs = 1_000L,
        )
        val nextPoll = ConnectionRouteSnapshot(
            generatedAtUnixMs = 2_000L,
            endpoints = emptyList(),
        )

        val result = state.withConnectionRoutePollResult(
            result = nextPoll,
            loadedAtUnixMs = 2_000L,
        )

        assertSame(state, result)
    }

    @Test
    fun routePollUpdatesLastCheckedWhenDisplayedMinuteChanges() {
        val state = MainUiState(
            connectionRoutes = ConnectionRouteSnapshot(
                generatedAtUnixMs = 59_000L,
                endpoints = emptyList(),
            ),
            connectionRoutesLastLoadedUnixMs = 59_000L,
        )
        val nextPoll = ConnectionRouteSnapshot(
            generatedAtUnixMs = 60_000L,
            endpoints = emptyList(),
        )

        val result = state.withConnectionRoutePollResult(
            result = nextPoll,
            loadedAtUnixMs = 60_000L,
        )

        assertNotSame(state, result)
        assertEquals(nextPoll, result.connectionRoutes)
        assertEquals(60_000L, result.connectionRoutesLastLoadedUnixMs)
    }
}
