package io.ironmesh.android.ui.screens

import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import io.ironmesh.android.R

internal data class RouteStateColors(
    val container: Color,
    val badge: Color,
    val onBadge: Color,
)

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
internal fun formatConnectionLatency(value: Double): String {
    return stringResource(R.string.connection_paths_latency, value)
}

@Composable
internal fun routeStatusLabel(state: ConnectionRouteState): String {
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
