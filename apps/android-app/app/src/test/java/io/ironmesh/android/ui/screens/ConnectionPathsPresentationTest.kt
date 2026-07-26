package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionPathsPresentationTest {
    @Test
    fun prioritizesActiveAndAvailableRoutesAheadOfPausedAndFailedRoutes() {
        val snapshot = ConnectionRouteSnapshot(
            generatedAtUnixMs = 1_000L,
            rankedIndices = listOf(0, 1, 2, 3),
            endpoints = listOf(
                endpoint(index = 0, circuitOpenUntilUnixMs = 2_000L),
                endpoint(index = 1, totalSuccesses = 4L),
                endpoint(index = 2, active = true, totalSuccesses = 2L),
                endpoint(index = 3, totalFailures = 1L, consecutiveFailures = 1),
            ),
        )

        val presentation = connectionPathsPresentation(snapshot, error = null)

        assertEquals(listOf(2, 1, 0, 3), presentation.routes.map { it.endpoint.index })
        assertEquals(ConnectionRouteState.ACTIVE, presentation.routes[0].state)
        assertEquals(ConnectionRouteState.AVAILABLE, presentation.routes[1].state)
        assertEquals(ConnectionRouteState.PAUSED, presentation.routes[2].state)
        assertEquals(ConnectionRouteState.UNAVAILABLE, presentation.routes[3].state)
    }

    @Test
    fun reportsStableRelayAsConnectedInsteadOfUnavailable() {
        val relay = endpoint(
            index = 4,
            pathKind = "relay_tunnel",
            locator = "node-a@https://relay.example:7443",
            targetNodeId = "node-a",
            active = true,
            totalSuccesses = 3L,
        )

        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                activeIndex = relay.index,
                rankedIndices = listOf(relay.index),
                endpoints = listOf(relay),
            ),
            error = null,
        )

        assertEquals(ConnectionOverviewState.RELAY, presentation.overview.state)
        assertEquals("Relay via relay.example:7443 to node-a", routeDisplayLabel(relay))
    }

    @Test
    fun keepsTransportFailureOutOfThePrimaryOverviewUntilDetailsAreOpened() {
        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                endpoints = listOf(
                    endpoint(
                        index = 0,
                        totalFailures = 1L,
                        consecutiveFailures = 1,
                    ),
                ),
            ),
            error = "TLS handshake timed out",
        )

        assertEquals(ConnectionOverviewState.ERROR, presentation.overview.state)
        assertEquals("TLS handshake timed out", presentation.overview.error)
        assertTrue(presentation.routes.isNotEmpty())
    }

    @Test
    fun exposesRankAndScoreFactorsThatExplainAnActiveRelayRoute() {
        val relay = endpoint(
            index = 1,
            pathKind = "relay_tunnel",
            active = true,
            score = 481.0,
            ewmaLatencyMs = 80.0,
            ewmaThroughputBytesPerSec = 12_500_000.0,
        )
        val direct = endpoint(
            index = 0,
            score = 525.0,
            ewmaLatencyMs = 525.0,
        )

        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                activeIndex = relay.index,
                rankedIndices = listOf(relay.index, direct.index),
                endpoints = listOf(direct, relay),
            ),
            error = null,
        )

        val activeRoute = presentation.routes.first()
        val directRoute = presentation.routes[1]

        assertEquals(1, activeRoute.selectionRank)
        assertEquals(2, activeRoute.selectionCandidateCount)
        assertEquals(481.0, activeRoute.scoreBreakdown.total, 0.0)
        assertEquals(1.0, activeRoute.scoreBreakdown.bootstrapOrderPoints, 0.0)
        assertEquals(80.0, activeRoute.scoreBreakdown.latencyPoints, 0.0)
        assertEquals(500.0, activeRoute.scoreBreakdown.relayPenaltyPoints, 0.0)
        assertEquals(50.0, activeRoute.scoreBreakdown.throughputCreditPoints, 0.0)
        assertEquals(50.0, activeRoute.scoreBreakdown.activeRouteCreditPoints, 0.0)
        assertEquals(2, directRoute.selectionRank)
    }

    @Test
    fun usesTheKnownLatencyEstimateInTheScoreBreakdownUntilMeasured() {
        val score = routeScoreBreakdown(endpoint(index = 0))

        assertTrue(score.latencyIsEstimated)
        assertEquals(75.0, score.latencyPoints, 0.0)
    }

    private fun endpoint(
        index: Int,
        pathKind: String = "direct_https",
        locator: String = "https://node.example",
        targetNodeId: String? = null,
        active: Boolean = false,
        score: Double = 0.0,
        ewmaLatencyMs: Double? = null,
        ewmaThroughputBytesPerSec: Double? = null,
        totalSuccesses: Long = 0L,
        totalFailures: Long = 0L,
        consecutiveFailures: Int = 0,
        circuitOpenUntilUnixMs: Long? = null,
    ): ConnectionRouteEndpointSnapshot {
        return ConnectionRouteEndpointSnapshot(
            index = index,
            pathKind = pathKind,
            locator = locator,
            bootstrapRank = index,
            targetNodeId = targetNodeId,
            active = active,
            score = score,
            ewmaLatencyMs = ewmaLatencyMs,
            ewmaThroughputBytesPerSec = ewmaThroughputBytesPerSec,
            consecutiveFailures = consecutiveFailures,
            totalFailures = totalFailures,
            totalSuccesses = totalSuccesses,
            circuitOpenUntilUnixMs = circuitOpenUntilUnixMs,
            backgroundProbeInFlight = false,
        )
    }
}
