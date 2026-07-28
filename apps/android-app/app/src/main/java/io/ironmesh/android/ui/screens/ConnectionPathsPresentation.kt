package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot

internal enum class ConnectionOverviewState {
    CHECKING,
    DIRECT,
    DIRECT_QUIC,
    RELAY,
    IMPROVING,
    UNAVAILABLE,
    ERROR,
}

internal enum class ConnectionRouteState {
    LAST_USED,
    AVAILABLE,
    CHECKING,
    COOL_DOWN,
    UNAVAILABLE,
    UNKNOWN,
}

internal data class ConnectionOverview(
    val state: ConnectionOverviewState,
    val activeRoute: ConnectionRouteEndpointSnapshot? = null,
    val checkedAtUnixMs: Long? = null,
    val error: String? = null,
)

internal data class ConnectionRouteItem(
    val endpoint: ConnectionRouteEndpointSnapshot,
    val state: ConnectionRouteState,
    val selectionRank: Int,
    val selectionCandidateCount: Int,
    val scoreBreakdown: RouteScoreBreakdown,
    val lastCheckedUnixMs: Long? = null,
)

internal data class RouteScoreBreakdown(
    val total: Double,
    val bootstrapOrderPoints: Double,
    val latencyPoints: Double,
    val latencyIsEstimated: Boolean,
    val relayPenaltyPoints: Double,
    val failurePenaltyPoints: Double,
    val throughputCreditPoints: Double,
    val activeRouteCreditPoints: Double,
)

internal data class ConnectionPathsPresentation(
    val overview: ConnectionOverview,
    val routes: List<ConnectionRouteItem>,
)

internal fun connectionPathsPresentation(
    snapshot: ConnectionRouteSnapshot?,
    error: String?,
): ConnectionPathsPresentation {
    val orderedEndpoints = rankedConnectionEndpoints(snapshot)
    val selectionRanks = orderedEndpoints
        .mapIndexed { position, endpoint -> endpoint.index to position + 1 }
        .toMap()
    val routes = orderedEndpoints
        .map { endpoint ->
            ConnectionRouteItem(
                endpoint = endpoint,
                state = connectionRouteState(endpoint, snapshot?.generatedAtUnixMs),
                selectionRank = requireNotNull(selectionRanks[endpoint.index]),
                selectionCandidateCount = orderedEndpoints.size,
                scoreBreakdown = routeScoreBreakdown(endpoint),
                lastCheckedUnixMs = endpoint.lastMeasurementUnixMs
                    ?: endpoint.lastSuccessUnixMs
                    ?: endpoint.lastFailureUnixMs,
            )
        }
        .sortedWith(
            compareBy<ConnectionRouteItem> { routeStatePriority(it.state) }
                .thenBy { orderedEndpoints.indexOf(it.endpoint) },
        )

    return ConnectionPathsPresentation(
        overview = connectionOverview(snapshot, error, orderedEndpoints),
        routes = routes,
    )
}

internal fun connectionOverview(
    snapshot: ConnectionRouteSnapshot?,
    error: String?,
    orderedEndpoints: List<ConnectionRouteEndpointSnapshot> = rankedConnectionEndpoints(snapshot),
): ConnectionOverview {
    if (!error.isNullOrBlank()) {
        return ConnectionOverview(
            state = ConnectionOverviewState.ERROR,
            checkedAtUnixMs = snapshot?.generatedAtUnixMs,
            error = error,
        )
    }
    if (snapshot == null || orderedEndpoints.isEmpty()) {
        return ConnectionOverview(state = ConnectionOverviewState.CHECKING)
    }

    val active = snapshot.endpoints.firstOrNull { it.active } ?: orderedEndpoints.firstOrNull()
    val hasSuccess = snapshot.endpoints.any { it.totalSuccesses > 0L }
    val hasProbeInFlight = snapshot.endpoints.any { it.backgroundProbeInFlight }
    val hasCoolingRoute = snapshot.endpoints.any { isCoolingDown(it, snapshot.generatedAtUnixMs) }

    val state = when {
        !hasSuccess && snapshot.endpoints.all { it.totalFailures == 0L } -> ConnectionOverviewState.CHECKING
        !hasSuccess -> ConnectionOverviewState.UNAVAILABLE
        active != null &&
            isRelayPath(active) &&
            active.consecutiveFailures == 0 &&
            !hasProbeInFlight -> {
            ConnectionOverviewState.RELAY
        }
        active != null &&
            isDirectQuicPath(active) &&
            active.consecutiveFailures == 0 &&
            !hasCoolingRoute &&
            !hasProbeInFlight -> {
            ConnectionOverviewState.DIRECT_QUIC
        }
        active != null &&
            isDirectPath(active) &&
            active.consecutiveFailures == 0 &&
            !hasCoolingRoute &&
            !hasProbeInFlight -> {
            ConnectionOverviewState.DIRECT
        }
        else -> ConnectionOverviewState.IMPROVING
    }
    return ConnectionOverview(
        state = state,
        activeRoute = active,
        checkedAtUnixMs = snapshot.generatedAtUnixMs,
    )
}

internal fun connectionRouteState(
    endpoint: ConnectionRouteEndpointSnapshot,
    snapshotUnixMs: Long?,
): ConnectionRouteState {
    return when {
        endpoint.active -> ConnectionRouteState.LAST_USED
        isCoolingDown(endpoint, snapshotUnixMs) -> ConnectionRouteState.COOL_DOWN
        endpoint.backgroundProbeInFlight -> ConnectionRouteState.CHECKING
        endpoint.totalSuccesses > 0L && endpoint.consecutiveFailures == 0 -> ConnectionRouteState.AVAILABLE
        endpoint.totalFailures > 0L -> ConnectionRouteState.UNAVAILABLE
        else -> ConnectionRouteState.UNKNOWN
    }
}

