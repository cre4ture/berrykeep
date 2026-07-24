package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteAttemptSnapshot
import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.TransportSessionPoolSnapshot
import org.junit.Assert.assertEquals
import org.junit.Test

class RequestTimingsPresentationTest {
    @Test
    fun summarizes_only_successful_requests_and_reset_window_session_counts() {
        val snapshot = ConnectionRouteSnapshot(
            generatedAtUnixMs = 1_000L,
            endpoints = listOf(
                endpoint(
                    index = 0,
                    attempts = listOf(
                        attempt(startedUnixMs = 1_000L, totalDurationUs = 4_000L),
                        attempt(
                            startedUnixMs = 2_000L,
                            outcome = "failure",
                            totalDurationUs = 20_000L,
                        ),
                    ),
                    sessionPool = TransportSessionPoolSnapshot(
                        connectCount = 1L,
                        reuseCount = 2L,
                        connectDurationUs = 1_200L,
                    ),
                ),
                endpoint(
                    index = 1,
                    pathKind = "relay_tunnel",
                    attempts = listOf(attempt(startedUnixMs = 3_000L, totalDurationUs = 8_000L)),
                    sessionPool = TransportSessionPoolSnapshot(
                        connectCount = 2L,
                        resetCount = 1L,
                        connectDurationUs = 1_800L,
                        relayPairingDurationUs = 600L,
                    ),
                ),
            ),
        )

        val presentation = timingMeasurementPresentation(snapshot)

        assertEquals(listOf(3_000L, 2_000L, 1_000L), presentation.requests.map { it.attempt.startedUnixMs })
        assertEquals(2, presentation.successfulRequestCount)
        assertEquals(6_000.0, presentation.averageTotalUs)
        assertEquals(3L, presentation.sessionPool.connectCount)
        assertEquals(2L, presentation.sessionPool.reuseCount)
        assertEquals(1L, presentation.sessionPool.resetCount)
        assertEquals(1_000.0, presentation.averageSessionSetupUs)
        assertEquals(300.0, presentation.averageRelayPairingUs)
    }

    private fun endpoint(
        index: Int,
        pathKind: String = "direct_https",
        attempts: List<ConnectionRouteAttemptSnapshot>,
        sessionPool: TransportSessionPoolSnapshot,
    ): ConnectionRouteEndpointSnapshot {
        return ConnectionRouteEndpointSnapshot(
            index = index,
            pathKind = pathKind,
            locator = "https://node-$index.example",
            bootstrapRank = index,
            active = index == 0,
            score = 0.0,
            consecutiveFailures = 0,
            totalFailures = 0L,
            totalSuccesses = attempts.count { it.outcome == "success" }.toLong(),
            backgroundProbeInFlight = false,
            recentAttempts = attempts,
            transportSessionPool = sessionPool,
        )
    }

    private fun attempt(
        startedUnixMs: Long,
        outcome: String = "success",
        totalDurationUs: Long,
    ): ConnectionRouteAttemptSnapshot {
        return ConnectionRouteAttemptSnapshot(
            startedUnixMs = startedUnixMs,
            method = "GET",
            url = "https://node.example/api/v1/store/index",
            outcome = outcome,
            totalDurationUs = totalDurationUs,
        )
    }
}
