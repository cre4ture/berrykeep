package io.berrykeep.android.work

import io.berrykeep.android.data.FolderSyncConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class FolderSyncEligibilityGateTest {
    private val wifi = FolderSyncNetworkSnapshot(
        connected = true,
        transports = setOf(FolderSyncActiveTransport.WIFI),
    )
    private val camera = FolderSyncConfig(
        id = "camera", label = "Fotos", prefix = "photos", localFolder = "/storage/emulated/0/DCIM/Camera",
        localFolderTreeUri = "content://com.android.externalstorage.documents/tree/primary%3ADCIM%2FCamera",
    )

    @Test
    fun missingGrantPausesOnlyAffectedProfileAndDoesNotChangeConfiguration() {
        val other = camera.copy(id = "other", label = "Other")
        val decisions = FolderSyncEligibilityGate.evaluateProfiles(listOf(camera, other), wifi) {
            it.id != camera.id
        }
        assertFalse(decisions[0].allowed)
        assertTrue(decisions[0].folderAccessRequired)
        assertTrue(decisions[0].reason.contains("Grant folder access"))
        assertTrue(decisions[1].allowed)
        assertTrue(camera.enabled)
    }

    @Test
    fun restoringAccessResumesProfileButStillHonorsNetworkRules() {
        assertTrue(FolderSyncEligibilityGate.evaluateProfiles(listOf(camera), wifi) { true }.single().allowed)
        val offline = FolderSyncEligibilityGate.evaluateProfiles(
            listOf(camera), FolderSyncNetworkSnapshot(connected = false),
        ) { true }.single()
        assertFalse(offline.allowed)
        assertFalse(offline.folderAccessRequired)
        assertEquals("No active internet connection", offline.reason)
    }

    @Test
    fun folderAccessMessageTakesPriorityOverOfflineNetwork() {
        val decision = FolderSyncEligibilityGate.evaluateProfiles(
            listOf(camera), FolderSyncNetworkSnapshot(connected = false),
        ) { false }.single()
        assertTrue(decision.folderAccessRequired)
    }
}
