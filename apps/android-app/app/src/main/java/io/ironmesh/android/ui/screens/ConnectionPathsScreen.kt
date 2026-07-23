package io.ironmesh.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import io.ironmesh.android.R
import io.ironmesh.android.ui.MainUiState
import io.ironmesh.android.ui.components.HeroTone
import io.ironmesh.android.ui.components.SectionCard
import io.ironmesh.android.ui.components.StatusHeroCard

@Composable
fun ConnectionPathsScreen(
    state: MainUiState,
    onRefresh: () -> Unit,
) {
    val presentation = remember(state.connectionRoutes, state.connectionRoutesError) {
        connectionPathsPresentation(state.connectionRoutes, state.connectionRoutesError)
    }
    val clipboardManager = LocalClipboardManager.current
    var expandedRouteIndex by rememberSaveable { mutableStateOf<Int?>(null) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        ConnectionOverviewCard(
            overview = presentation.overview,
            onRefresh = onRefresh,
            refreshing = state.connectionRoutesLoading,
        )

        if (presentation.routes.isEmpty()) {
            SectionCard(
                title = stringResource(R.string.connection_paths_empty_title),
                supportingText = stringResource(R.string.connection_paths_empty_body),
            ) {}
        } else {
            SectionCard(
                title = stringResource(R.string.connection_paths_routes_title),
                supportingText = stringResource(R.string.connection_paths_routes_body),
            ) {
                presentation.routes.forEach { route ->
                    ConnectionRouteCard(
                        route = route,
                        expanded = expandedRouteIndex == route.endpoint.index,
                        onToggleDetails = {
                            expandedRouteIndex = if (expandedRouteIndex == route.endpoint.index) {
                                null
                            } else {
                                route.endpoint.index
                            }
                        },
                        onCopyEndpoint = {
                            clipboardManager.setText(AnnotatedString(route.endpoint.locator))
                        },
                    )
                }
            }
        }

        if (presentation.overview.state == ConnectionOverviewState.RELAY) {
            SectionCard(
                title = stringResource(R.string.connection_paths_relay_note_title),
                supportingText = stringResource(R.string.connection_paths_relay_note_body),
            ) {}
        }
    }
}

@Composable
private fun ConnectionOverviewCard(
    overview: ConnectionOverview,
    refreshing: Boolean,
    onRefresh: () -> Unit,
) {
    var showErrorDetails by rememberSaveable { mutableStateOf(false) }

    StatusHeroCard(
        title = stringResource(overviewTitle(overview.state)),
        subtitle = overviewSubtitle(overview),
        tone = overviewHeroTone(overview.state),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = stringResource(R.string.connection_paths_live),
                style = MaterialTheme.typography.labelLarge,
            )
            overview.checkedAtUnixMs?.let { checkedAt ->
                Text(
                    text = stringResource(R.string.connection_paths_last_checked, formatTimestamp(checkedAt)),
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
        if (overview.state == ConnectionOverviewState.ERROR) {
            TextButton(onClick = { showErrorDetails = !showErrorDetails }) {
                Text(
                    stringResource(
                        if (showErrorDetails) R.string.connection_paths_hide_details
                        else R.string.connection_paths_technical_details,
                    ),
                )
            }
            if (showErrorDetails) {
                SelectionContainer {
                    Text(overview.error.orEmpty(), style = MaterialTheme.typography.bodySmall)
                }
            }
        }
        if (overview.state in setOf(ConnectionOverviewState.ERROR, ConnectionOverviewState.UNAVAILABLE)) {
            Button(onClick = onRefresh, enabled = !refreshing) {
                Text(stringResource(R.string.connection_paths_try_again))
            }
        }
    }
}

@Composable
private fun overviewSubtitle(overview: ConnectionOverview): String {
    val activeRoute = overview.activeRoute
    if (activeRoute != null && overview.state !in setOf(ConnectionOverviewState.ERROR, ConnectionOverviewState.UNAVAILABLE)) {
        val quality = activeRoute.ewmaLatencyMs?.let { latency -> formatConnectionLatency(latency) }
            ?: stringResource(R.string.connection_paths_route_working)
        return stringResource(
            R.string.connection_paths_active_route,
            routeDisplayLabel(activeRoute),
            quality,
        )
    }
    return stringResource(
        when (overview.state) {
            ConnectionOverviewState.CHECKING -> R.string.connection_paths_checking_body
            ConnectionOverviewState.UNAVAILABLE -> R.string.connection_paths_unavailable_body
            ConnectionOverviewState.ERROR -> R.string.connection_paths_error_body
            ConnectionOverviewState.DIRECT,
            ConnectionOverviewState.RELAY,
            ConnectionOverviewState.IMPROVING,
            -> R.string.connection_paths_checking_body
        },
    )
}

private fun overviewTitle(state: ConnectionOverviewState): Int {
    return when (state) {
        ConnectionOverviewState.CHECKING -> R.string.connection_paths_checking_title
        ConnectionOverviewState.DIRECT -> R.string.connection_paths_direct_title
        ConnectionOverviewState.RELAY -> R.string.connection_paths_relay_title
        ConnectionOverviewState.IMPROVING -> R.string.connection_paths_improving_title
        ConnectionOverviewState.UNAVAILABLE -> R.string.connection_paths_unavailable_title
        ConnectionOverviewState.ERROR -> R.string.connection_paths_error_title
    }
}

private fun overviewHeroTone(state: ConnectionOverviewState): HeroTone {
    return when (state) {
        ConnectionOverviewState.DIRECT -> HeroTone.Good
        ConnectionOverviewState.RELAY,
        ConnectionOverviewState.IMPROVING,
        -> HeroTone.Warning
        ConnectionOverviewState.UNAVAILABLE,
        ConnectionOverviewState.ERROR,
        -> HeroTone.Error
        ConnectionOverviewState.CHECKING -> HeroTone.Neutral
    }
}
