package io.ironmesh.android.ui.screens

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.ironmesh.android.R

@Composable
internal fun ConnectionRouteCard(
    route: ConnectionRouteItem,
    expanded: Boolean,
    onToggleDetails: () -> Unit,
    onCopyEndpoint: () -> Unit,
) {
    val endpoint = route.endpoint
    val stateColors = routeStateColors(route.state)
    val toggleLabel = stringResource(
        if (expanded) R.string.connection_paths_hide_details else R.string.connection_paths_details,
    )

    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .animateContentSize(),
        color = if (route.state == ConnectionRouteState.ACTIVE) {
            stateColors.container
        } else {
            MaterialTheme.colorScheme.surface
        },
    ) {
        Column(
            modifier = Modifier.fillMaxWidth(),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 40.dp)
                    .clickable(onClickLabel = toggleLabel, onClick = onToggleDetails)
                    .padding(horizontal = 8.dp, vertical = 3.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                RouteRankIndicator(route, stateColors)
                Column(
                    modifier = Modifier.weight(1f),
                ) {
                    Text(
                        text = compactRouteDisplayLabel(endpoint),
                        style = MaterialTheme.typography.labelMedium,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = routeCompactSummary(route),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Text(
                    text = if (expanded) "⌃" else "›",
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            if (expanded) {
                HorizontalDivider()
                Column(
                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    ConnectionRouteDetails(route, onCopyEndpoint)
                }
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        }
    }
}

@Composable
private fun RouteRankIndicator(
    route: ConnectionRouteItem,
    stateColors: RouteStateColors,
) {
    Surface(
        modifier = Modifier.size(24.dp),
        color = stateColors.badge,
        contentColor = stateColors.onBadge,
        shape = MaterialTheme.shapes.small,
    ) {
        Box(contentAlignment = Alignment.Center) {
            Text(
                text = route.selectionRank.toString(),
                style = MaterialTheme.typography.labelSmall,
            )
        }
    }
}

@Composable
private fun routeCompactSummary(route: ConnectionRouteItem): String {
    val parts = mutableListOf(routeStatusLabel(route.state))
    route.endpoint.ewmaLatencyMs?.let { latency ->
        parts += formatConnectionLatency(latency)
    }
    parts += stringResource(
        R.string.connection_paths_route_score_compact,
        route.scoreBreakdown.total,
    )
    return parts.joinToString(" · ")
}

@Composable
private fun ConnectionRouteDetails(
    route: ConnectionRouteItem,
    onCopyEndpoint: () -> Unit,
) {
    val endpoint = route.endpoint
    val totalChecks = endpoint.totalSuccesses + endpoint.totalFailures

    Text(
        text = stringResource(R.string.connection_paths_selection_details_title),
        style = MaterialTheme.typography.titleSmall,
    )
    Text(
        text = routeSelectionReason(route),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    endpoint.targetNodeId?.let { targetNodeId ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_target_server_node),
            value = targetNodeId,
            selectable = true,
        )
    }
    if (endpoint.irohRelayUrls.isNotEmpty()) {
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_configured_iroh_relays),
            value = endpoint.irohRelayUrls.joinToString(separator = "\n"),
            selectable = true,
        )
    }
    endpoint.lastSuccessfulIrohRelayUrl?.let { relayUrl ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_last_successful_iroh_relay),
            value = relayUrl,
            selectable = true,
        )
    }
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_selection_rank),
        value = routeSelectionRankValue(route),
    )
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_route_score),
        value = routeScoreValue(route.scoreBreakdown.total),
    )
    Text(
        text = stringResource(R.string.connection_paths_score_breakdown_title),
        style = MaterialTheme.typography.titleSmall,
    )
    ConnectionScoreBreakdown(route.scoreBreakdown)

    ConnectionDetail(
        label = stringResource(R.string.connection_paths_endpoint),
        value = endpoint.locator,
        selectable = true,
    )
    TextButton(onClick = onCopyEndpoint) {
        Text(stringResource(R.string.connection_paths_copy_endpoint))
    }
    endpoint.ewmaLatencyMs?.let { latency ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_latency_label),
            value = formatConnectionLatency(latency),
        )
    }
    if (totalChecks > 0L) {
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_success_rate),
            value = stringResource(
                R.string.connection_paths_success_count,
                endpoint.totalSuccesses,
                totalChecks,
            ),
        )
    }
    route.lastCheckedUnixMs?.let { checkedAt ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_last_checked_label),
            value = formatTimestamp(checkedAt),
        )
    }
    endpoint.lastSuccessUnixMs?.let { lastSuccess ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_last_success_label),
            value = formatTimestamp(lastSuccess),
        )
    }
    endpoint.lastUsedUnixMs?.let { lastUsed ->
        ConnectionDetail(
            label = stringResource(R.string.connection_paths_last_used_label),
            value = formatTimestamp(lastUsed),
        )
    }
    endpoint.circuitOpenUntilUnixMs
        ?.takeIf { route.state == ConnectionRouteState.COOL_DOWN }
        ?.let { until ->
            ConnectionDetail(
                label = stringResource(R.string.connection_paths_retry_after),
                value = formatTimestamp(until),
            )
        }
    endpoint.lastError
        ?.takeIf { error -> error.isNotBlank() }
        ?.let { error ->
            ConnectionDetail(
                label = stringResource(R.string.connection_paths_technical_details),
                value = error,
                selectable = true,
                isError = true,
            )
        }
}

