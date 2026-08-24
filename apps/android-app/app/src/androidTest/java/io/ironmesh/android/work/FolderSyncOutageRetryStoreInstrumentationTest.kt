package io.ironmesh.android.work

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class FolderSyncOutageRetryStoreInstrumentationTest {
    private val context = ApplicationProvider.getApplicationContext<android.content.Context>()

    @After
    fun clearState() {
        FolderSyncOutageRetryStore(context).clear()
    }

    @Test
    fun retryStateSurvivesAStoreRecreation() {
        val now = 2_000L
        val firstStore = FolderSyncOutageRetryStore(
            context = context,
            nowEpochMs = { now },
            jitterPermille = { 0 },
        )
        firstStore.clear()
        val recorded = firstStore.recordFailure()

        val recreatedStore = FolderSyncOutageRetryStore(
            context = context,
            nowEpochMs = { now },
            jitterPermille = { 0 },
        )

        assertEquals(recorded, recreatedStore.state())
        assertFalse(recreatedStore.allowsAttempt(FolderSyncRetryTrigger.APP_FOREGROUNDED))
        assertTrue(recreatedStore.allowsAttempt(FolderSyncRetryTrigger.MANUAL_SYNC))
    }
}
