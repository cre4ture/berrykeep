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
        assertEquals(now, recreatedStore.state().lastFailureAtEpochMs)
        assertFalse(recreatedStore.allowsAttempt(FolderSyncRetryTrigger.APP_FOREGROUNDED))
        assertTrue(recreatedStore.allowsAttempt(FolderSyncRetryTrigger.MANUAL_SYNC))
    }

    @Test
    fun staleWallClockDeadlineIsClampedToItsStoredBackoffRung() {
        val writer = FolderSyncOutageRetryStore(
            context = context,
            nowEpochMs = { 2 * 24 * 60 * 60_000L },
            jitterPermille = { 0 },
        )
        writer.clear()
        writer.recordFailure()

        val reader = FolderSyncOutageRetryStore(
            context = context,
            nowEpochMs = { 0L },
            jitterPermille = { 0 },
        )

        assertEquals(
            33_000L,
            reader.state().nextRetryAtEpochMs,
        )
    }

    @Test
    fun clearingTheSchedulerCircuitAlsoRemovesPersistedRetryState() {
        val store = FolderSyncOutageRetryStore(
            context = context,
            jitterPermille = { 0 },
        )
        store.recordFailure()

        FolderSyncScheduler.clearOutageRetryCircuit(context)

        assertEquals(FolderSyncOutageRetryState(), store.state())
    }
}
