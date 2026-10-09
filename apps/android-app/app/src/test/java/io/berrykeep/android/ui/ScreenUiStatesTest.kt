package io.berrykeep.android.ui

import io.berrykeep.android.data.AppConnectionStatus
import io.berrykeep.android.data.FolderSyncConfig
import io.berrykeep.android.data.TitleLatencyMonitorSettings
import org.junit.Assert.assertEquals
import org.junit.Test

class ScreenUiStatesTest {
    private val base = MainUiState()

    @Test
    fun homeProjectionIncludesObservableClientWork() {
        val projected = base.copy(
            galleryLoading = true,
            connectionRoutesLoading = true,
        ).toHomeUiState()

        assertEquals(2L, projected.clientTaskQueues.first { it.id == "foreground" }.active)
        assertEquals("running", projected.clientTaskQueues.first { it.id == "foreground" }.state)
    }

    @Test
    fun syncProjectionIgnoresConnectionChanges() {
        assertEquals(
            base.toSyncUiState(),
            base.copy(appConnectionStatus = AppConnectionStatus(state = "connected")).toSyncUiState(),
        )
    }

    @Test
    fun libraryProjectionIgnoresSyncChanges() {
        assertEquals(
            base.toLibraryUiState(),
            base.copy(
                syncProfiles = listOf(
                    FolderSyncConfig(
                        id = "profile",
                        label = "Profile",
                        prefix = "photos",
                        localFolder = "/photos",
                    ),
                ),
            ).toLibraryUiState(),
        )
    }

    @Test
    fun connectivityProjectionIgnoresOnboardingChanges() {
        assertEquals(
            base.toConnectivityUiState(),
            base.copy(bootstrapInput = "new-bootstrap-input").toConnectivityUiState(),
        )
    }

    @Test
    fun requestTimingsProjectionIgnoresLibraryChanges() {
        assertEquals(
            base.toRequestTimingsUiState(),
            base.copy(galleryLoading = true).toRequestTimingsUiState(),
        )
    }

    @Test
    fun settingsProjectionIgnoresTransientStatusChanges() {
        assertEquals(
            base.toSettingsUiState(),
            base.copy(status = "background operation completed").toSettingsUiState(),
        )
    }

    @Test
    fun galleryMapProjectionIgnoresSettingsChanges() {
        assertEquals(
            base.toGalleryMapUiState(),
            base.copy(payload = "new payload").toGalleryMapUiState(),
        )
    }

    @Test
    fun onboardingProjectionIgnoresMonitoringChanges() {
        assertEquals(
            base.toOnboardingUiState(),
            base.copy(
                titleLatencyMonitorSettings = TitleLatencyMonitorSettings(enabled = true),
            ).toOnboardingUiState(),
        )
    }
}
