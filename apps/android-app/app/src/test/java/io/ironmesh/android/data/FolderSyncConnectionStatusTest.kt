package io.ironmesh.android.data

import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AppConnectionStatusTest {
    @Test
    fun legacyPersistedStatusDefaultsFunctionalSuccessFields() {
        val adapter = Moshi.Builder()
            .add(KotlinJsonAdapterFactory())
            .build()
            .adapter(AppConnectionStatus::class.java)

        val status = adapter.fromJson(
            """
            {
              "state": "connected",
              "lastSuccessfulConnectionUnixMs": 1234,
              "lastSuccessfulConnectionUrl": "https://example.test/api/v1/diagnostics/latency"
            }
            """.trimIndent(),
        )

        assertEquals(1234L, status?.lastSuccessfulConnectionUnixMs)
        assertNull(status?.lastSuccessfulFunctionalRequestUnixMs)
        assertNull(status?.lastSuccessfulFunctionalRequestUrl)
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
