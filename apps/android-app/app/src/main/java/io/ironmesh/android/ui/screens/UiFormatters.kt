package io.ironmesh.android.ui.screens

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import io.ironmesh.android.data.FolderSyncModificationRecord
import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.FolderSyncNetworkPolicy
import io.ironmesh.android.data.FolderSyncProfileStatus
import io.ironmesh.android.data.FolderSyncRuntimeMetrics
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_ERROR
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_HEALTHY
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_NOT_CONFIGURED
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_PAUSED
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_STARTING
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_SYNCING
import io.ironmesh.android.data.GLOBAL_SYNC_STATE_WAITING
import io.ironmesh.android.data.APP_CONNECTION_STATE_CONNECTED
import io.ironmesh.android.data.APP_CONNECTION_STATE_CONNECTING
import io.ironmesh.android.data.APP_CONNECTION_STATE_ERROR
import io.ironmesh.android.data.APP_CONNECTION_STATE_RECONNECTING
import io.ironmesh.android.data.APP_CONNECTION_STATE_RETRY_SCHEDULED
import io.ironmesh.android.data.APP_CONNECTION_STATE_WAITING_FOR_ENROLLMENT
import io.ironmesh.android.data.APP_CONNECTION_STATE_WAITING_FOR_NETWORK
import io.ironmesh.android.data.APP_CONNECTION_HEALTH_MAX_AGE_MS
import io.ironmesh.android.data.formatAllowedWifiSsidsInput
import io.ironmesh.android.data.isConnected
import io.ironmesh.android.data.isRetryPending
import io.ironmesh.android.ui.FolderSyncActivityFilter
import kotlinx.coroutines.delay
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

fun displayStatusToken(value: String): String {
    if (value.isBlank()) {
        return "Unknown"
    }
    return value
        .replace('-', ' ')
        .replace('_', ' ')
        .split(' ')
        .filter { it.isNotBlank() }
        .joinToString(" ") { part ->
            part.lowercase().replaceFirstChar { ch ->
                if (ch.isLowerCase()) ch.titlecase() else ch.toString()
            }
        }
}

fun appConnectionHeadline(
    connectionStatus: AppConnectionStatus,
    nowUnixMs: Long = System.currentTimeMillis(),
): String {
    return when (connectionStatus.state) {
        APP_CONNECTION_STATE_CONNECTING -> "Connecting app"
        APP_CONNECTION_STATE_RECONNECTING -> "Reconnecting app"
        APP_CONNECTION_STATE_RETRY_SCHEDULED -> "Retry scheduled"
        APP_CONNECTION_STATE_WAITING_FOR_NETWORK -> "Waiting for network"
        APP_CONNECTION_STATE_WAITING_FOR_ENROLLMENT -> "Enrollment needed"
        APP_CONNECTION_STATE_ERROR -> "Connection failed"
        APP_CONNECTION_STATE_CONNECTED -> {
            if (isAppConnectionHealthy(connectionStatus, nowUnixMs)) {
                "Connection is healthy"
            } else {
                "Connection status is stale"
            }
        }
        else -> "Connection is idle"
    }
}

@Composable
fun rememberConnectionHealthNow(connectionStatus: AppConnectionStatus): Long {
    var expiryRefreshFloorUnixMs by remember { mutableLongStateOf(0L) }
    // Read the wall clock during composition so a newly published success is never compared with
    // the previous composition's time. The retained floor triggers the expiry recomposition and
    // keeps health evaluation monotonic if the wall clock moves backwards.
    val nowUnixMs = currentConnectionHealthNow(expiryRefreshFloorUnixMs)
    LaunchedEffect(
        connectionStatus.state,
        connectionStatus.lastSuccessfulConnectionUnixMs,
    ) {
        if (!connectionStatus.isConnected(nowUnixMs)) {
            return@LaunchedEffect
        }

        val expiresAtUnixMs = requireNotNull(connectionStatus.lastSuccessfulConnectionUnixMs) +
            APP_CONNECTION_HEALTH_MAX_AGE_MS
        delay((expiresAtUnixMs - nowUnixMs + 1L).coerceAtLeast(0L))
        expiryRefreshFloorUnixMs = System.currentTimeMillis()
    }
    return nowUnixMs
}

internal fun currentConnectionHealthNow(
    refreshFloorUnixMs: Long,
    currentTimeMillis: () -> Long = System::currentTimeMillis,
): Long = maxOf(refreshFloorUnixMs, currentTimeMillis())

