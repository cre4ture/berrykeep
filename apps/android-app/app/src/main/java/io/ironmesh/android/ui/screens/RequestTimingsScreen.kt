package io.ironmesh.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.ironmesh.android.data.ConnectionRouteAttemptSnapshot
import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.ui.MainUiState
import io.ironmesh.android.ui.components.HeroTone
import io.ironmesh.android.ui.components.MetricPill
import io.ironmesh.android.ui.components.SectionCard
import io.ironmesh.android.ui.components.StatusHeroCard
import java.text.DateFormat
import java.util.Date
import kotlin.math.abs

private data class TimedRequest(
    val endpoint: ConnectionRouteEndpointSnapshot,
    val attempt: ConnectionRouteAttemptSnapshot,
)

private data class SessionPoolTotals(
    val connectCount: Long = 0L,
    val reuseCount: Long = 0L,
    val resetCount: Long = 0L,
    val connectDurationUs: Long = 0L,
    val relayPairingDurationUs: Long = 0L,
)

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun RequestTimingsScreen(
    state: MainUiState,
    onRefresh: () -> Unit,
) {
    val snapshot = state.connectionRoutes
    val requests = snapshot
        ?.endpoints
        .orEmpty()
        .flatMap { endpoint -> endpoint.recentAttempts.map { attempt -> TimedRequest(endpoint, attempt) } }
        .sortedByDescending { request -> request.attempt.finishedUnixMs ?: request.attempt.startedUnixMs }
    val successful = requests.filter { request ->
        request.attempt.outcome == "success" && request.attempt.totalDurationUs != null
    }
    val totalAverage = successful.mapNotNull { it.attempt.totalDurationUs }.averageOrNull()
    val serverAverage = successful.mapNotNull { it.attempt.serverProcessingDurationUs }.averageOrNull()
    val transportAverage = successful.mapNotNull { it.attempt.transportOverheadUs }.averageOrNull()
    val networkAverage = successful.mapNotNull { it.attempt.networkTransferDurationUs }.averageOrNull()
    val pool = snapshot?.endpoints.orEmpty().fold(SessionPoolTotals()) { total, endpoint ->
        val stats = endpoint.transportSessionPool
        total.copy(
            connectCount = total.connectCount + stats.connectCount,
            reuseCount = total.reuseCount + stats.reuseCount,
            resetCount = total.resetCount + stats.resetCount,
            connectDurationUs = total.connectDurationUs + stats.connectDurationUs,
            relayPairingDurationUs = total.relayPairingDurationUs + stats.relayPairingDurationUs,
        )
    }
    val relayConnectCount = snapshot?.endpoints.orEmpty()
        .filter { endpoint -> endpoint.pathKind == "relay_tunnel" }
        .sumOf { endpoint -> endpoint.transportSessionPool.connectCount }
    val averageConnectUs = pool.connectDurationUs.takeIf { pool.connectCount > 0L }
        ?.toDouble()
        ?.div(pool.connectCount)
    val averageRelayPairingUs = pool.relayPairingDurationUs.takeIf { relayConnectCount > 0L }
        ?.toDouble()
        ?.div(relayConnectCount)
    val bestClockSample = successful
        .map { request -> request.attempt }
        .filter { attempt -> attempt.clockOffsetUs != null && attempt.clockUncertaintyUs != null }
        .minByOrNull { attempt -> attempt.clockUncertaintyUs ?: Long.MAX_VALUE }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        StatusHeroCard(
            title = if (requests.isEmpty()) "Waiting for remote requests" else "${requests.size} recent requests",
            subtitle = state.connectionRoutesError
                ?: "Live timings from the same cached Rust client used by the native gallery and embedded web UI.",
            tone = if (state.connectionRoutesError == null) HeroTone.Neutral else HeroTone.Error,
        ) {
            Button(onClick = onRefresh, enabled = !state.connectionRoutesLoading) {
                Text(if (state.connectionRoutesLoading) "Refreshing..." else "Refresh snapshot")
            }
        }

        FlowRow(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            MetricPill("Average total", formatDurationUs(totalAverage))
            MetricPill("Average server", formatDurationUs(serverAverage))
            MetricPill("Average transport", formatDurationUs(transportAverage))
            MetricPill("Network / relay", formatDurationUs(networkAverage))
            MetricPill("Session setup", formatDurationUs(averageConnectUs))
            MetricPill("Relay pairing", formatDurationUs(averageRelayPairingUs))
            MetricPill("Clock estimate", formatClock(bestClockSample))
        }

        SectionCard(
            title = "Where time is measured",
            supportingText = "Server processing uses the node's monotonic clock. Transport is total client time minus server time, so it includes upload, download, network queues, and relay forwarding.",
        ) {
            Text(
                text = "The relay cannot inspect request boundaries inside the inner mTLS tunnel. It reports exact tunnel pairing/wait time; per-request relay cost remains part of end-to-end transport overhead.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                text = "Sessions: ${pool.connectCount} connected, ${pool.reuseCount} reused, ${pool.resetCount} reset.",
                style = MaterialTheme.typography.bodySmall,
            )
        }

        SectionCard(
            title = "Recent real requests",
            supportingText = "Up to 64 requests are retained for each direct or relay path.",
        ) {
            if (requests.isEmpty()) {
                Text(
                    text = "Open the gallery or another remote screen. Measurements will appear here automatically.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            requests.take(64).forEachIndexed { index, request ->
                if (index > 0) {
                    HorizontalDivider()
                }
                RequestTimingRow(request)
            }
        }
    }
}

