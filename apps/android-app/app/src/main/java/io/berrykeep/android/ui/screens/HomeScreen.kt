package io.berrykeep.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import io.berrykeep.android.R
import io.berrykeep.android.data.GLOBAL_SYNC_STATE_ERROR
import io.berrykeep.android.data.GLOBAL_SYNC_STATE_HEALTHY
import io.berrykeep.android.data.GLOBAL_SYNC_STATE_WAITING
import io.berrykeep.android.data.TaskQueueEntry
import io.berrykeep.android.ui.HomeUiState
import io.berrykeep.android.ui.MainSection
import io.berrykeep.android.ui.components.EmptyStateCard
import io.berrykeep.android.ui.components.HeroTone
import io.berrykeep.android.ui.components.MetricPill
import io.berrykeep.android.ui.components.SectionCard
import io.berrykeep.android.ui.components.StatusHeroCard

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun HomeScreen(
    state: HomeUiState,
    onRunSyncNow: () -> Unit,
    onRetryConnection: () -> Unit,
    onOpenWebConsole: () -> Unit,
    onOpenSync: () -> Unit,
    onSelectSection: (MainSection) -> Unit,
) {
    val status = state.folderSyncStatus
    val globalSyncStatus = state.globalFolderSyncStatus
    val connectionStatus = state.appConnectionStatus
    val connectionHealthNow = rememberConnectionHealthNow(connectionStatus)
    val hasProfiles = state.syncProfileCount > 0
    val connectionTone = when {
        !isAppConnectionHealthy(connectionStatus, connectionHealthNow) -> HeroTone.Warning
        else -> HeroTone.Good
    }
    val syncTone = when (globalSyncStatus.state) {
        GLOBAL_SYNC_STATE_ERROR -> HeroTone.Error
        GLOBAL_SYNC_STATE_WAITING -> HeroTone.Warning
        GLOBAL_SYNC_STATE_HEALTHY -> HeroTone.Good
        else -> HeroTone.Neutral
    }
    val heroTitle = appConnectionHeadline(connectionStatus, connectionHealthNow)
    val heroBody = appConnectionSummary(connectionStatus)

    androidx.compose.foundation.layout.Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        StatusHeroCard(
            title = heroTitle,
            subtitle = heroBody,
            tone = connectionTone,
        ) {
            FlowRow(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                if (
                    state.isEnrolled &&
                    shouldShowRetryConnectionAction(connectionStatus, connectionHealthNow)
                ) {
                    OutlinedButton(onClick = onRetryConnection) {
                        Text(stringResource(R.string.retry_connection))
                    }
                }
                OutlinedButton(onClick = onOpenWebConsole) {
                    Text(stringResource(R.string.open_web_console))
                }
            }
        }

        StatusHeroCard(
            title = syncOverviewHeadline(globalSyncStatus),
            subtitle = syncOverviewSummary(globalSyncStatus),
            tone = syncTone,
        ) {
            FlowRow(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Button(onClick = onRunSyncNow) {
                    Text(stringResource(R.string.sync_now))
                }
                OutlinedButton(onClick = onOpenSync) {
                    Text(stringResource(R.string.open_sync))
                }
            }
        }

        FlowRow(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            MetricPill(
                label = stringResource(R.string.metric_profiles),
                value = state.syncProfileCount.toString(),
            )
            MetricPill(
                label = stringResource(R.string.metric_last_success),
                value = globalSyncStatus.lastSuccessUnixMs?.let(::formatTimestamp) ?: "None",
            )
            MetricPill(
                label = stringResource(R.string.metric_uploads),
                value = totalUploadedCount(state).toString(),
            )
            MetricPill(
                label = stringResource(R.string.metric_errors),
                value = status.errorProfileCount.toString(),
            )
        }

        SectionCard(
            title = "Task queues",
            supportingText = "Best-effort client and server work; server counts refresh about every 30 seconds.",
        ) {
            Text("This client", style = MaterialTheme.typography.titleSmall)
            state.clientTaskQueues.forEach { queue ->
                TaskQueueRow(queue = queue)
            }
            Text("Server cluster", style = MaterialTheme.typography.titleSmall)
            state.clusterTaskQueues?.nodes
                ?.flatMap { node ->
                    node.queues
                        .filter { queue -> queue.pending > 0 || queue.active > 0 }
                        .map { queue -> queue.copy(label = "${queue.label} · ${shortNodeId(node.nodeId)}") }
                }
                ?.takeIf { queues -> queues.isNotEmpty() }
                ?.forEach { queue -> TaskQueueRow(queue = queue) }
                ?: Text(
                    text = if (state.clusterTaskQueues == null) {
                        "Waiting for a server snapshot."
                    } else {
                        "No pending or active server work observed."
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            state.clusterTaskQueues?.unavailableNodes?.forEach { node ->
                TaskQueueRow(
                    queue = TaskQueueEntry(
                        label = "Server node ${shortNodeId(node.nodeId)}",
                        state = "unavailable",
                        detail = node.error,
                    ),
                )
            }
            state.clusterTaskQueuesError?.let { error ->
                Text(
                    text = error,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
        }

        if (!hasProfiles) {
            SectionCard(
                title = stringResource(R.string.home_next_step),
                supportingText = stringResource(R.string.home_next_step_body),
            ) {
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Button(onClick = { onSelectSection(MainSection.SYNC) }) {
                        Text(stringResource(R.string.create_sync))
                    }
                    OutlinedButton(onClick = { onSelectSection(MainSection.LIBRARY) }) {
                        Text(stringResource(R.string.open_library))
                    }
                }
            }
        }

        if (status.profiles.isEmpty()) {
            EmptyStateCard(
                title = stringResource(R.string.home_empty_activity_title),
                body = stringResource(R.string.home_empty_activity_body),
                actionLabel = stringResource(R.string.open_profile_creator),
                onAction = onOpenSync,
            )
        } else {
            SectionCard(title = stringResource(R.string.recent_activity)) {
                status.profiles
                    .sortedByDescending { it.updatedUnixMs }
                    .take(3)
                    .forEach { profile ->
                        androidx.compose.foundation.layout.Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(bottom = 10.dp),
                            verticalArrangement = Arrangement.spacedBy(4.dp),
                        ) {
                            Text(profile.label, style = MaterialTheme.typography.titleSmall)
                            Text(
                                text = profile.message.ifBlank { displayStatusToken(profile.state) },
                                style = MaterialTheme.typography.bodyMedium,
                            )
                            val detail = listOfNotNull(
                                profile.lastSuccessUnixMs?.let { "Last success ${formatTimestamp(it)}" },
                                profile.activity.takeIf { it.isNotBlank() }?.let(::displayStatusToken),
                                profile.lastError?.takeIf { it.isNotBlank() },
                            ).joinToString(" | ")
                            if (detail.isNotBlank()) {
                                Text(
                                    text = detail,
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
            }
        }
    }
}

@Composable
private fun TaskQueueRow(queue: TaskQueueEntry) {
    androidx.compose.foundation.layout.Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
            Text(queue.label, style = MaterialTheme.typography.bodyMedium)
            Text(
                "${queue.state} · ${queue.pending} pending · ${queue.active} active",
                style = MaterialTheme.typography.labelSmall,
                color = if (queue.state == "backlogged" || queue.state == "unavailable") {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
        queue.detail?.takeIf { it.isNotBlank() }?.let { detail ->
            Text(
                detail,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

private fun shortNodeId(nodeId: String): String = if (nodeId.length > 12) nodeId.take(8) else nodeId

private fun totalUploadedCount(state: HomeUiState): Long {
    return state.folderSyncStatus.profiles.sumOf { it.metrics.uploadedFileCount }
}
