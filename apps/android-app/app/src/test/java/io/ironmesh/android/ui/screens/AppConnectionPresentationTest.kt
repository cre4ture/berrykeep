package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.APP_CONNECTION_HEALTH_MAX_AGE_MS
import io.ironmesh.android.data.APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE
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
    fun newConnectionSuccessUsesCurrentOsTimeInsteadOfPreviousUiTime() {
        val previousUiTimeUnixMs = 1_750_000_000_000L
        val successUnixMs = previousUiTimeUnixMs + 1_000L
        val currentOsTimeUnixMs = successUnixMs + 1L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = successUnixMs,
        )

        assertEquals(
            "Connection status is stale",
            appConnectionHeadline(status, previousUiTimeUnixMs),
        )
        assertTrue(shouldShowRetryConnectionAction(status, previousUiTimeUnixMs))

        val nowUnixMs = currentConnectionHealthNow(previousUiTimeUnixMs) {
            currentOsTimeUnixMs
        }

        assertEquals(currentOsTimeUnixMs, nowUnixMs)
        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertFalse(shouldShowRetryConnectionAction(status, nowUnixMs))
    }

    @Test
    fun appRequestFailureIsNotPresentedAsIdle() {
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_ERROR,
            message = "store index timed out",
        )

        assertEquals("Connection failed", appConnectionHeadline(status))
        assertEquals("store index timed out", appConnectionSummary(status))
    }

    @Test
    fun staleConnectionIsNotPresentedAsHealthy() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs - APP_CONNECTION_HEALTH_MAX_AGE_MS - 1L,
        )

        assertEquals("Connection status is stale", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Stale", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(shouldShowRetryConnectionAction(status, nowUnixMs = nowUnixMs))
    }

    @Test
    fun connectivityProbeEstablishesConnectionHealth() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "relay://node@https://example.test/api/v1/diagnostics/latency?response_bytes=0",
        )

        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(isAppConnectionHealthy(status, nowUnixMs))
        assertFalse(
            shouldShowRetryConnectionAction(
                status,
                nowUnixMs = nowUnixMs,
            ),
        )
        assertTrue(
            appConnectionSummary(status).contains(
                "The server is reachable",
            ),
        )
    }

    @Test
    fun functionalRequestFailureDoesNotChangeTransportHealth() {
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

        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(isAppConnectionHealthy(status, nowUnixMs))
        assertFalse(
            shouldShowRetryConnectionAction(
                status,
                nowUnixMs = nowUnixMs,
            ),
        )
        assertTrue(appConnectionSummary(status).contains("The server is reachable"))
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

        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(isAppConnectionHealthy(status, nowUnixMs))
        assertFalse(
            shouldShowRetryConnectionAction(
                status,
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

        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
    }

    @Test
    fun backgroundMaintenanceFailureDoesNotDegradeTheHomeConnectionCard() {
        val nowUnixMs = 1_750_000_000_000L
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            lastSuccessfulConnectionUnixMs = nowUnixMs,
            lastSuccessfulConnectionUrl =
                "https://example.test/api/v1/diagnostics/latency?response_bytes=0",
            lastSuccessfulFunctionalRequestUnixMs = nowUnixMs - 1_000L,
            lastSuccessfulFunctionalRequestUrl = "https://example.test/api/v1/store/index",
            failedAttempts = listOf(
                AppFailedConnectionAttempt(
                    impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE,
                    startedUnixMs = nowUnixMs - 500L,
                    method = "GET",
                    url = "iroh://candidate/api/v1/cluster/status",
                    error = "candidate timed out",
                ),
            ),
        )

        assertEquals("Connection is healthy", appConnectionHeadline(status, nowUnixMs))
        assertEquals("Healthy", appConnectionStatusBadge(status, nowUnixMs))
        assertTrue(isAppConnectionHealthy(status, nowUnixMs))
    }
}
