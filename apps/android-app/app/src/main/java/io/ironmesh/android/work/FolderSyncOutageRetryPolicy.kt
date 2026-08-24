package io.ironmesh.android.work

import android.content.Context

/**
 * Persistent retry state for endpoint outages.
 *
 * This is deliberately separate from Android's connectivity constraints: a missing or
 * disallowed network must not be treated as a failing endpoint. The state only records failed
 * attempts that reached the sync runtime and is shared by the foreground service and the
 * periodic WorkManager fallback.
 */
internal data class FolderSyncOutageRetryState(
    val failureCount: Int = 0,
    val nextRetryAtEpochMs: Long = 0L,
)

internal enum class FolderSyncRetryTrigger {
    SERVICE_START,
    APP_FOREGROUNDED,
    NETWORK_AVAILABLE,
    LOCAL_FOLDER_CHANGED,
    BACKOFF_TIMER,
    PERIODIC_WORK,
    MANUAL_SYNC,
    CONFIGURATION_CHANGED,
}

internal object FolderSyncOutageRetryPolicy {
    const val BASE_DELAY_MS = 30_000L
    const val MAX_DELAY_MS = 30 * 60_000L
    private const val MAX_FAILURE_COUNT = 7
    private const val MAX_JITTER_PERMILLE = 100

    /**
     * 30 seconds, 1, 2, 4, 8, 16 and then 30 minutes. Jitter is additive only, so a retry is
     * never earlier than the documented delay and the maximum remains bounded at 30 minutes.
     */
    fun delayForFailure(failureCount: Int, jitterPermille: Int = 0): Long {
        val normalizedFailureCount = failureCount.coerceIn(1, MAX_FAILURE_COUNT)
        val exponent = normalizedFailureCount - 1
        val unjitteredDelay = (BASE_DELAY_MS * (1L shl exponent)).coerceAtMost(MAX_DELAY_MS)
        val normalizedJitter = jitterPermille.coerceIn(0, MAX_JITTER_PERMILLE)
        val jitteredDelay = unjitteredDelay +
            (unjitteredDelay * normalizedJitter / 1_000L)
        return jitteredDelay.coerceAtMost(MAX_DELAY_MS)
    }

    fun afterFailure(
        state: FolderSyncOutageRetryState,
        nowEpochMs: Long,
        jitterPermille: Int = 0,
    ): FolderSyncOutageRetryState {
        val failureCount = (state.failureCount + 1).coerceAtMost(MAX_FAILURE_COUNT)
        return FolderSyncOutageRetryState(
            failureCount = failureCount,
            nextRetryAtEpochMs = nowEpochMs + delayForFailure(failureCount, jitterPermille),
        )
    }

    fun allowsAttempt(
        state: FolderSyncOutageRetryState,
        trigger: FolderSyncRetryTrigger,
        nowEpochMs: Long,
    ): Boolean {
        return trigger == FolderSyncRetryTrigger.MANUAL_SYNC ||
            trigger == FolderSyncRetryTrigger.CONFIGURATION_CHANGED ||
            state.nextRetryAtEpochMs <= nowEpochMs
    }
}

/**
 * Stores the endpoint-outage circuit in app-private preferences so restarting the app or process
 * cannot reset its network work back to an aggressive retry cadence.
 */
internal class FolderSyncOutageRetryStore(
    context: Context,
    private val nowEpochMs: () -> Long = System::currentTimeMillis,
    private val jitterPermille: () -> Int = { (0..100).random() },
) {
    private val preferences = context.applicationContext.getSharedPreferences(
        PREFERENCES_NAME,
        Context.MODE_PRIVATE,
    )

    fun state(): FolderSyncOutageRetryState {
        val now = nowEpochMs()
        val storedNextRetryAtEpochMs = preferences
            .getLong(KEY_NEXT_RETRY_AT_EPOCH_MS, 0L)
            .coerceAtLeast(0L)
        val nextRetryAtEpochMs = storedNextRetryAtEpochMs.coerceAtMost(
            now + FolderSyncOutageRetryPolicy.MAX_DELAY_MS,
        )
        if (nextRetryAtEpochMs != storedNextRetryAtEpochMs) {
            preferences.edit()
                .putLong(KEY_NEXT_RETRY_AT_EPOCH_MS, nextRetryAtEpochMs)
                .apply()
        }
        return FolderSyncOutageRetryState(
            failureCount = preferences.getInt(KEY_FAILURE_COUNT, 0).coerceAtLeast(0),
            // Backwards wall-clock corrections must not strand the circuit beyond its maximum.
            nextRetryAtEpochMs = nextRetryAtEpochMs,
        )
    }

    fun allowsAttempt(trigger: FolderSyncRetryTrigger): Boolean {
        return FolderSyncOutageRetryPolicy.allowsAttempt(state(), trigger, nowEpochMs())
    }

    fun recordFailure(): FolderSyncOutageRetryState {
        val next = FolderSyncOutageRetryPolicy.afterFailure(
            state = state(),
            nowEpochMs = nowEpochMs(),
            jitterPermille = jitterPermille(),
        )
        preferences.edit()
            .putInt(KEY_FAILURE_COUNT, next.failureCount)
            .putLong(KEY_NEXT_RETRY_AT_EPOCH_MS, next.nextRetryAtEpochMs)
            .apply()
        return next
    }

    fun clear() {
        preferences.edit().clear().apply()
    }

    companion object {
        private const val PREFERENCES_NAME = "ironmesh-folder-sync-outage-retry"
        private const val KEY_FAILURE_COUNT = "failure_count"
        private const val KEY_NEXT_RETRY_AT_EPOCH_MS = "next_retry_at_epoch_ms"
    }
}
