package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.APP_CONNECTION_HEALTH_MAX_AGE_MS
import io.ironmesh.android.data.APP_CONNECTION_STATE_CONNECTED
import io.ironmesh.android.data.APP_CONNECTION_STATE_ERROR
import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.AppFailedConnectionAttempt
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AppConnectionPresentationTest {
    @Test
    fun appRequestFailureIsNotPresentedAsIdle() {
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_ERROR,
            message = "store index timed out",
        )

        assertEquals("App requests failed", appConnectionHeadline(status))
        assertEquals("store index timed out", appConnectionSummary(status))
    }

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

    @Test
    fun connectivityProbeIsPresentedAsReachabilityRatherThanAppHealth() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "relay://node@https://example.test/api/v1/diagnostics/latency?response_bytes=0",
        )

        assertEquals("Server is reachable", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Reachable", appConnectionStatusBadge(status, nowUnixMs))
        assertFalse(isAppConnectionHealthy(status, nowUnixMs))
        assertTrue(
            shouldShowRetryConnectionAction(
                status,
                hasProfiles = true,
                nowUnixMs = nowUnixMs,
            ),
        )
        assertTrue(
            appConnectionSummary(status).contains(
                "library and sync requests are not verified",
            ),
        )
    }

    @Test
    fun connectivityProbeDoesNotHideFunctionalRequestFailures() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "https://example.test/api/v1/diagnostics/latency?response_bytes=0",
            failedAttempts = listOf(
                AppFailedConnectionAttempt(
                    startedUnixMs = nowUnixMs - 2_000L,
                    finishedUnixMs = nowUnixMs - 1_000L,
                    method = "GET",
                    url = "https://example.test/api/v1/store/index?media_filter=image",
                    error = "request timed out",
                ),
            ),
        )

        assertEquals(
            "Server reachable; app requests failed",
            appConnectionHeadline(status, nowUnixMs),
        )
        assertEquals("Degraded", appConnectionStatusBadge(status, nowUnixMs))
        assertFalse(isAppConnectionHealthy(status, nowUnixMs))
        assertTrue(
            shouldShowRetryConnectionAction(
                status,
                hasProfiles = true,
                nowUnixMs = nowUnixMs,
            ),
        )
        assertTrue(
            appConnectionSummary(status).contains(
                "a newer app request failed",
            ),
        )
    }

    @Test
    fun failureThenFunctionalSuccessThenProbeRemainsHealthy() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "https://example.test/api/v1/diagnostics/latency?response_bytes=0",
            lastSuccessfulFunctionalRequestUnixMs = nowUnixMs - 1_000L,
            lastSuccessfulFunctionalRequestUrl =
                "https://example.test/api/v1/store/index?media_filter=image",
            failedAttempts = listOf(
                AppFailedConnectionAttempt(
                    startedUnixMs = nowUnixMs - 3_000L,
                    finishedUnixMs = nowUnixMs - 2_000L,
                    method = "GET",
                    url = "https://example.test/api/v1/store/index?media_filter=image",
                ),
            ),
        )

        assertEquals("App connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(isAppConnectionHealthy(status, nowUnixMs))
        assertFalse(
            shouldShowRetryConnectionAction(
                status,
                hasProfiles = true,
                nowUnixMs = nowUnixMs,
            ),
        )
    }

    @Test
    fun failedConnectivityProbeAloneIsNotReportedAsFunctionalFailure() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "https://example.test/api/v1/diagnostics/latency?response_bytes=0",
            failedAttempts = listOf(
                AppFailedConnectionAttempt(
                    startedUnixMs = nowUnixMs - 2_000L,
                    method = "GET",
                    url = "https://example.test/api/v1/diagnostics/latency?response_bytes=0",
                ),
            ),
        )

        assertEquals("Server is reachable", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Reachable", appConnectionStatusBadge(status, nowUnixMs))
    }
}