@Composable
private fun RequestTimingRow(request: TimedRequest) {
    val attempt = request.attempt
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                text = "${attempt.method} ${requestPath(attempt.url)}",
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = attempt.statusCode?.toString() ?: attempt.outcome,
                color = if (attempt.outcome == "success") {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.error
                },
                style = MaterialTheme.typography.labelLarge,
            )
        }
        Text(
            text = "${request.endpoint.pathKind} · ${request.endpoint.locator} · ${formatTimestampMs(attempt.startedUnixMs)}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            text = buildString {
                append(
                    if (attempt.outcome != "success") {
                        "Failed after "
                    } else if (attempt.responseBodyComplete) {
                        "Total "
                    } else {
                        "Response head "
                    },
                )
                append(formatDurationUs(attempt.totalDurationUs?.toDouble()))
                append(" · Server ")
                append(formatDurationUs(attempt.serverProcessingDurationUs?.toDouble()))
                append(" · Transport ")
                append(formatDurationUs(attempt.transportOverheadUs?.toDouble()))
                append(" · Setup ")
                append(formatDurationUs(attempt.sessionSetupDurationUs.toDouble()))
                if (attempt.sessionReused) append(" reused")
                if (attempt.relayPairingDurationUs > 0L) {
                    append(" (relay ")
                    append(formatDurationUs(attempt.relayPairingDurationUs.toDouble()))
                    append(")")
                }
                append(" · Network/relay ")
                append(formatDurationUs(attempt.networkTransferDurationUs?.toDouble()))
            },
            style = MaterialTheme.typography.bodyMedium,
        )
        Text(
            text = "Sent ${formatBytes(attempt.requestBytes)} · received ${formatBytes(attempt.responseBytes)} · clock ${formatClock(attempt)}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        attempt.error?.takeIf { error -> error.isNotBlank() }?.let { error ->
            Text(error, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
        }
    }
}

private fun List<Long>.averageOrNull(): Double? {
    return if (isEmpty()) null else average()
}

private fun formatDurationUs(value: Double?): String {
    if (value == null || !value.isFinite()) return "n/a"
    val milliseconds = value / 1_000.0
    return if (milliseconds < 10.0) "%.2f ms".format(milliseconds) else "%.1f ms".format(milliseconds)
}

private fun formatClock(attempt: ConnectionRouteAttemptSnapshot?): String {
    val offset = attempt?.clockOffsetUs ?: return "n/a"
    val uncertainty = attempt.clockUncertaintyUs ?: return "n/a"
    val sign = if (offset >= 0L) "+" else "-"
    return "$sign%.1f ± %.1f ms".format(abs(offset).toDouble() / 1_000.0, uncertainty.toDouble() / 1_000.0)
}

private fun formatBytes(value: Long): String {
    return if (value < 1_024L) "$value B" else "%.1f KiB".format(value.toDouble() / 1_024.0)
}

private fun formatTimestampMs(value: Long): String {
    return DateFormat.getTimeInstance(DateFormat.MEDIUM).format(Date(value))
}

private fun requestPath(url: String): String {
    val schemeIndex = url.indexOf("://")
    if (schemeIndex < 0) return url
    val pathIndex = url.indexOf('/', startIndex = schemeIndex + 3)
    return if (pathIndex < 0) "/" else url.substring(pathIndex)
}
