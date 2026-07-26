package io.ironmesh.android.ui.screens

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
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

    Card(
        modifier = Modifier
            .fillMaxWidth()
            .animateContentSize()
            .clickable(onClick = onToggleDetails),
        colors = CardDefaults.cardColors(containerColor = stateColors.container),
    ) {
        Column(
            modifier = Modifier.padding(14.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.Top,
            ) {
                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(routeDisplayLabel(endpoint), style = MaterialTheme.typography.titleSmall)
                    Text(
                        text = routeStatusDetail(route),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Text(
                        text = routeSelectionSummary(route),
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                RouteStateBadge(route.state, stateColors)
            }

            TextButton(onClick = onToggleDetails) {
                Text(
                    stringResource(
                        if (expanded) R.string.connection_paths_hide_details else R.string.connection_paths_details,
                    ),
                )
            }

            if (expanded) {
                HorizontalDivider()
                ConnectionRouteDetails(route, onCopyEndpoint)
            }
        }
    }
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
    endpoint.circuitOpenUntilUnixMs
        ?.takeIf { route.state == ConnectionRouteState.PAUSED }
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
    ConnectionDetail(
        label = stringResource(R.string.connection_paths_score_active_route_credit),
        value = if (score.activeRouteCreditPoints > 0.0) {
            scorePointsCredited(score.activeRouteCreditPoints)
        } else {
            stringResource(R.string.connection_paths_score_not_applied)
        },
    )
}

@Composable
private fun routeSelectionSummary(route: ConnectionRouteItem): String {
    return stringResource(
        R.string.connection_paths_route_selection_summary,
        route.selectionRank,
        route.selectionCandidateCount,
        route.scoreBreakdown.total,
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
        route.state == ConnectionRouteState.PAUSED -> {
            stringResource(R.string.connection_paths_selection_reason_cooling_down)
        }
        route.endpoint.active && route.selectionRank == 1 -> {
            stringResource(R.string.connection_paths_selection_reason_active_best)
        }
        route.endpoint.active -> {
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