@Composable
private fun ConnectionScoreBreakdown(score: RouteScoreBreakdown) {
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_bootstrap_order),
        value = scorePointsAdded(score.bootstrapOrderPoints),
    )
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_latency),
        value = if (score.latencyIsEstimated) {
            stringResource(R.string.connection_paths_score_latency_estimated, score.latencyPoints)
        } else {
            stringResource(R.string.connection_paths_score_latency_measured, score.latencyPoints)
        },
    )
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_relay_penalty),
        value = if (score.relayPenaltyPoints > 0.0) {
            scorePointsAdded(score.relayPenaltyPoints)
        } else {
            stringResource(R.string.connection_paths_score_not_applied)
        },
    )
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_failure_penalty),
        value = if (score.failurePenaltyPoints > 0.0) {
            scorePointsAdded(score.failurePenaltyPoints)
        } else {
            stringResource(R.string.connection_paths_score_no_penalty)
        },
    )
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_throughput_credit),
        value = if (score.throughputCreditPoints > 0.0) {
            scorePointsCredited(score.throughputCreditPoints)
        } else {
            stringResource(R.string.connection_paths_score_no_credit)
        },
    )
}

@Composable
private fun routeSelectionRankValue(route: ConnectionRouteItem): String {
    return stringResource(
        R.string.connection_paths_selection_rank_value,
        route.selectionRank,
        route.selectionCandidateCount,
    )
}

@Composable
private fun routeScoreValue(score: Double): String {
    return stringResource(R.string.connection_paths_route_score_value, score)
}

@Composable
private fun scorePointsAdded(points: Double): String {
    return stringResource(R.string.connection_paths_score_points_added, points)
}

@Composable
private fun scorePointsCredited(points: Double): String {
    return stringResource(R.string.connection_paths_score_points_credited, points)
}

@Composable
private fun routeSelectionReason(route: ConnectionRouteItem): String {
    return when {
        route.state == ConnectionRouteState.COOL_DOWN -> {
            stringResource(R.string.connection_paths_selection_reason_cooling_down)
        }
        route.state == ConnectionRouteState.ACTIVE && route.selectionRank == 1 -> {
            stringResource(R.string.connection_paths_selection_reason_active_best)
        }
        route.state == ConnectionRouteState.ACTIVE -> {
            stringResource(
                R.string.connection_paths_selection_reason_active_ranked,
                route.selectionRank,
            )
        }
        route.selectionRank == 1 -> {
            stringResource(R.string.connection_paths_selection_reason_preferred)
        }
        else -> {
            stringResource(R.string.connection_paths_selection_reason_ranked_after)
        }
    }
}

@Composable
private fun ConnectionDetail(
    label: String,
    value: String,
    selectable: Boolean = false,
    isError: Boolean = false,
) {
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(label, style = MaterialTheme.typography.labelLarge)
        if (value.isNotBlank()) {
            val valueColor = if (isError) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant
            if (selectable) {
                SelectionContainer {
                    Text(value, style = MaterialTheme.typography.bodySmall, color = valueColor)
                }
            } else {
                Text(value, style = MaterialTheme.typography.bodySmall, color = valueColor)
            }
        }
    }
}
