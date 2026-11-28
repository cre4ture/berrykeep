package io.berrykeep.android.work

import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.activity.result.ActivityResult
import io.berrykeep.android.handleFolderPickerResult
import io.berrykeep.android.data.FolderSyncConfig
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class FolderSyncAccessInstrumentationTest {
    private val context = ApplicationProvider.getApplicationContext<Context>()
    private val profile = FolderSyncConfig(
        id = "camera", label = "Fotos", prefix = "photos", localFolder = "/storage/emulated/0/DCIM/Camera",
        localFolderTreeUri = "content://com.android.externalstorage.documents/tree/primary%3ADCIM%2FCamera",
    )

    @Test
    fun failedPersistenceAndCancelledSelectionNeverReplaceTheConfiguredFolder() {
        var selectionCount = 0
        var errorCount = 0
        val onSelection: (String, String) -> Unit = { _, _ -> selectionCount++ }
        val onError: (String) -> Unit = { errorCount++ }
        handleFolderPickerResult(
            context,
            ActivityResult(android.app.Activity.RESULT_OK, Intent().apply {
                data = Uri.parse("content://io.berrykeep.missing.documents/tree/primary%3ADCIM%2FCamera")
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            }),
            onSelection, onError,
        )
        handleFolderPickerResult(
            context, ActivityResult(android.app.Activity.RESULT_CANCELED, Intent().apply {
                data = Uri.parse(profile.localFolderTreeUri)
            }), onSelection, onError,
        )
        assertEquals(0, selectionCount)
        assertEquals(2, errorCount)
    }

    @Test
    fun accessCheckRequiresBothReadAndWriteOnTheTreeDocument() {
        var permission = PackageManager.PERMISSION_DENIED
        val permissionContext = object : ContextWrapper(context) {
            override fun checkUriPermission(uri: Uri, pid: Int, uid: Int, modeFlags: Int): Int {
                assertEquals("primary:DCIM/Camera", android.provider.DocumentsContract.getDocumentId(uri))
                assertEquals(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION, modeFlags)
                return permission
            }
        }
        assertFalse(FolderSyncEligibilityGate.hasFolderAccess(permissionContext, profile))
        permission = PackageManager.PERMISSION_GRANTED
        assertTrue(FolderSyncEligibilityGate.hasFolderAccess(permissionContext, profile))
    }

    @Test
    fun malformedSavedTreeIsPausedAndNativeFilesystemProfilesRemainEligible() {
        assertFalse(FolderSyncEligibilityGate.hasFolderAccess(context, profile.copy(localFolderTreeUri = "broken")))
        assertTrue(FolderSyncEligibilityGate.hasFolderAccess(context, profile.copy(localFolderTreeUri = null)))
    }
}
