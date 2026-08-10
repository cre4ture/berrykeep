package io.ironmesh.android.ui

import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.FolderSyncConfig
import io.ironmesh.android.data.TitleLatencyMonitorSettings
import org.junit.Assert.assertEquals
import org.junit.Test

class ScreenUiStatesTest {
    private val base = MainUiState()

    @Test
    fun homeProjectionIgnoresLibraryChanges() {
        assertEquals(
            base.toHomeUiState(),
            base.copy(galleryLoading = true).toHomeUiState(),
        )
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
