package io.ironmesh.android.data

data class FolderSyncServiceStatus(
    val serviceState: String = "stopped",
    val serviceMessage: String = "Continuous sync is stopped",
    val profiles: List<FolderSyncProfileStatus> = emptyList(),
    val updatedUnixMs: Long = 0L,
    val profileCount: Long = 0L,
    val activeProfileCount: Long = 0L,
    val syncingProfileCount: Long = 0L,
    val errorProfileCount: Long = 0L,
    val startingProfileCount: Long = 0L,
    val runningProfileCount: Long = 0L,
    val currentActivity: String = "",
    val activeSummary: String = "",
    val lastSuccessUnixMs: Long? = null,
)

const val GLOBAL_SYNC_STATE_NOT_CONFIGURED = "not-configured"
const val GLOBAL_SYNC_STATE_PAUSED = "paused"
const val GLOBAL_SYNC_STATE_WAITING = "waiting"
const val GLOBAL_SYNC_STATE_STARTING = "starting"
const val GLOBAL_SYNC_STATE_SYNCING = "syncing"
const val GLOBAL_SYNC_STATE_HEALTHY = "healthy"
const val GLOBAL_SYNC_STATE_ERROR = "error"

/**
 * App-wide sync state derived from configured profiles, network eligibility, and
 * both continuous and recovery one-shot execution. It deliberately does not
 * include transport health; callers display [AppConnectionStatus] separately.
 */
data class GlobalFolderSyncStatus(
    val state: String = GLOBAL_SYNC_STATE_NOT_CONFIGURED,
    val message: String = "No sync profiles are configured",
    val updatedUnixMs: Long = 0L,
    val configuredProfileCount: Long = 0L,
    val enabledProfileCount: Long = 0L,
    val activeProfileCount: Long = 0L,
    val syncingProfileCount: Long = 0L,
    val errorProfileCount: Long = 0L,
    val waitingProfileCount: Long = 0L,
    val lastSuccessUnixMs: Long? = null,
)

fun mergeGlobalFolderSyncStatus(
    configs: List<FolderSyncConfig>,
    runtimeStatus: FolderSyncServiceStatus,
    blockedProfileCount: Int = 0,
    blockedReason: String? = null,
    enrollmentReady: Boolean = true,
    oneShotRunning: Boolean = false,
    oneShotProfileLabel: String? = null,
    nowUnixMs: Long = System.currentTimeMillis(),
): GlobalFolderSyncStatus {
    val configuredCount = configs.size.toLong()
    val enabledCount = configs.count { profile -> profile.enabled }.toLong()
    val normalizedBlockedCount = blockedProfileCount
        .coerceIn(0, enabledCount.coerceAtMost(Int.MAX_VALUE.toLong()).toInt())
        .toLong()
    val common = GlobalFolderSyncStatus(
        updatedUnixMs = maxOf(nowUnixMs, runtimeStatus.updatedUnixMs),
        configuredProfileCount = configuredCount,
        enabledProfileCount = enabledCount,
        activeProfileCount = runtimeStatus.activeProfileCount,
        syncingProfileCount = runtimeStatus.syncingProfileCount,
        errorProfileCount = runtimeStatus.errorProfileCount,
        waitingProfileCount = normalizedBlockedCount,
        lastSuccessUnixMs = runtimeStatus.lastSuccessUnixMs,
    )

    return when {
        configuredCount == 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_NOT_CONFIGURED,
            message = "No sync profiles are configured",
        )
        enabledCount == 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_PAUSED,
            message = "All sync profiles are paused",
        )
        !enrollmentReady -> common.copy(
            state = GLOBAL_SYNC_STATE_WAITING,
            message = "Enroll this device before sync can run",
            waitingProfileCount = enabledCount,
        )
        runtimeStatus.errorProfileCount > 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_ERROR,
            message = runtimeStatus.currentActivity
                .takeIf { activity -> activity.isNotBlank() }
                ?: runtimeStatus.serviceMessage.takeIf { message -> message.isNotBlank() }
                ?: "One or more sync profiles failed",
        )
        oneShotRunning -> common.copy(
            state = GLOBAL_SYNC_STATE_SYNCING,
            message = oneShotProfileLabel
                ?.takeIf { label -> label.isNotBlank() }
                ?.let { label -> "Running scheduled reconciliation for $label" }
                ?: "Running scheduled folder reconciliation",
        )
        runtimeStatus.syncingProfileCount > 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_SYNCING,
            message = runtimeStatus.currentActivity
                .takeIf { activity -> activity.isNotBlank() }
                ?: runtimeStatus.serviceMessage,
        )
        runtimeStatus.startingProfileCount > 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_STARTING,
            message = runtimeStatus.currentActivity
                .takeIf { activity -> activity.isNotBlank() }
                ?: runtimeStatus.serviceMessage,
        )
        normalizedBlockedCount > 0L -> common.copy(
            state = GLOBAL_SYNC_STATE_WAITING,
            message = buildString {
                append(normalizedBlockedCount)
                append(if (normalizedBlockedCount == 1L) " sync profile is waiting" else " sync profiles are waiting")
                blockedReason?.takeIf { reason -> reason.isNotBlank() }?.let { reason ->
                    append(": ")
                    append(reason)
                }
            },
        )
        runtimeStatus.activeProfileCount >= enabledCount -> common.copy(
            state = GLOBAL_SYNC_STATE_HEALTHY,
            message = if (enabledCount == 1L) {
                "Watching 1 sync profile for changes"
            } else {
                "Watching $enabledCount sync profiles for changes"
            },
        )
        else -> common.copy(
            state = GLOBAL_SYNC_STATE_STARTING,
            message = "Starting ${enabledCount - runtimeStatus.activeProfileCount} sync profile(s)",
        )
    }
}

