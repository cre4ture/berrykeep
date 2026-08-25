package io.ironmesh.android.work

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import java.util.concurrent.TimeUnit

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
    NETWORK_POLICY_CHANGED,
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

    /**
     * The caller has already established that an attempt is allowed. A persisted outage may force
     * one continuous-sync restart, but subsequent app/network/content events must not interrupt
     * the recovery that restart began.
     */
    fun shouldForceReconcileAfterAllowedAttempt(
        state: FolderSyncOutageRetryState,
        outageRetryArmed: Boolean,
    ): Boolean = state.failureCount > 0 && outageRetryArmed

    /** A retry is healthy only after every profile from its desired set is running again. */
    fun allDesiredProfilesRunning(
        desiredProfileCount: Int,
        activeProfileCount: Long,
        runningProfileCount: Long,
    ): Boolean = desiredProfileCount > 0 &&
        activeProfileCount == desiredProfileCount.toLong() &&
        runningProfileCount == desiredProfileCount.toLong()
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
        val failureCount = preferences.getInt(KEY_FAILURE_COUNT, 0).coerceAtLeast(0)
        val storedNextRetryAtEpochMs = preferences
            .getLong(KEY_NEXT_RETRY_AT_EPOCH_MS, 0L)
            .coerceAtLeast(0L)
        val nextRetryAtEpochMs = if (failureCount == 0) {
            0L
        } else {
            storedNextRetryAtEpochMs.coerceAtMost(
                now + FolderSyncOutageRetryPolicy.delayForFailure(
                    failureCount,
                    jitterPermille = 100,
                ),
            )
        }
        if (nextRetryAtEpochMs != storedNextRetryAtEpochMs) {
            preferences.edit()
                .putLong(KEY_NEXT_RETRY_AT_EPOCH_MS, nextRetryAtEpochMs)
                .apply()
        }
        return FolderSyncOutageRetryState(
            failureCount = failureCount,
            // Backwards wall-clock corrections cannot extend a circuit beyond its stored rung.
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

/**
 * Arms one doze-aware wake-up for the persistent circuit. The worker re-checks the wall-clock
 * deadline because users and NTP can adjust the clock after WorkManager accepted the delay.
 */
internal object FolderSyncOutageRetryScheduler {
    private const val UNIQUE_OUTAGE_RETRY_WORK = "ironmesh-folder-sync-outage-retry"

    fun schedule(context: Context, state: FolderSyncOutageRetryState) {
        if (state.failureCount == 0) {
            cancel(context)
            return
        }
        val initialDelayMs = (state.nextRetryAtEpochMs - System.currentTimeMillis()).coerceAtLeast(0L)
        val request = OneTimeWorkRequestBuilder<FolderSyncOutageRetryWorker>()
            .setInitialDelay(initialDelayMs, TimeUnit.MILLISECONDS)
            .build()
        WorkManager.getInstance(context.applicationContext).enqueueUniqueWork(
            UNIQUE_OUTAGE_RETRY_WORK,
            ExistingWorkPolicy.REPLACE,
            request,
        )
    }

    fun cancel(context: Context) {
        WorkManager.getInstance(context.applicationContext).cancelUniqueWork(UNIQUE_OUTAGE_RETRY_WORK)
    }
}

class FolderSyncOutageRetryWorker(
    appContext: Context,
    params: WorkerParameters,
) : CoroutineWorker(appContext, params) {
    override suspend fun doWork(): Result {
        val retryStore = FolderSyncOutageRetryStore(applicationContext)
        val state = retryStore.state()
        if (state.failureCount == 0) {
            return Result.success()
        }
        if (!retryStore.allowsAttempt(FolderSyncRetryTrigger.BACKOFF_TIMER)) {
            FolderSyncOutageRetryScheduler.schedule(applicationContext, state)
            return Result.success()
        }

        if (!FolderSyncForegroundService.signalScheduledRetryIfRunning(applicationContext)) {
            FolderSyncScheduler.enqueueOutageRetryAttempt(applicationContext)
        }
        return Result.success()
    }
}
