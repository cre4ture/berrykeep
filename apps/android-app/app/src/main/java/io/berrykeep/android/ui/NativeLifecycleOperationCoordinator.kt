package io.berrykeep.android.ui

import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * Serializes native start/stop calls and lets superseded background work exit before it
 * reaches JNI. Callers must keep every operation that changes the same native lifecycle
 * behind this coordinator.
 */
internal class NativeLifecycleOperationCoordinator {
    private val operationMutex = Mutex()
    private val latestOperation = LatestOperationGate()

    fun nextGeneration(): Long = latestOperation.next()

    /** Invalidates an in-flight result before a forced lifecycle transition. */
    fun invalidatePendingOperations() {
        latestOperation.next()
    }

    fun isCurrent(generation: Long): Boolean = latestOperation.isCurrent(generation)

    suspend fun <T> run(operation: suspend () -> T): T = operationMutex.withLock {
        operation()
    }

    suspend fun runIfCurrent(
        generation: Long,
        operation: suspend () -> Unit,
    ): Boolean = operationMutex.withLock {
        if (!latestOperation.isCurrent(generation)) {
            return@withLock false
        }
        operation()
        true
    }
}
