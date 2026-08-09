package io.ironmesh.android.data

const val APP_CONNECTION_STATE_STOPPED = "stopped"
const val APP_CONNECTION_STATE_CONNECTING = "connecting"
const val APP_CONNECTION_STATE_CONNECTED = "connected"
const val APP_CONNECTION_STATE_RECONNECTING = "reconnecting"
const val APP_CONNECTION_STATE_WAITING_FOR_NETWORK = "waiting-for-network"
const val APP_CONNECTION_STATE_WAITING_FOR_ENROLLMENT = "waiting-for-enrollment"
const val APP_CONNECTION_STATE_RETRY_SCHEDULED = "retry-scheduled"
const val APP_CONNECTION_STATE_ERROR = "error"

internal const val APP_CONNECTION_HEALTH_MAX_AGE_MS = 60 * 60 * 1_000L
internal const val APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING = "user_facing"
internal const val APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE =
    "background_maintenance"

// App-wide transport reachability shared by sync, gallery, and other requests.
// HTTP/application failures belong to their owning feature state, not this status.
data class AppConnectionStatus(
    val state: String = APP_CONNECTION_STATE_STOPPED,
    val message: String = "No app connection activity yet",
    val updatedUnixMs: Long = 0L,
    val retryAttemptCount: Long = 0L,
    val nextRetryUnixMs: Long? = null,
    val lastSuccessfulConnectionUnixMs: Long? = null,
    val lastSuccessfulConnectionUrl: String? = null,
    val lastSuccessfulFunctionalRequestUnixMs: Long? = null,
    val lastSuccessfulFunctionalRequestUrl: String? = null,
    val failedAttempts: List<AppFailedConnectionAttempt> = emptyList(),
)

data class AppFailedConnectionAttempt(
    val sourceLabel: String? = null,
    val impact: String = APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING,
    val operationTerminal: Boolean = false,
    val endpointLocator: String = "",
    val pathKind: String = "",
    val startedUnixMs: Long = 0L,
    val finishedUnixMs: Long? = null,
    val method: String = "",
    val url: String = "",
    val timeoutMs: Long? = null,
    val error: String? = null,
)

data class AppConnectionDiagnosticsUpdate(
    val sourceLabel: String? = null,
    val impact: String = APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING,
    val lastSuccessfulConnectionUnixMs: Long? = null,
    val lastSuccessfulConnectionUrl: String? = null,
    val lastSuccessfulFunctionalRequestUnixMs: Long? = null,
    val lastSuccessfulFunctionalRequestUrl: String? = null,
    val failedAttempts: List<AppFailedConnectionAttempt> = emptyList(),
)

internal fun String.affectsAppConnectionStatus(): Boolean {
    return this != APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE
}

internal fun AppFailedConnectionAttempt.affectsAppConnectionStatus(): Boolean {
    return operationTerminal && impact.affectsAppConnectionStatus()
}

fun AppConnectionStatus.isRetryPending(): Boolean {
    return state == APP_CONNECTION_STATE_RETRY_SCHEDULED || nextRetryUnixMs != null
}

fun AppConnectionStatus.isConnected(nowUnixMs: Long = System.currentTimeMillis()): Boolean {
    val lastSuccessUnixMs = lastSuccessfulConnectionUnixMs ?: return false
    return state == APP_CONNECTION_STATE_CONNECTED &&
        lastSuccessUnixMs in (nowUnixMs - APP_CONNECTION_HEALTH_MAX_AGE_MS)..nowUnixMs
}
