package io.ironmesh.android.work

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * Serializes and conflates connectivity callbacks before they reach native route discovery.
 *
 * Android commonly emits `onAvailable` and several capability changes as one logical network
 * transition. A single worker preserves a trailing hint while native work is active, so callback
 * bursts cannot create concurrent client builds or refreshes.
 */
internal class ConflatedNetworkChangeNotifier(
    scope: CoroutineScope,
    private val debounceMs: Long = DEFAULT_NETWORK_CHANGE_DEBOUNCE_MS,
    private val onNetworkChange: suspend (reason: String) -> Unit,
    private val onFailure: (reason: String, error: Throwable) -> Unit,
) {
    private val hints = Channel<String>(capacity = Channel.CONFLATED)
    private val worker: Job = scope.launch { processHints() }

    fun submit(reason: String) {
        hints.trySend(reason)
    }

    fun close() {
        hints.close()
        worker.cancel()
    }

    private suspend fun processHints() {
        for (firstReason in hints) {
            delay(debounceMs.coerceAtLeast(0L))
            var latestReason = firstReason
            while (true) {
                latestReason = hints.tryReceive().getOrNull() ?: break
            }

            try {
                onNetworkChange(latestReason)
            } catch (error: CancellationException) {
                throw error
            } catch (error: Throwable) {
                onFailure(latestReason, error)
            }
        }
    }

    private companion object {
        const val DEFAULT_NETWORK_CHANGE_DEBOUNCE_MS = 300L
    }
}
