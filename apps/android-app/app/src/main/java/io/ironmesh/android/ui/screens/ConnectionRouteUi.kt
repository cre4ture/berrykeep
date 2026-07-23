package io.ironmesh.android.ui.screens

import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.ironmesh.android.R
import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot

internal data class RouteStateColors(
    val container: Color,
    val badge: Color,
    val onBadge: Color,
)

@Composable
internal fun RouteStateBadge(
    state: ConnectionRouteState,
    colors: RouteStateColors,
) {
    Surface(
        modifier = Modifier.widthIn(min = 72.dp),
        color = colors.badge,
        contentColor = colors.onBadge,
        shape = MaterialTheme.shapes.small,
    ) {
        Text(
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
            text = routeStatusLabel(state),
            style = MaterialTheme.typography.labelMedium,
        )
    }
}

@Composable
internal fun routeStateColors(state: ConnectionRouteState): RouteStateColors {
    val colors = MaterialTheme.colorScheme
    return when (state) {
        ConnectionRouteState.ACTIVE -> RouteStateColors(
            container = colors.primaryContainer,
            badge = colors.primary,
            onBadge = colors.onPrimary,
        )
        ConnectionRouteState.AVAILABLE -> RouteStateColors(
            container = colors.surfaceVariant,
            badge = colors.secondaryContainer,
            onBadge = colors.onSecondaryContainer,
        )
        ConnectionRouteState.CHECKING,
        ConnectionRouteState.STANDBY,
        -> RouteStateColors(
            container = colors.surfaceVariant,
            badge = colors.surface,
            onBadge = colors.onSurface,
        )
        ConnectionRouteState.PAUSED -> RouteStateColors(
            container = colors.tertiaryContainer,
            badge = colors.tertiary,
            onBadge = colors.onTertiary,
        )
        ConnectionRouteState.UNAVAILABLE -> RouteStateColors(
            container = colors.errorContainer,
            badge = colors.error,
            onBadge = colors.onError,
        )
    }
}

@Composable
internal fun routeStatusDetail(route: ConnectionRouteItem): String {
    return when (route.state) {
        ConnectionRouteState.ACTIVE -> routeQuality(route.endpoint)
        ConnectionRouteState.AVAILABLE -> stringResource(R.string.connection_paths_route_available)
        ConnectionRouteState.CHECKING -> stringResource(R.string.connection_paths_route_checking)
        ConnectionRouteState.PAUSED -> route.endpoint.circuitOpenUntilUnixMs?.let { until ->
            stringResource(R.string.connection_paths_route_retry_after, formatTimestamp(until))
        } ?: stringResource(R.string.connection_paths_state_paused)
        ConnectionRouteState.UNAVAILABLE -> stringResource(R.string.connection_paths_route_failed)
        ConnectionRouteState.STANDBY -> stringResource(R.string.connection_paths_route_standby)
    }
}

@Composable
internal fun formatConnectionLatency(value: Double): String {
    return stringResource(R.string.connection_paths_latency, value)
}

@Composable
private fun routeQuality(endpoint: ConnectionRouteEndpointSnapshot): String {
    return endpoint.ewmaLatencyMs?.let { latency -> formatConnectionLatency(latency) }
        ?: stringResource(R.string.connection_paths_route_working)
}

@Composable
private fun routeStatusLabel(state: ConnectionRouteState): String {
    return stringResource(
        when (state) {
            ConnectionRouteState.ACTIVE -> R.string.connection_paths_state_active
            ConnectionRouteState.AVAILABLE -> R.string.connection_paths_state_available
            ConnectionRouteState.CHECKING -> R.string.connection_paths_state_checking
            ConnectionRouteState.PAUSED -> R.string.connection_paths_state_paused
            ConnectionRouteState.UNAVAILABLE -> R.string.connection_paths_state_unavailable
            ConnectionRouteState.STANDBY -> R.string.connection_paths_state_standby
        },
    )
}