internal fun routeDisplayLabel(endpoint: ConnectionRouteEndpointSnapshot): String {
    val prefix = when (endpoint.pathKind) {
        RELAY_TUNNEL_PATH_KIND -> summarizeRelayLocator(endpoint.locator)?.let { "Relay via $it" } ?: "Relay"
        "direct_quic" -> when (endpoint.holePunchingMode) {
            HOLE_PUNCHING_MODE_DIRECT -> "Direct via NAT (QUIC)"
            HOLE_PUNCHING_MODE_RELAY -> "QUIC via Relay"
            else -> "Direct QUIC"
        }
        else -> "Direct HTTPS"
    }
    return endpoint.targetNodeId?.let { "$prefix to $it" } ?: prefix
}

internal fun routeScoreBreakdown(endpoint: ConnectionRouteEndpointSnapshot): RouteScoreBreakdown {
    val latencyIsEstimated = endpoint.ewmaLatencyMs == null
    return RouteScoreBreakdown(
        total = endpoint.score,
        bootstrapOrderPoints = endpoint.bootstrapRank.toDouble(),
        latencyPoints = endpoint.ewmaLatencyMs ?: ROUTE_UNKNOWN_LATENCY_POINTS,
        latencyIsEstimated = latencyIsEstimated,
        relayPenaltyPoints = if (endpoint.pathKind == RELAY_TUNNEL_PATH_KIND) {
            ROUTE_RELAY_PENALTY_POINTS
        } else {
            0.0
        },
        failurePenaltyPoints = endpoint.consecutiveFailures * ROUTE_FAILURE_PENALTY_POINTS,
        throughputCreditPoints = endpoint.ewmaThroughputBytesPerSec?.let { throughput ->
            (throughput / ROUTE_THROUGHPUT_BYTES_PER_POINT)
                .coerceAtMost(ROUTE_MAX_THROUGHPUT_CREDIT_POINTS)
        }
            ?: 0.0,
        activeRouteCreditPoints = if (endpoint.active) ROUTE_ACTIVE_ROUTE_CREDIT_POINTS else 0.0,
    )
}

internal fun isCoolingDown(
    endpoint: ConnectionRouteEndpointSnapshot,
    snapshotUnixMs: Long?,
): Boolean {
    val until = endpoint.circuitOpenUntilUnixMs ?: return false
    return snapshotUnixMs != null && until > snapshotUnixMs
}

private fun rankedConnectionEndpoints(snapshot: ConnectionRouteSnapshot?): List<ConnectionRouteEndpointSnapshot> {
    if (snapshot == null) {
        return emptyList()
    }
    val byIndex = snapshot.endpoints.associateBy { it.index }
    val ranked = snapshot.rankedIndices.mapNotNull(byIndex::get)
    val missing = snapshot.endpoints.filter { endpoint -> endpoint.index !in snapshot.rankedIndices }
    return ranked + missing
}

private fun routeStatePriority(state: ConnectionRouteState): Int {
    return when (state) {
        ConnectionRouteState.LAST_USED -> 0
        ConnectionRouteState.AVAILABLE -> 1
        ConnectionRouteState.CHECKING -> 2
        ConnectionRouteState.COOL_DOWN -> 3
        ConnectionRouteState.UNAVAILABLE -> 4
        ConnectionRouteState.UNKNOWN -> 5
    }
}

private fun isDirectPath(endpoint: ConnectionRouteEndpointSnapshot): Boolean {
    return endpoint.pathKind == "direct_https" || isDirectQuicPath(endpoint)
}

private fun isDirectQuicPath(endpoint: ConnectionRouteEndpointSnapshot): Boolean {
    return endpoint.pathKind == "direct_quic" &&
        endpoint.holePunchingMode == HOLE_PUNCHING_MODE_DIRECT
}

private fun isRelayPath(endpoint: ConnectionRouteEndpointSnapshot): Boolean {
    return endpoint.pathKind == RELAY_TUNNEL_PATH_KIND ||
        (endpoint.pathKind == "direct_quic" && endpoint.holePunchingMode == HOLE_PUNCHING_MODE_RELAY)
}

private fun summarizeRelayLocator(locator: String): String? {
    val rendezvousIndex = locator.lastIndexOf("@")
    if (rendezvousIndex < 0 || rendezvousIndex + 1 >= locator.length) {
        return null
    }
    return summarizeUrl(locator.substring(rendezvousIndex + 1))
}

private fun summarizeUrl(value: String): String {
    return try {
        val uri = java.net.URI(value)
        val host = uri.host ?: value
        if (uri.port > 0) {
            "$host:${uri.port}"
        } else {
            host
        }
    } catch (_: Exception) {
        value
    }
}

private const val RELAY_TUNNEL_PATH_KIND = "relay_tunnel"
private const val ROUTE_UNKNOWN_LATENCY_POINTS = 75.0
private const val ROUTE_RELAY_PENALTY_POINTS = 500.0
private const val ROUTE_FAILURE_PENALTY_POINTS = 250.0
private const val ROUTE_THROUGHPUT_BYTES_PER_POINT = 250_000.0
private const val ROUTE_MAX_THROUGHPUT_CREDIT_POINTS = 50.0
private const val ROUTE_ACTIVE_ROUTE_CREDIT_POINTS = 50.0
private const val HOLE_PUNCHING_MODE_DIRECT = "direct"
private const val HOLE_PUNCHING_MODE_RELAY = "relay"
