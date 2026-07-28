package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AppConnectionStatusTest {
    @Test
    fun nextRetryDelayStartsAtTwoSeconds() {
        assertEquals(2_000L, nextAppConnectionRetryDelayMs(1))
    }

    @Test
    fun nextRetryDelayCapsAtSixtySeconds() {
        assertEquals(60_000L, nextAppConnectionRetryDelayMs(8))
    }

    @Test
    fun retryPendingReflectsScheduledRetryState() {
        val pending = AppConnectionStatus(
            state = APP_CONNECTION_STATE_RETRY_SCHEDULED,
            nextRetryUnixMs = 1234L,
        )
        val idle = AppConnectionStatus()

        assertTrue(pending.isRetryPending())
        assertFalse(idle.isRetryPending())
    }

    @Test
    fun connectedStatusRequiresASuccessWithinTheLastHour() {
        val nowUnixMs = 1_750_000_000_000L
        val fresh = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs - APP_CONNECTION_HEALTH_MAX_AGE_MS,
        )
        val stale = fresh.copy(
            lastSuccessfulConnectionUnixMs = nowUnixMs - APP_CONNECTION_HEALTH_MAX_AGE_MS - 1L,
        )
        val future = fresh.copy(lastSuccessfulConnectionUnixMs = nowUnixMs + 1L)

        assertTrue(fresh.isConnected(nowUnixMs))
        assertFalse(stale.isConnected(nowUnixMs))
        assertFalse(future.isConnected(nowUnixMs))
        assertFalse(AppConnectionStatus(state = APP_CONNECTION_STATE_CONNECTED).isConnected(nowUnixMs))
    }
}
