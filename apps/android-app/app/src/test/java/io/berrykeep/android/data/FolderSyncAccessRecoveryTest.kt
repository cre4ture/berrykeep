package io.berrykeep.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class FolderSyncAccessRecoveryTest {
    private val profile = FolderSyncConfig(
        id = "camera", label = "Fotos", prefix = "devices/photos",
        localFolder = "/storage/emulated/0/DCIM/Camera", localFolderTreeUri = "old-uri",
    )

    @Test
    fun regrantPreservesProfileIdentityAndSyncState() {
        assertEquals(
            profile.copy(localFolderTreeUri = "new-uri"),
            profile.withRestoredFolderAccess("/storage/emulated/0/DCIM/Camera/", "new-uri"),
        )
        val paused = profile.copy(enabled = false)
        assertEquals(paused.copy(localFolderTreeUri = "new-uri"),
            paused.withRestoredFolderAccess(profile.localFolder, "new-uri"))
    }

    @Test
    fun selectingDifferentFolderCannotRetargetExistingSyncProfile() {
        assertThrows(IllegalArgumentException::class.java) {
            profile.withRestoredFolderAccess("/storage/emulated/0/DCIM", "different-uri")
        }
    }
}
