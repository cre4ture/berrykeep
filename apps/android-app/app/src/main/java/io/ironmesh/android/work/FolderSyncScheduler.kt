package io.ironmesh.android.work

import android.content.Context
import android.util.Log
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import io.ironmesh.android.data.IronmeshPreferences
import java.util.concurrent.TimeUnit

object FolderSyncScheduler {
    private const val TAG = "FolderSyncScheduler"
    private const val UNIQUE_PERIODIC_WORK = "ironmesh-folder-sync-periodic"
    private const val UNIQUE_OUTAGE_RETRY_ATTEMPT_WORK = "ironmesh-folder-sync-outage-attempt"
    private const val PERIODIC_INTERVAL_MINUTES = 15L

    fun reschedule(
        context: Context,
        resetOutageBackoff: Boolean = false,
    ) {
        val workManager = WorkManager.getInstance(context)
        val enabledProfiles = IronmeshPreferences
            .getFolderSyncConfigs(context)
            .filter { it.enabled }
        val hasEnabledProfiles = enabledProfiles.isNotEmpty()

        if (!hasEnabledProfiles) {
            workManager.cancelUniqueWork(UNIQUE_PERIODIC_WORK)
            clearOutageRetryCircuit(context)
            FolderSyncForegroundService.stop(context)
            return
        }

        if (resetOutageBackoff) {
            // Persist the reset before asking the service to reconcile. If Android delays or
            // rejects that service start, a stale outage worker must still not survive a real
            // configuration change.
            clearOutageRetryCircuit(context)
        }

        val constraints = Constraints.Builder()
            .setRequiredNetworkType(requiredNetworkType(enabledProfiles))
            .build()

        val request = PeriodicWorkRequestBuilder<FolderSyncWorker>(
            PERIODIC_INTERVAL_MINUTES,
            TimeUnit.MINUTES,
        )
            .setConstraints(constraints)
            .build()

        workManager.enqueueUniquePeriodicWork(
            UNIQUE_PERIODIC_WORK,
            ExistingPeriodicWorkPolicy.UPDATE,
            request,
        )

        runCatching {
            if (resetOutageBackoff) {
                FolderSyncForegroundService.syncConfigChanged(context)
            } else {
                FolderSyncForegroundService.syncRuntimeRestored(context)
            }
        }.onFailure { error ->
            // Android can reject foreground-service starts while the app is backgrounded. The
            // periodic work above is deliberately already registered as the durable fallback.
            Log.w(TAG, "failed to start folder sync foreground service: ${error.message}")
        }
    }

    fun runNow(context: Context) {
        val enabledProfiles = IronmeshPreferences
            .getFolderSyncConfigs(context)
            .filter { it.enabled }
        val hasEnabledProfiles = enabledProfiles.isNotEmpty()
        if (!hasEnabledProfiles) {
            clearOutageRetryCircuit(context)
            FolderSyncForegroundService.stop(context)
            return
        }
        FolderSyncForegroundService.syncNow(context)
    }

    /**
     * Runs a due outage retry without starting a foreground service from a background worker.
     * WorkManager holds this request until the most permissive enabled profile can use the
     * current network; [FolderSyncWorker] applies the per-profile policy again before syncing.
     */
    internal fun enqueueOutageRetryAttempt(context: Context) {
        val appContext = context.applicationContext
        val enabledProfiles = IronmeshPreferences
            .getFolderSyncConfigs(appContext)
            .filter { it.enabled }
        if (enabledProfiles.isEmpty()) {
            clearOutageRetryCircuit(appContext)
            return
        }

        val request = OneTimeWorkRequestBuilder<FolderSyncWorker>()
            .setConstraints(
                Constraints.Builder()
                    .setRequiredNetworkType(requiredNetworkType(enabledProfiles))
                    .build(),
            )
            .build()
        WorkManager.getInstance(appContext).enqueueUniqueWork(
            UNIQUE_OUTAGE_RETRY_ATTEMPT_WORK,
            ExistingWorkPolicy.REPLACE,
            request,
        )
    }

    private fun requiredNetworkType(enabledProfiles: List<io.ironmesh.android.data.FolderSyncConfig>): NetworkType {
        val mayUseMeteredNetwork = enabledProfiles.any { profile ->
            val policy = profile.networkPolicy.normalized()
            policy.allowCellular || policy.allowOtherConnections
        }
        return if (mayUseMeteredNetwork) {
            NetworkType.CONNECTED
        } else {
            NetworkType.UNMETERED
        }
    }

    /**
     * A profile removal must cancel both the doze-aware work request and its persisted deadline.
     * The foreground service may already have been killed, so its onDestroy cleanup is not enough.
     */
    internal fun clearOutageRetryCircuit(context: Context) {
        val appContext = context.applicationContext
        WorkManager.getInstance(appContext).cancelUniqueWork(UNIQUE_OUTAGE_RETRY_ATTEMPT_WORK)
        FolderSyncOutageRetryScheduler.cancel(appContext)
        FolderSyncOutageRetryStore(appContext).clear()
    }
}
