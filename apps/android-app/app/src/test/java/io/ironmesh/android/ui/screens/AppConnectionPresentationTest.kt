package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.APP_CONNECTION_HEALTH_MAX_AGE_MS
import io.ironmesh.android.data.APP_CONNECTION_STATE_CONNECTED
import io.ironmesh.android.data.AppConnectionStatus
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AppConnectionPresentationTest {
    @Test
    fun staleConnectionIsNotPresentedAsHealthy() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs - APP_CONNECTION_HEALTH_MAX_AGE_MS - 1L,
        )

        assertEquals("App connection status is stale", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Stale", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(shouldShowRetryConnectionAction(status, hasProfiles = true, nowUnixMs = nowUnixMs))
    }
}
