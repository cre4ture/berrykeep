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
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.ironmesh.android.data.ConnectionRouteAttemptSnapshot
import io.ironmesh.android.ui.MainUiState
import io.ironmesh.android.ui.components.HeroTone
import io.ironmesh.android.ui.components.MetricPill
import io.ironmesh.android.ui.components.SectionCard
import io.ironmesh.android.ui.components.StatusHeroCard
import java.text.DateFormat
import java.util.Date
import kotlin.math.abs

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun RequestTimingsScreen(
    state: MainUiState,
    onRefresh: () -> Unit,
    onResetMeasurement: () -> Unit,
    onRunStoreIndexTest: () -> Unit,
) {
    val measurement = timingMeasurementPresentation(state.connectionRoutes)
    val busy = state.connectionRoutesLoading ||
        state.timingMeasurementResetting ||
        state.timingStoreIndexTestRunning
    var expandedRequestStartedAt by rememberSaveable { mutableStateOf<Long?>(null) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        StatusHeroCard(
            title = timingHeroTitle(state, measurement.requests.size),
            subtitle = timingHeroSubtitle(state, measurement.requests.size),
            tone = if (state.connectionRoutesError == null) HeroTone.Neutral else HeroTone.Error,
        ) {
            FlowRow(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                OutlinedButton(
                    onClick = onResetMeasurement,
                    enabled = !busy,
                ) {
                    Text(if (state.timingMeasurementResetting) "Resetting..." else "Reset measurement")
                }
                Button(
                    onClick = onRunStoreIndexTest,
                    enabled = !busy,
                ) {
                    Text(if (state.timingStoreIndexTestRunning) "Running test..." else "Run store-index test")
                }
            }
        }

        FlowRow(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            MetricPill("Average total", formatDurationUs(measurement.averageTotalUs))
            MetricPill("Average server", formatDurationUs(measurement.averageServerUs))
            MetricPill("Average transport", formatDurationUs(measurement.averageTransportUs))
            MetricPill("Session setup", formatDurationUs(measurement.averageSessionSetupUs))
            measurement.bestClockSample?.let { sample ->
                MetricPill("Clock estimate", formatClock(sample))
            }
        }

        if (measurement.sessionPool.connectCount > 0L || measurement.sessionPool.reuseCount > 0L) {
            SectionCard(
                title = "Transport sessions",
                supportingText = "Only session activity recorded after the last reset is shown.",
            ) {
                Text(
                    text = "${measurement.sessionPool.connectCount} connected, " +
                        "${measurement.sessionPool.reuseCount} reused, " +
                        "${measurement.sessionPool.resetCount} reset.",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Text(
                    text = "Average setup ${formatDurationUs(measurement.averageSessionSetupUs)}" +
                        relayPairingSummary(measurement.averageRelayPairingUs),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        SectionCard(
            title = "Captured requests",
            supportingText = if (measurement.requests.isEmpty()) {
                "Reset the measurement, then run the read-only test or use another remote screen. " +
                    "Only requests after the reset will appear here."
            } else {
                "${measurement.successfulRequestCount} successful request(s) in the current measurement. " +
                    "Tap a request for its full breakdown."
            },
        ) {
            if (measurement.requests.isEmpty()) {
                Text(
                    text = "The store-index test only reads the top-level index and uses the same cached client as the app.",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            measurement.requests.take(64).forEachIndexed { index, request ->
                if (index > 0) {
                    HorizontalDivider()
                }
                RequestTimingRow(
                    request = request,
                    expanded = expandedRequestStartedAt == request.attempt.startedUnixMs,
                    onToggle = {
                        expandedRequestStartedAt = if (expandedRequestStartedAt == request.attempt.startedUnixMs) {
                            null
                        } else {
                            request.attempt.startedUnixMs
                        }
                    },
                )
            }
        }

        TextButton(onClick = onRefresh, enabled = !busy) {
            Text(if (state.connectionRoutesLoading) "Refreshing..." else "Refresh snapshot")
        }
    }
}

@Composable
private fun RequestTimingRow(
    request: TimedRequest,
    expanded: Boolean,
    onToggle: () -> Unit,
) {
    val attempt = request.attempt
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
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
            text = "${formatDurationUs(attempt.totalDurationUs?.toDouble())} total / " +
                "${formatDurationUs(attempt.transportOverheadUs?.toDouble())} transport",
            style = MaterialTheme.typography.bodyMedium,
        )
        Text(
            text = "${request.endpoint.pathKind} / ${formatTimestampMs(attempt.startedUnixMs)}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        TextButton(onClick = onToggle) {
            Text(if (expanded) "Hide details" else "Details")
        }
        if (expanded) {
            TimingDetails(request)
        }
    }
}

@Composable
private fun TimingDetails(request: TimedRequest) {
    val attempt = request.attempt
    Column(verticalArrangement = Arrangement.spacedBy(5.dp)) {
        TimingDetail("Endpoint", request.endpoint.locator)
        TimingDetail("Server processing", formatDurationUs(attempt.serverProcessingDurationUs?.toDouble()))
        TimingDetail("Transport overhead", formatDurationUs(attempt.transportOverheadUs?.toDouble()))
        TimingDetail("Network and relay", formatDurationUs(attempt.networkTransferDurationUs?.toDouble()))
        TimingDetail(
            "Session",
            buildString {
                append(formatDurationUs(attempt.sessionSetupDurationUs.toDouble()))
                append(" setup")
                if (attempt.sessionReused) append(", reused")
                if (attempt.relayPairingDurationUs > 0L) {
                    append(", ")
                    append(formatDurationUs(attempt.relayPairingDurationUs.toDouble()))
                    append(" relay pairing")
                }
            },
        )
        TimingDetail(
            "Transfer",
            "Sent ${formatBytes(attempt.requestBytes)}, received ${formatBytes(attempt.responseBytes)}",
        )
        TimingDetail("Clock estimate", formatClock(attempt))
        attempt.error?.takeIf { error -> error.isNotBlank() }?.let { error ->
            Text(error, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
        }
    }
}

@Composable
private fun TimingDetail(label: String, value: String) {
    Text(
        text = "$label: $value",
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

private fun timingHeroTitle(state: MainUiState, requestCount: Int): String {
    return when {
        state.connectionRoutesError != null -> "Timing measurement unavailable"
        state.timingMeasurementResetting -> "Resetting timing measurement"
        state.timingStoreIndexTestRunning -> "Running store-index test"
        requestCount == 0 && state.timingMeasurementStartedUnixMs != null -> "Ready to measure"
        requestCount == 0 -> "No current measurements"
        else -> "$requestCount request(s) captured"
    }
}

private fun timingHeroSubtitle(state: MainUiState, requestCount: Int): String {
    return when {
        state.connectionRoutesError != null -> state.connectionRoutesError
        state.timingMeasurementResetting -> "Clearing recorded requests and starting a new measurement window."
        state.timingStoreIndexTestRunning -> "Making a read-only store-index request through the app's cached Rust client."
        requestCount == 0 && state.timingMeasurementStartedUnixMs != null ->
            "Reset complete at ${formatTimestampMs(state.timingMeasurementStartedUnixMs)}. Run the test or perform the action you want to inspect."
        requestCount == 0 ->
            "Reset first when you want to isolate the timing of one action, then run that action."
        else -> "These requests and session counters belong to the current measurement window."
    }
}

private fun relayPairingSummary(averageRelayPairingUs: Double?): String {
    return averageRelayPairingUs?.let { ", relay pairing ${formatDurationUs(it)}" }.orEmpty()
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
    return "$sign%.1f +/- %.1f ms".format(
        abs(offset).toDouble() / 1_000.0,
        uncertainty.toDouble() / 1_000.0,
    )
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