fun appConnectionStatusBadge(
    connectionStatus: AppConnectionStatus,
    nowUnixMs: Long = System.currentTimeMillis(),
): String {
    return when {
        isAppConnectionHealthy(connectionStatus, nowUnixMs) -> "Healthy"
        connectionStatus.state == APP_CONNECTION_STATE_CONNECTED -> "Stale"
        else -> displayStatusToken(connectionStatus.state)
    }
}

fun appConnectionSummary(
    connectionStatus: AppConnectionStatus,
): String {
    val parts = mutableListOf<String>()
    if (connectionStatus.state == APP_CONNECTION_STATE_CONNECTED) {
        parts += "The server is reachable"
    } else {
        connectionStatus.message
            .trim()
            .takeIf { it.isNotBlank() }
            ?.let(parts::add)
    }
    if (connectionStatus.retryAttemptCount > 0L) {
        parts += "Retry ${connectionStatus.retryAttemptCount}"
    }
    connectionStatus.nextRetryUnixMs?.let { retryAt ->
        parts += "Next retry ${formatTimestamp(retryAt)}"
    }
    connectionStatus.lastSuccessfulConnectionUnixMs?.let { lastSuccess ->
        parts += "Last reachable ${formatTimestamp(lastSuccess)}"
    }
    if (parts.isEmpty()) {
        parts += "No app connection activity yet"
    }
    return parts.joinToString(" | ")
}

fun isAppConnectionHealthy(
    connectionStatus: AppConnectionStatus,
    nowUnixMs: Long = System.currentTimeMillis(),
): Boolean {
    return connectionStatus.isConnected(nowUnixMs)
}

fun syncOverviewHeadline(
    status: GlobalFolderSyncStatus,
): String {
    return when (status.state) {
        GLOBAL_SYNC_STATE_NOT_CONFIGURED -> "Set up your first sync profile"
        GLOBAL_SYNC_STATE_PAUSED -> "Sync is paused"
        GLOBAL_SYNC_STATE_WAITING -> "Sync is waiting"
        GLOBAL_SYNC_STATE_STARTING -> "Sync is starting"
        GLOBAL_SYNC_STATE_SYNCING -> "Sync in progress"
        GLOBAL_SYNC_STATE_HEALTHY -> "Sync is healthy"
        GLOBAL_SYNC_STATE_ERROR -> "Sync needs attention"
        else -> "Sync status is unknown"
    }
}

fun syncOverviewSummary(status: GlobalFolderSyncStatus): String {
    val parts = mutableListOf<String>()
    status.message
        .trim()
        .takeIf { it.isNotBlank() }
        ?.let(parts::add)
    status.lastSuccessUnixMs?.let { lastSuccess ->
        parts += "Last success ${formatTimestamp(lastSuccess)}"
    }
    return parts.joinToString(" | ").ifBlank { "No sync activity yet" }
}

fun shouldShowRetryConnectionAction(
    connectionStatus: AppConnectionStatus,
    nowUnixMs: Long = System.currentTimeMillis(),
): Boolean {
    return !isAppConnectionHealthy(connectionStatus, nowUnixMs) ||
        connectionStatus.isRetryPending()
}

fun formatDurationMillis(durationMs: Long): String {
    val totalSeconds = (durationMs / 1000L).coerceAtLeast(1L)
    return if (totalSeconds < 60L) {
        "${totalSeconds}s"
    } else {
        val minutes = totalSeconds / 60L
        val seconds = totalSeconds % 60L
        if (seconds == 0L) {
            "${minutes}m"
        } else {
            "${minutes}m ${seconds}s"
        }
    }
}

fun profileInventorySummary(status: FolderSyncProfileStatus): String {
    val metrics = status.metrics
    return buildString {
        append("Local ${metrics.localEntryCount} entries")
        append(" (${metrics.localFileCount} files, ${metrics.localDirectoryCount} folders)")
        append(" | Remote ${metrics.remoteEntryCount} entries")
        append(" (${metrics.remoteFileCount} files, ${metrics.remoteDirectoryCount} folders)")
    }
}

