package io.ironmesh.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
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
import androidx.compose.ui.text.style.TextOverflow
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

    if (presentation.routes.isNotEmpty()) {
        ConnectionRoutesList(
            presentation = presentation,
            expandedRouteIndex = expandedRouteIndex,
            refreshing = state.connectionRoutesLoading,
            onRefresh = onRefresh,
            onToggleDetails = { routeIndex ->
                expandedRouteIndex = if (expandedRouteIndex == routeIndex) null else routeIndex
            },
            onCopyEndpoint = { endpoint ->
                clipboardManager.setText(AnnotatedString(endpoint))
            },
        )
        return
    }

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

        SectionCard(
            title = stringResource(R.string.connection_paths_empty_title),
            supportingText = stringResource(R.string.connection_paths_empty_body),
        ) {}
    }
}

@Composable
private fun ConnectionRoutesList(
    presentation: ConnectionPathsPresentation,
    expandedRouteIndex: Int?,
    refreshing: Boolean,
    onRefresh: () -> Unit,
    onToggleDetails: (Int) -> Unit,
    onCopyEndpoint: (String) -> Unit,
) {
    LazyColumn(modifier = Modifier.fillMaxSize()) {
        item(key = "connection-overview") {
            CompactConnectionOverview(presentation)
        }
        items(
            items = presentation.routes,
            key = { route -> route.endpoint.index },
        ) { route ->
            ConnectionRouteCard(
                route = route,
                expanded = expandedRouteIndex == route.endpoint.index,
                onToggleDetails = { onToggleDetails(route.endpoint.index) },
                onCopyEndpoint = { onCopyEndpoint(route.endpoint.locator) },
            )
        }
        if (
            presentation.overview.state == ConnectionOverviewState.ERROR ||
            presentation.overview.state == ConnectionOverviewState.UNAVAILABLE
        ) {
            item(key = "connection-issue") {
                ConnectionIssueCard(
                    overview = presentation.overview,
                    refreshing = refreshing,
                    onRefresh = onRefresh,
                )
            }
        }
        item(key = "selection-explanation") {
            SectionCard(
                title = stringResource(R.string.connection_paths_selection_title),
                supportingText = stringResource(R.string.connection_paths_selection_body),
                modifier = Modifier.padding(top = 12.dp),
            ) {
                Text(
                    text = stringResource(R.string.connection_paths_selection_score_additions),
                    style = MaterialTheme.typography.bodySmall,
                )
                Text(
                    text = stringResource(R.string.connection_paths_selection_score_credits),
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
        if (presentation.overview.state == ConnectionOverviewState.RELAY) {
            item(key = "relay-note") {
                SectionCard(
                    title = stringResource(R.string.connection_paths_relay_note_title),
                    supportingText = stringResource(R.string.connection_paths_relay_note_body),
                    modifier = Modifier.padding(top = 12.dp),
                ) {}
            }
        }
        if (presentation.overview.state == ConnectionOverviewState.DIRECT_QUIC) {
            item(key = "direct-quic-note") {
                SectionCard(
                    title = stringResource(R.string.connection_paths_direct_quic_note_title),
                    supportingText = stringResource(R.string.connection_paths_direct_quic_note_body),
                    modifier = Modifier.padding(top = 12.dp),
                ) {}
            }
        }
    }
}

@Composable
private fun CompactConnectionOverview(presentation: ConnectionPathsPresentation) {
    val overview = presentation.overview
    Surface(
        modifier = Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.surfaceVariant,
        shape = RoundedCornerShape(topStart = 12.dp, topEnd = 12.dp),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 36.dp)
                .padding(horizontal = 8.dp, vertical = 3.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = stringResource(overviewTitle(overview.state)),
                    style = MaterialTheme.typography.labelMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    text = compactOverviewSubtitle(overview),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Text(
                text = stringResource(
                    R.string.connection_paths_compact_route_count,
                    presentation.routes.size,
                ),
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun compactOverviewSubtitle(overview: ConnectionOverview): String {
    if (overview.activeRouteCount > 1) {
        return stringResource(R.string.connection_paths_active_route_count, overview.activeRouteCount)
    }
    val activeRoute = overview.displayRoute
    if (
        activeRoute != null &&
        overview.state != ConnectionOverviewState.ERROR &&
        overview.state != ConnectionOverviewState.UNAVAILABLE
    ) {
        val parts = mutableListOf(compactRouteDisplayLabel(activeRoute))
        activeRoute.ewmaLatencyMs?.let { latency ->
            parts += formatConnectionLatency(latency)
        }
        return parts.joinToString(" · ")
    }
    return stringResource(
        when (overview.state) {
            ConnectionOverviewState.ERROR -> R.string.connection_paths_error_body
            ConnectionOverviewState.UNAVAILABLE -> R.string.connection_paths_unavailable_body
            else -> R.string.connection_paths_checking_body
        },
    )
}

@Composable
private fun ConnectionIssueCard(
    overview: ConnectionOverview,
    refreshing: Boolean,
    onRefresh: () -> Unit,
) {
    SectionCard(
        title = stringResource(overviewTitle(overview.state)),
        supportingText = compactOverviewSubtitle(overview),
        modifier = Modifier.padding(top = 12.dp),
    ) {
        overview.error
            ?.takeIf { it.isNotBlank() }
            ?.let { error ->
                SelectionContainer {
                    Text(error, style = MaterialTheme.typography.bodySmall)
                }
            }
        Button(onClick = onRefresh, enabled = !refreshing) {
            Text(stringResource(R.string.connection_paths_try_again))
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
    if (overview.activeRouteCount > 1) {
        return stringResource(R.string.connection_paths_active_route_count, overview.activeRouteCount)
    }
    val activeRoute = overview.displayRoute
    if (activeRoute != null && overview.state !in setOf(ConnectionOverviewState.ERROR, ConnectionOverviewState.UNAVAILABLE)) {
        val quality = activeRoute.ewmaLatencyMs?.let { latency -> formatConnectionLatency(latency) }
            ?: stringResource(R.string.connection_paths_route_last_used)
        return stringResource(
            R.string.connection_paths_last_used_route,
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
            ConnectionOverviewState.DIRECT_QUIC,
            ConnectionOverviewState.RELAY,
            ConnectionOverviewState.MULTIPLE,
            ConnectionOverviewState.AVAILABLE,
            ConnectionOverviewState.IMPROVING,
            -> R.string.connection_paths_checking_body
        },
    )
}

private fun overviewTitle(state: ConnectionOverviewState): Int {
    return when (state) {
        ConnectionOverviewState.CHECKING -> R.string.connection_paths_checking_title
        ConnectionOverviewState.DIRECT -> R.string.connection_paths_direct_title
        ConnectionOverviewState.DIRECT_QUIC -> R.string.connection_paths_direct_quic_title
        ConnectionOverviewState.RELAY -> R.string.connection_paths_relay_title
        ConnectionOverviewState.MULTIPLE -> R.string.connection_paths_multiple_title
        ConnectionOverviewState.AVAILABLE -> R.string.connection_paths_available_title
        ConnectionOverviewState.IMPROVING -> R.string.connection_paths_improving_title
        ConnectionOverviewState.UNAVAILABLE -> R.string.connection_paths_unavailable_title
        ConnectionOverviewState.ERROR -> R.string.connection_paths_error_title
    }
}

private fun overviewHeroTone(state: ConnectionOverviewState): HeroTone {
    return when (state) {
        ConnectionOverviewState.DIRECT -> HeroTone.Good
        ConnectionOverviewState.DIRECT_QUIC -> HeroTone.Good
        ConnectionOverviewState.MULTIPLE -> HeroTone.Good
        ConnectionOverviewState.AVAILABLE -> HeroTone.Good
        ConnectionOverviewState.RELAY,
        ConnectionOverviewState.IMPROVING,
        -> HeroTone.Warning
        ConnectionOverviewState.UNAVAILABLE,
        ConnectionOverviewState.ERROR,
        -> HeroTone.Error
        ConnectionOverviewState.CHECKING -> HeroTone.Neutral
    }
}
