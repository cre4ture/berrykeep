package io.ironmesh.android.work

import kotlinx.coroutines.CompletableDeferred

data class FolderSyncExecutionSnapshot(
    val continuousRequested: Boolean = false,
    val continuousServiceActive: Boolean = false,
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
    private var continuousStartRequestCount = 0
    private var continuousServiceActive = false
    private var oneShotCompletion: CompletableDeferred<Unit>? = null
    private var oneShotProfileLabel: String? = null

    fun requestContinuousStart() {
        synchronized(lock) {
            continuousStartRequestCount += 1
        }
    }

    fun markContinuousServiceActive() {
        synchronized(lock) {
            continuousServiceActive = true
            continuousStartRequestCount = 0
        }
    }

    fun cancelContinuousStartRequest() {
        synchronized(lock) {
            continuousStartRequestCount = (continuousStartRequestCount - 1).coerceAtLeast(0)
        }
    }

    fun cancelAllContinuousStartRequests() {
        synchronized(lock) {
            continuousStartRequestCount = 0
        }
    }

    fun releaseContinuousService() {
        synchronized(lock) {
            continuousServiceActive = false
            continuousStartRequestCount = 0
        }
    }

    fun tryBeginOneShot(nativeContinuousActive: Boolean): Boolean {
        synchronized(lock) {
            if (
                nativeContinuousActive ||
                continuousStartRequestCount > 0 ||
                continuousServiceActive ||
                oneShotCompletion != null
            ) {
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
                continuousRequested = continuousStartRequestCount > 0 || continuousServiceActive,
                continuousServiceActive = continuousServiceActive,
                oneShotRunning = oneShotCompletion != null,
                oneShotProfileLabel = oneShotProfileLabel,
            )
        }
    }

    internal fun resetForTest() {
        val completion = synchronized(lock) {
            val activeCompletion = oneShotCompletion
            continuousStartRequestCount = 0
            continuousServiceActive = false
            oneShotCompletion = null
            oneShotProfileLabel = null
            activeCompletion
        }
        completion?.complete(Unit)
    }
}
