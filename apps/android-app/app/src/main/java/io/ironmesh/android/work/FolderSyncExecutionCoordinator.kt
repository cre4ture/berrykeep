package io.ironmesh.android.work

import kotlinx.coroutines.CompletableDeferred

private const val SYNC_RETRY_BASE_DELAY_MS = 2_000L
private const val SYNC_RETRY_MAX_DELAY_MS = 60_000L

internal fun nextFolderSyncRetryDelayMs(attempt: Int): Long {
    if (attempt <= 1) {
        return SYNC_RETRY_BASE_DELAY_MS
    }
    val exponent = (attempt - 1).coerceAtMost(5)
    val multiplier = 1L shl exponent
    return (SYNC_RETRY_BASE_DELAY_MS * multiplier).coerceAtMost(SYNC_RETRY_MAX_DELAY_MS)
}

data class FolderSyncExecutionSnapshot(
    val continuousRequested: Boolean = false,
    val oneShotRunning: Boolean = false,
    val oneShotProfileLabel: String? = null,
)

/**
 * Serializes the Android service and WorkManager entry points before either can
 * enter the native folder-sync runtime. Both entry points run in this process,
 * so claiming here closes the preflight-check race that previously allowed a
 * one-shot reconciliation and continuous reconciliation to start together.
 */
object FolderSyncExecutionCoordinator {
    private val lock = Any()
    private var continuousRequested = false
    private var oneShotCompletion: CompletableDeferred<Unit>? = null
    private var oneShotProfileLabel: String? = null

    fun requestContinuous() {
        synchronized(lock) {
            continuousRequested = true
        }
    }

    fun releaseContinuous() {
        synchronized(lock) {
            continuousRequested = false
        }
    }

    fun tryBeginOneShot(nativeContinuousActive: Boolean): Boolean {
        synchronized(lock) {
            if (nativeContinuousActive || continuousRequested || oneShotCompletion != null) {
                return false
            }
            oneShotCompletion = CompletableDeferred()
            oneShotProfileLabel = null
            return true
        }
    }

    fun updateOneShotProfile(label: String?) {
        synchronized(lock) {
            if (oneShotCompletion != null) {
                oneShotProfileLabel = label
            }
        }
    }

    fun finishOneShot() {
        val completion = synchronized(lock) {
            val activeCompletion = oneShotCompletion
            oneShotCompletion = null
            oneShotProfileLabel = null
            activeCompletion
        }
        completion?.complete(Unit)
    }

    suspend fun awaitOneShotCompletion() {
        val completion = synchronized(lock) { oneShotCompletion }
        completion?.await()
    }

    fun snapshot(): FolderSyncExecutionSnapshot {
        return synchronized(lock) {
            FolderSyncExecutionSnapshot(
                continuousRequested = continuousRequested,
                oneShotRunning = oneShotCompletion != null,
                oneShotProfileLabel = oneShotProfileLabel,
            )
        }
    }

    internal fun resetForTest() {
        val completion = synchronized(lock) {
            val activeCompletion = oneShotCompletion
            continuousRequested = false
            oneShotCompletion = null
            oneShotProfileLabel = null
            activeCompletion
        }
        completion?.complete(Unit)
    }
}
