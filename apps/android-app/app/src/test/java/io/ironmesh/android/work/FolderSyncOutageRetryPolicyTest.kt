package io.ironmesh.android.work

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class FolderSyncOutageRetryPolicyTest {
    @Test
    fun endpointFailuresUseThirtySecondsToThirtyMinutesBoundedBackoff() {
        assertEquals(30_000L, FolderSyncOutageRetryPolicy.delayForFailure(1))
        assertEquals(60_000L, FolderSyncOutageRetryPolicy.delayForFailure(2))
        assertEquals(120_000L, FolderSyncOutageRetryPolicy.delayForFailure(3))
        assertEquals(240_000L, FolderSyncOutageRetryPolicy.delayForFailure(4))
        assertEquals(480_000L, FolderSyncOutageRetryPolicy.delayForFailure(5))
        assertEquals(960_000L, FolderSyncOutageRetryPolicy.delayForFailure(6))
        assertEquals(1_800_000L, FolderSyncOutageRetryPolicy.delayForFailure(7))
        assertEquals(1_800_000L, FolderSyncOutageRetryPolicy.delayForFailure(99))
    }

    @Test
    fun jitterNeverMakesRetryEarlierOrLongerThanMaximum() {
        assertEquals(33_000L, FolderSyncOutageRetryPolicy.delayForFailure(1, jitterPermille = 100))
        assertEquals(1_800_000L, FolderSyncOutageRetryPolicy.delayForFailure(7, jitterPermille = 100))
    }

    @Test
    fun nonManualEventsAreHeldUntilPersistentBackoffIsDue() {
        val state = FolderSyncOutageRetryState(
            failureCount = 3,
            nextRetryAtEpochMs = 1_000L,
        )

        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.APP_FOREGROUNDED,
                nowEpochMs = 999L,
            ),
        )
        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.NETWORK_AVAILABLE,
                nowEpochMs = 999L,
            ),
        )
        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.NETWORK_POLICY_CHANGED,
                nowEpochMs = 999L,
            ),
        )
        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.LOCAL_FOLDER_CHANGED,
                nowEpochMs = 999L,
            ),
        )
        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.PERIODIC_WORK,
                nowEpochMs = 999L,
            ),
        )
        assertTrue(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.MANUAL_SYNC,
                nowEpochMs = 999L,
            ),
        )
        assertTrue(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.CONFIGURATION_CHANGED,
                nowEpochMs = 999L,
            ),
        )
        assertTrue(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.NETWORK_AVAILABLE,
                nowEpochMs = 1_000L,
            ),
        )
    }

    @Test
    fun aLaterFailureBuildsOnPersistedStateRatherThanRestartingTheBackoff() {
        val initial = FolderSyncOutageRetryState()
        val first = FolderSyncOutageRetryPolicy.afterFailure(initial, nowEpochMs = 1_000L)
        val second = FolderSyncOutageRetryPolicy.afterFailure(first, nowEpochMs = 31_000L)

        assertEquals(1, first.failureCount)
        assertEquals(31_000L, first.nextRetryAtEpochMs)
        assertEquals(2, second.failureCount)
        assertEquals(91_000L, second.nextRetryAtEpochMs)
    }

    @Test
    fun backoffTimerIsHeldUntilTheStoredDeadline() {
        val state = FolderSyncOutageRetryState(
            failureCount = 1,
            nextRetryAtEpochMs = 30_000L,
        )

        assertFalse(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.BACKOFF_TIMER,
                nowEpochMs = 29_999L,
            ),
        )
        assertTrue(
            FolderSyncOutageRetryPolicy.allowsAttempt(
                state,
                FolderSyncRetryTrigger.BACKOFF_TIMER,
                nowEpochMs = 30_000L,
            ),
        )
    }
}