data class FolderSyncProfileStatus(
    val profileId: String = "",
    val label: String = "",
    val state: String = "stopped",
    val message: String = "",
    val updatedUnixMs: Long = 0L,
    val phase: String = "",
    val activity: String = "",
    val scopeLabel: String = "<root>",
    val rootDir: String = "",
    val localTreeUri: String? = null,
    val connectionTarget: String? = null,
    val storageMode: String = "",
    val watchMode: String = "",
    val runMode: String = "",
    val lastSuccessUnixMs: Long? = null,
    val lastError: String? = null,
    val connectionDiagnostics: FolderSyncProfileConnectionDiagnostics? = null,
    val metrics: FolderSyncRuntimeMetrics = FolderSyncRuntimeMetrics(),
)

data class FolderSyncProfileConnectionDiagnostics(
    val endpoints: List<FolderSyncConnectionEndpointStatus> = emptyList(),
    val lastSuccessUnixMs: Long? = null,
)

data class FolderSyncConnectionEndpointStatus(
    val pathKind: String = "",
    val locator: String = "",
    val requestBaseUrl: String = "",
    val active: Boolean = false,
    val consecutiveFailures: Long = 0L,
    val totalFailures: Long = 0L,
    val totalSuccesses: Long = 0L,
    val lastAttemptUnixMs: Long? = null,
    val lastSuccessUnixMs: Long? = null,
    val lastFailureUnixMs: Long? = null,
    val lastError: String? = null,
    val recentAttempts: List<FolderSyncConnectionAttemptStatus> = emptyList(),
)

data class FolderSyncConnectionAttemptStatus(
    val startedUnixMs: Long = 0L,
    val finishedUnixMs: Long? = null,
    val method: String = "",
    val url: String = "",
    val timeoutMs: Long? = null,
    val outcome: String = "",
    val error: String? = null,
)

data class FolderSyncRuntimeMetrics(
    val localEntryCount: Long = 0L,
    val localFileCount: Long = 0L,
    val localDirectoryCount: Long = 0L,
    val remoteEntryCount: Long = 0L,
    val remoteFileCount: Long = 0L,
    val remoteDirectoryCount: Long = 0L,
    val changedPathCount: Long = 0L,
    val uploadedFileCount: Long = 0L,
    val downloadedFileCount: Long = 0L,
    val deletedRemoteFileCount: Long = 0L,
    val removedLocalPathCount: Long = 0L,
    val ensuredDirectoryCount: Long = 0L,
    val preservedLocalFileCount: Long = 0L,
    val startupConflictCount: Long = 0L,
)
