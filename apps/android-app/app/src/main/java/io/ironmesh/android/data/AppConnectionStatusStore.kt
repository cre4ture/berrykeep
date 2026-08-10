package io.ironmesh.android.data

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

internal interface AppConnectionStatusPersistence {
    suspend fun load(): AppConnectionStatus

    suspend fun save(status: AppConnectionStatus)
}

/**
 * Process-wide source of truth for app connection status.
 *
 * Native callbacks can arrive concurrently and on arbitrary threads. Commands are serialized by
 * one coroutine so every diagnostic update observes the result of the previous update. Persistence
 * is deliberately conflated and rate-limited: Compose receives every meaningful in-memory change,
 * while bursts of route diagnostics produce at most one durable snapshot per interval.
 */
internal class AppConnectionStatusStore(
    scope: CoroutineScope,
    private val persistence: AppConnectionStatusPersistence,
    private val decodeUpdate: (String) -> AppConnectionDiagnosticsUpdate?,
    private val mergeUpdate: (
        current: AppConnectionStatus,
        update: AppConnectionDiagnosticsUpdate,
    ) -> AppConnectionStatus,
    private val onStatusChanged: (
        previous: AppConnectionStatus,
        next: AppConnectionStatus,
        update: AppConnectionDiagnosticsUpdate,
    ) -> Unit,
    private val onFailure: (operation: String, error: Throwable) -> Unit,
    private val persistenceIntervalMs: Long = DEFAULT_PERSISTENCE_INTERVAL_MS,
) {
    private sealed interface Command {
        data object Load : Command

        data object Flush : Command

        data class ApplyDiagnostics(val json: String) : Command
    }

    private data class PersistenceSnapshot(
        val sequence: Long,
        val status: AppConnectionStatus,
    )

    private val commands = Channel<Command>(capacity = Channel.UNLIMITED)
    private val persistenceRequests = Channel<PersistenceSnapshot>(capacity = Channel.CONFLATED)
    private val persistenceMutex = Mutex()
    private val mutableStatus = MutableStateFlow<AppConnectionStatus?>(null)
    private var lastPersistedSequence = 0L

    val status: StateFlow<AppConnectionStatus?> = mutableStatus.asStateFlow()

    init {
        scope.launch { processCommands() }
        scope.launch { persistSnapshots() }
    }

    fun load() {
        check(commands.trySend(Command.Load).isSuccess) {
            "App connection status store is unavailable"
        }
    }

    fun submitDiagnosticsJson(json: String) {
        check(commands.trySend(Command.ApplyDiagnostics(json)).isSuccess) {
            "App connection status store is unavailable"
        }
    }

    fun flush() {
        check(commands.trySend(Command.Flush).isSuccess) {
            "App connection status store is unavailable"
        }
    }

    private suspend fun processCommands() {
        var current: AppConnectionStatus? = null
        var currentSequence = 0L
        for (command in commands) {
            if (current == null) {
                current = runCatching { persistence.load() }
                    .getOrElse { error ->
                        onFailure("load", error)
                        AppConnectionStatus()
                    }
                mutableStatus.value = current
            }

            when (command) {
                Command.Load -> Unit
                Command.Flush -> {
                    persistSnapshot(
                        snapshot = PersistenceSnapshot(
                            sequence = currentSequence,
                            status = requireNotNull(current),
                        ),
                        operation = "flush",
                    )
                }
                is Command.ApplyDiagnostics -> {
                    val update = runCatching { decodeUpdate(command.json) }
                        .getOrElse { error ->
                            onFailure("decode", error)
                            null
                        } ?: continue
                    val previous = requireNotNull(current)
                    val mergeResult = runCatching { mergeUpdate(previous, update) }
                    val mergeError = mergeResult.exceptionOrNull()
                    if (mergeError != null) {
                        onFailure("merge", mergeError)
                        continue
                    }
                    val next = mergeResult.getOrThrow()
                    if (next == previous) {
                        continue
                    }

                    current = next
                    currentSequence += 1L
                    mutableStatus.value = next
                    runCatching { onStatusChanged(previous, next, update) }
                        .onFailure { error -> onFailure("observe", error) }
                    persistenceRequests.trySend(
                        PersistenceSnapshot(sequence = currentSequence, status = next),
                    )
                }
            }
        }
    }

    private suspend fun persistSnapshots() {
        while (kotlin.coroutines.coroutineContext.isActive) {
            var latest = persistenceRequests.receive()
            kotlinx.coroutines.delay(persistenceIntervalMs.coerceAtLeast(0L))
            while (true) {
                latest = persistenceRequests.tryReceive().getOrNull() ?: break
            }
            persistSnapshot(latest, operation = "persist")
        }
    }

    private suspend fun persistSnapshot(
        snapshot: PersistenceSnapshot,
        operation: String,
    ) {
        persistenceMutex.withLock {
            if (snapshot.sequence <= lastPersistedSequence) {
                return
            }
            runCatching { persistence.save(snapshot.status) }
                .onSuccess { lastPersistedSequence = snapshot.sequence }
                .onFailure { error -> onFailure(operation, error) }
        }
    }

    private companion object {
        const val DEFAULT_PERSISTENCE_INTERVAL_MS = 1_000L
    }
}
