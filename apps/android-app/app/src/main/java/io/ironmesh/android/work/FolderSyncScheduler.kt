package io.ironmesh.android.work

import android.content.Context
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import io.ironmesh.android.data.IronmeshPreferences
import java.util.concurrent.TimeUnit

object FolderSyncScheduler {
    private const val UNIQUE_PERIODIC_WORK = "ironmesh-folder-sync-periodic"
    private const val PERIODIC_INTERVAL_MINUTES = 15L

    fun reschedule(context: Context) {
        val workManager = WorkManager.getInstance(context)
        val enabledProfiles = IronmeshPreferences
            .getFolderSyncConfigs(context)
            .filter { it.enabled }
        val hasEnabledProfiles = enabledProfiles.isNotEmpty()

        if (!hasEnabledProfiles) {
            workManager.cancelUniqueWork(UNIQUE_PERIODIC_WORK)
            FolderSyncForegroundService.stop(context)
            return
        }

        FolderSyncForegroundService.syncConfigChanged(context)

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
    }

    fun runNow(context: Context) {
        val enabledProfiles = IronmeshPreferences
            .getFolderSyncConfigs(context)
            .filter { it.enabled }
        val hasEnabledProfiles = enabledProfiles.isNotEmpty()
        if (!hasEnabledProfiles) {
            FolderSyncForegroundService.stop(context)
            return
        }
        FolderSyncForegroundService.syncNow(context)
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
}