fun recentWorkSummary(metrics: FolderSyncRuntimeMetrics): String? {
    val parts = mutableListOf<String>()
    if (metrics.changedPathCount > 0L) {
        parts += "${metrics.changedPathCount} path(s)"
    }
    if (metrics.uploadedFileCount > 0L) {
        parts += "${metrics.uploadedFileCount} upload(s)"
    }
    if (metrics.downloadedFileCount > 0L) {
        parts += "${metrics.downloadedFileCount} download(s)"
    }
    if (metrics.deletedRemoteFileCount > 0L) {
        parts += "${metrics.deletedRemoteFileCount} remote delete(s)"
    }
    if (metrics.removedLocalPathCount > 0L) {
        parts += "${metrics.removedLocalPathCount} local removal(s)"
    }
    if (metrics.ensuredDirectoryCount > 0L) {
        parts += "${metrics.ensuredDirectoryCount} directory update(s)"
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString(", ")
}

fun startupDetailSummary(metrics: FolderSyncRuntimeMetrics): String? {
    val parts = mutableListOf<String>()
    if (metrics.preservedLocalFileCount > 0L) {
        parts += "${metrics.preservedLocalFileCount} preserved local file(s)"
    }
    if (metrics.startupConflictCount > 0L) {
        parts += "${metrics.startupConflictCount} startup conflict(s)"
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString(", ")
}

fun folderSyncAllowedTransportLabel(policy: FolderSyncNetworkPolicy): String {
    val normalizedPolicy = policy.normalized()
    val parts = mutableListOf<String>()
    if (normalizedPolicy.allowWifi) {
        parts += "Wi-Fi"
    }
    if (normalizedPolicy.allowCellular) {
        parts += "Mobile"
    }
    if (normalizedPolicy.allowOtherConnections) {
        parts += "Other"
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString("/") ?: "Blocked"
}

fun folderSyncNetworkPolicySummary(policy: FolderSyncNetworkPolicy): String {
    val normalizedPolicy = policy.normalized()
    val parts = mutableListOf<String>()
    parts += folderSyncAllowedTransportLabel(normalizedPolicy)
    if (normalizedPolicy.allowCellular) {
        parts += if (normalizedPolicy.allowRoaming) {
            "roaming allowed"
        } else {
            "no roaming"
        }
    }
    if (normalizedPolicy.allowWifi && normalizedPolicy.allowedWifiSsids.isNotEmpty()) {
        parts += "Wi-Fi names: ${formatAllowedWifiSsidsInput(normalizedPolicy)}"
    }
    return parts.joinToString(" | ")
}

fun folderSyncActivityFilterLabel(filter: FolderSyncActivityFilter): String {
    return when (filter) {
        FolderSyncActivityFilter.ALL -> "All"
        FolderSyncActivityFilter.UPLOADS -> "Uploads"
        FolderSyncActivityFilter.DOWNLOADS -> "Downloads"
        FolderSyncActivityFilter.DELETES -> "Deletes"
    }
}

fun folderSyncHistoryMatchesFilter(
    record: FolderSyncModificationRecord,
    filter: FolderSyncActivityFilter,
): Boolean {
    return when (filter) {
        FolderSyncActivityFilter.ALL -> true
        FolderSyncActivityFilter.UPLOADS -> record.operation == "upload"
        FolderSyncActivityFilter.DOWNLOADS -> record.operation == "download"
        FolderSyncActivityFilter.DELETES ->
            record.operation == "delete-local" || record.operation == "delete-remote"
    }
}

fun folderSyncOperationLabel(operation: String): String {
    return when (operation) {
        "upload" -> "Upload"
        "download" -> "Download"
        "delete-local" -> "Delete local"
        "delete-remote" -> "Delete remote"
        else -> displayStatusToken(operation)
    }
}

fun folderSyncHistorySecondaryText(record: FolderSyncModificationRecord): String? {
    val parts = mutableListOf<String>()
    record.sizeBytes?.let { sizeBytes ->
        parts += formatByteCount(sizeBytes)
    }
    if (record.phase.isNotBlank()) {
        parts += displayStatusToken(record.phase)
    }
    if (record.triggerSource.isNotBlank()) {
        parts += if (record.triggerSource == "conflict-resolution") {
            "Conflict resolution"
        } else {
            displayStatusToken(record.triggerSource)
        }
    }
    if (record.remoteKey.isNotBlank() && record.remoteKey != record.localRelativePath) {
        parts += record.remoteKey
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString(" | ")
}

fun formatByteCount(sizeBytes: Long): String {
    val kib = 1024.0
    val mib = kib * 1024.0
    return when {
        sizeBytes >= mib.toLong() -> String.format("%.1f MB", sizeBytes / mib)
        sizeBytes >= kib.toLong() -> String.format("%.1f KB", sizeBytes / kib)
        else -> "$sizeBytes B"
    }
}

fun formatTimestamp(value: Long): String {
    return DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")
        .withZone(ZoneId.systemDefault())
        .format(Instant.ofEpochMilli(value))
}
