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
    private var continuousStartRequested = false
    private var continuousServiceActive = false
    private var oneShotCompletion: CompletableDeferred<Unit>? = null
    private var oneShotProfileLabel: String? = null

    fun requestContinuousStart() {
        synchronized(lock) {
            continuousStartRequested = true
        }
    }

    fun markContinuousServiceActive() {
        synchronized(lock) {
            continuousServiceActive = true
            continuousStartRequested = false
        }
    }

    fun cancelContinuousStartRequest() {
        synchronized(lock) {
            continuousStartRequested = false
        }
    }

    fun releaseContinuousService() {
        synchronized(lock) {
            continuousServiceActive = false
            continuousStartRequested = false
        }
    }

    fun tryBeginOneShot(nativeContinuousActive: Boolean): Boolean {
        synchronized(lock) {
            if (
                nativeContinuousActive ||
                continuousStartRequested ||
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
                continuousRequested = continuousStartRequested || continuousServiceActive,
                continuousServiceActive = continuousServiceActive,
                oneShotRunning = oneShotCompletion != null,
                oneShotProfileLabel = oneShotProfileLabel,
            )
        }
    }

    internal fun resetForTest() {
        val completion = synchronized(lock) {
            val activeCompletion = oneShotCompletion
            continuousStartRequested = false
            continuousServiceActive = false
            oneShotCompletion = null
            oneShotProfileLabel = null
            activeCompletion
        }
        completion?.complete(Unit)
    }
}
