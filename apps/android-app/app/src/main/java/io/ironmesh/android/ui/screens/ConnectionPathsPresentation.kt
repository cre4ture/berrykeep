package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot

internal enum class ConnectionOverviewState {
    CHECKING,
    DIRECT,
    RELAY,
    IMPROVING,
    UNAVAILABLE,
    ERROR,
}

internal enum class ConnectionRouteState {
    ACTIVE,
    AVAILABLE,
    CHECKING,
    PAUSED,
    UNAVAILABLE,
    STANDBY,
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
    val lastCheckedUnixMs: Long? = null,
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
    val routes = orderedEndpoints
        .map { endpoint ->
            ConnectionRouteItem(
                endpoint = endpoint,
                state = connectionRouteState(endpoint, snapshot?.generatedAtUnixMs),
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
        active?.pathKind == RELAY_TUNNEL_PATH_KIND && active.consecutiveFailures == 0 && !hasProbeInFlight -> {
            ConnectionOverviewState.RELAY
        }
        active != null && isDirectPath(active) && active.consecutiveFailures == 0 && !hasCoolingRoute && !hasProbeInFlight -> {
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
        endpoint.active -> ConnectionRouteState.ACTIVE
        isCoolingDown(endpoint, snapshotUnixMs) -> ConnectionRouteState.PAUSED
        endpoint.backgroundProbeInFlight -> ConnectionRouteState.CHECKING
        endpoint.totalSuccesses > 0L && endpoint.consecutiveFailures == 0 -> ConnectionRouteState.AVAILABLE
        endpoint.totalFailures > 0L -> ConnectionRouteState.UNAVAILABLE
        else -> ConnectionRouteState.STANDBY
    }
}

internal fun routeDisplayLabel(endpoint: ConnectionRouteEndpointSnapshot): String {
    val prefix = when (endpoint.pathKind) {
        RELAY_TUNNEL_PATH_KIND -> summarizeRelayLocator(endpoint.locator)?.let { "Relay via $it" } ?: "Relay"
        "direct_quic" -> "Direct QUIC"
        else -> "Direct HTTPS"
    }
    return endpoint.targetNodeId?.let { "$prefix to $it" } ?: prefix
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
        ConnectionRouteState.ACTIVE -> 0
        ConnectionRouteState.AVAILABLE -> 1
        ConnectionRouteState.CHECKING -> 2
        ConnectionRouteState.PAUSED -> 3
        ConnectionRouteState.UNAVAILABLE -> 4
        ConnectionRouteState.STANDBY -> 5
    }
}

private fun isDirectPath(endpoint: ConnectionRouteEndpointSnapshot): Boolean {
    return endpoint.pathKind == "direct_https" || endpoint.pathKind == "direct_quic"
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
