package io.ironmesh.android.ui.screens

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
import io.ironmesh.android.R
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_ERROR
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_HEALTHY
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_WAITING
import io.ironmesh.android.ui.MainSection
import io.ironmesh.android.ui.MainUiState
import io.ironmesh.android.ui.components.EmptyStateCard
import io.ironmesh.android.ui.components.HeroTone
import io.ironmesh.android.ui.components.MetricPill
import io.ironmesh.android.ui.components.SectionCard
import io.ironmesh.android.ui.components.StatusHeroCard

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun HomeScreen(
    state: MainUiState,
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
    val hasProfiles = state.syncProfiles.isNotEmpty()
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
                    state.deviceAuthState.hasClientIdentity() &&
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
                value = state.syncProfiles.size.toString(),
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

private fun totalUploadedCount(state: MainUiState): Long {
    return state.folderSyncStatus.profiles.sumOf { it.metrics.uploadedFileCount }
}
