package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Test

class GlobalFolderSyncStatusTest {
    @Test
    fun reportsNotConfiguredAndPausedStates() {
        assertEquals(
            GLOBAL_SYNC_STATE_NOT_CONFIGURED,
            mergeGlobalFolderSyncStatus(emptyList(), FolderSyncServiceStatus()).state,
        )
        assertEquals(
            GLOBAL_SYNC_STATE_PAUSED,
            mergeGlobalFolderSyncStatus(
                configs = listOf(profile(enabled = false)),
                runtimeStatus = FolderSyncServiceStatus(),
            ).state,
        )
    }

    @Test
    fun syncErrorTakesPriorityOverExecutionAndNetworkWaiting() {
        val status = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(
                serviceState = "error",
                serviceMessage = "1 active profile with errors",
                currentActivity = "Photos: upload returned 404",
                activeProfileCount = 1,
                errorProfileCount = 1,
            ),
            blockedProfileCount = 1,
            blockedReason = "Wi-Fi is not allowed",
            oneShotRunning = true,
        )

        assertEquals(GLOBAL_SYNC_STATE_ERROR, status.state)
        assertEquals("Photos: upload returned 404", status.message)
        assertEquals(1L, status.errorProfileCount)
    }

    @Test
    fun includesRecoveryOneShotInGlobalState() {
        val status = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(),
            oneShotRunning = true,
            oneShotProfileLabel = "Photos",
        )

        assertEquals(GLOBAL_SYNC_STATE_SYNCING, status.state)
        assertEquals("Running scheduled reconciliation for Photos", status.message)
    }

    @Test
    fun reportsNetworkWaitingSeparatelyFromConnectionHealth() {
        val status = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(),
            blockedProfileCount = 1,
            blockedReason = "Mobile data is disabled for this profile",
        )

        assertEquals(GLOBAL_SYNC_STATE_WAITING, status.state)
        assertEquals(1L, status.waitingProfileCount)
        assertEquals(
            "1 sync profile is waiting: Mobile data is disabled for this profile",
            status.message,
        )
    }

    @Test
    fun reportsHealthyOnlyWhenAllEnabledProfilesAreActive() {
        val status = mergeGlobalFolderSyncStatus(
            configs = listOf(profile(id = "photos"), profile(id = "documents")),
            runtimeStatus = FolderSyncServiceStatus(
                serviceState = "running",
                activeProfileCount = 2,
                runningProfileCount = 2,
                lastSuccessUnixMs = 123L,
            ),
        )

        assertEquals(GLOBAL_SYNC_STATE_HEALTHY, status.state)
        assertEquals(2L, status.enabledProfileCount)
        assertEquals(123L, status.lastSuccessUnixMs)
    }

    @Test
    fun unchangedSemanticStatusKeepsItsOriginalUpdateTimestamp() {
        val first = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(
                updatedUnixMs = 500L,
                activeProfileCount = 1,
                runningProfileCount = 1,
            ),
            nowUnixMs = 1_000L,
        )

        val unchanged = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(
                updatedUnixMs = 2_000L,
                activeProfileCount = 1,
                runningProfileCount = 1,
            ),
            previousStatus = first,
            nowUnixMs = 3_000L,
        )

        assertEquals(1_000L, first.updatedUnixMs)
        assertEquals(first, unchanged)
    }

    @Test
    fun semanticStatusChangeAdvancesItsUpdateTimestamp() {
        val first = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(activeProfileCount = 1),
            nowUnixMs = 1_000L,
        )

        val changed = mergeGlobalFolderSyncStatus(
            configs = listOf(profile()),
            runtimeStatus = FolderSyncServiceStatus(
                updatedUnixMs = 2_000L,
                activeProfileCount = 1,
                syncingProfileCount = 1,
                currentActivity = "Uploading photo.jpg",
            ),
            previousStatus = first,
            nowUnixMs = 3_000L,
        )

        assertEquals(GLOBAL_SYNC_STATE_SYNCING, changed.state)
        assertEquals(3_000L, changed.updatedUnixMs)
    }

    private fun profile(
        id: String = "photos",
        enabled: Boolean = true,
    ): FolderSyncConfig {
        return FolderSyncConfig(
            id = id,
            label = id.replaceFirstChar { character -> character.titlecase() },
            prefix = id,
            localFolder = "/tmp/$id",
            enabled = enabled,
        )
    }
}
