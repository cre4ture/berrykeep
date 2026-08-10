package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionPathsPresentationTest {
    @Test
    fun prioritizesActiveAndAvailableRoutesAheadOfCoolDownAndFailedRoutes() {
        val snapshot = ConnectionRouteSnapshot(
            generatedAtUnixMs = 1_000L,
            rankedIndices = listOf(0, 1, 2, 3),
            endpoints = listOf(
                endpoint(index = 0, circuitOpenUntilUnixMs = 2_000L),
                endpoint(index = 1, totalSuccesses = 4L),
                endpoint(index = 2, lastUsedUnixMs = 900L, totalSuccesses = 2L),
                endpoint(index = 3, totalFailures = 1L, consecutiveFailures = 1),
            ),
        )

        val presentation = connectionPathsPresentation(snapshot, error = null)

        assertEquals(listOf(2, 1, 0, 3), presentation.routes.map { it.endpoint.index })
        assertEquals(ConnectionRouteState.ACTIVE, presentation.routes[0].state)
        assertEquals(ConnectionRouteState.AVAILABLE, presentation.routes[1].state)
        assertEquals(ConnectionRouteState.COOL_DOWN, presentation.routes[2].state)
        assertEquals(ConnectionRouteState.UNAVAILABLE, presentation.routes[3].state)
    }

    @Test
    fun reportsStableRelayAsConnectedInsteadOfUnavailable() {
        val relay = endpoint(
            index = 4,
            pathKind = "relay_tunnel",
            locator = "node-a@https://relay.example:7443",
            targetNodeId = "node-a",
            lastUsedUnixMs = 900L,
            totalSuccesses = 3L,
        )

        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                rankedIndices = listOf(relay.index),
                endpoints = listOf(relay),
            ),
            error = null,
        )

        assertEquals(ConnectionOverviewState.RELAY, presentation.overview.state)
        assertEquals("Relay via relay.example:7443 to node-a", routeDisplayLabel(relay))
        assertEquals("Relay · relay.example:7443", compactRouteDisplayLabel(relay))
    }

    @Test
    fun compactsLongTargetIdentifiersForDenseRouteRows() {
        val direct = endpoint(
            index = 2,
            pathKind = "direct_https",
            targetNodeId = "7314c3bb-2e1d-4508-a4d1-d274d985f059",
        )

        assertEquals("HTTPS · 7314c3bb…f059", compactRouteDisplayLabel(direct))
    }

    @Test
    fun reportsDirectQuicHolePunchingAsItsOwnDirectConnectionState() {
        val directQuic = endpoint(
            index = 5,
            pathKind = "direct_quic",
            holePunchingMode = "direct",
            lastUsedUnixMs = 900L,
            totalSuccesses = 3L,
        )

        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                rankedIndices = listOf(directQuic.index),
                endpoints = listOf(directQuic),
            ),
            error = null,
        )

        assertEquals(ConnectionOverviewState.DIRECT_QUIC, presentation.overview.state)
        assertEquals("Direct via NAT (QUIC)", routeDisplayLabel(directQuic))
    }

    @Test
    fun reportsRelayWhenAQuicSessionUsesItsRelayPath() {
        val relayedQuic = endpoint(
            index = 6,
            pathKind = "direct_quic",
            holePunchingMode = "relay",
            lastUsedUnixMs = 900L,
            totalSuccesses = 3L,
        )

        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 1_000L,
                rankedIndices = listOf(relayedQuic.index),
                endpoints = listOf(relayedQuic),
            ),
            error = null,
        )

        assertEquals(ConnectionOverviewState.RELAY, presentation.overview.state)
        assertEquals("QUIC via Relay", routeDisplayLabel(relayedQuic))
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
    fun exposesRankAndScoreFactorsWithoutCreditingRecentUse() {
        val relay = endpoint(
            index = 1,
            pathKind = "relay_tunnel",
            lastUsedUnixMs = 900L,
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
        assertEquals(2, directRoute.selectionRank)
    }

    @Test
    fun reportsEveryRouteUsedWithinTwoSecondsAsActive() {
        val snapshot = ConnectionRouteSnapshot(
            generatedAtUnixMs = 10_000L,
            rankedIndices = listOf(0, 1, 2),
            endpoints = listOf(
                endpoint(index = 0, lastUsedUnixMs = 9_900L, totalSuccesses = 1L),
                endpoint(index = 1, lastUsedUnixMs = 8_000L, totalSuccesses = 1L),
                endpoint(index = 2, lastUsedUnixMs = 7_999L, totalSuccesses = 1L),
            ),
        )

        val presentation = connectionPathsPresentation(snapshot, error = null)

        assertEquals(ConnectionOverviewState.MULTIPLE, presentation.overview.state)
        assertEquals(2, presentation.overview.activeRouteCount)
        assertEquals(
            listOf(ConnectionRouteState.ACTIVE, ConnectionRouteState.ACTIVE, ConnectionRouteState.AVAILABLE),
            presentation.routes.map { it.state },
        )
    }

    @Test
    fun reportsHealthyRoutesAsAvailableAfterTheActiveWindowExpires() {
        val presentation = connectionPathsPresentation(
            snapshot = ConnectionRouteSnapshot(
                generatedAtUnixMs = 10_000L,
                rankedIndices = listOf(0),
                endpoints = listOf(
                    endpoint(index = 0, lastUsedUnixMs = 7_999L, totalSuccesses = 1L),
                ),
            ),
            error = null,
        )

        assertEquals(ConnectionOverviewState.AVAILABLE, presentation.overview.state)
        assertEquals(0, presentation.overview.activeRouteCount)
        assertEquals(ConnectionRouteState.AVAILABLE, presentation.routes.single().state)
    }

    @Test
    fun ignoresSnapshotTimeWhileTheVisibleRouteStateIsUnchanged() {
        val route = endpoint(index = 0, lastUsedUnixMs = 900L, totalSuccesses = 1L)
        val previous = ConnectionRouteSnapshot(
            generatedAtUnixMs = 1_000L,
            endpoints = listOf(route),
        )
        val next = previous.copy(generatedAtUnixMs = 1_500L)

        assertTrue(connectionRouteSnapshotsUiEquivalent(previous, next))
    }

    @Test
    fun observesSnapshotTimeWhenItChangesTheVisibleRouteState() {
        val route = endpoint(index = 0, lastUsedUnixMs = 900L, totalSuccesses = 1L)
        val active = ConnectionRouteSnapshot(
            generatedAtUnixMs = 1_000L,
            endpoints = listOf(route),
        )
        val expired = active.copy(generatedAtUnixMs = 3_001L)

        assertFalse(connectionRouteSnapshotsUiEquivalent(active, expired))
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
        holePunchingMode: String? = null,
        locator: String = "https://node.example",
        targetNodeId: String? = null,
        lastUsedUnixMs: Long? = null,
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
            holePunchingMode = holePunchingMode,
            locator = locator,
            bootstrapRank = index,
            targetNodeId = targetNodeId,
            lastUsedUnixMs = lastUsedUnixMs,
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
