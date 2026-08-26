package io.ironmesh.android.work

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import io.ironmesh.android.data.DeviceIdentityStorageException
import io.ironmesh.android.data.FolderSyncConfig
import io.ironmesh.android.data.IronmeshPreferences
import io.ironmesh.android.data.IronmeshRepository
import io.ironmesh.android.data.FolderSyncStorageDiagnosticsHelper
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

class FolderSyncWorker(
    appContext: Context,
    params: WorkerParameters,
) : CoroutineWorker(appContext, params) {

    private val repository = IronmeshRepository()
    private val outageRetryStore = FolderSyncOutageRetryStore(appContext)

    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val nativeContinuousActive = repository.hasContinuousFolderSyncActive()
        if (!FolderSyncExecutionCoordinator.tryBeginOneShot(nativeContinuousActive)) {
            // A stale continuous-start marker must not turn a due outage wake-up into a
            // zero-delay WorkManager cycle while no foreground service is actually running.
            rearmOutageRetry(deferDueAttempt = true)
            Log.i(TAG, "continuous folder sync is requested or active; skipping one-shot worker run")
            return@withContext Result.success()
        }

        try {
            val deviceAuth = try {
                IronmeshPreferences.getDeviceAuthState(applicationContext)
            } catch (error: DeviceIdentityStorageException) {
                val message = error.message ?: "Protected device identity is unavailable; enroll again."
                Log.e(TAG, message, error)
                return@withContext Result.failure(workDataOf(OUTPUT_ERROR to message))
            }
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            val serverCaPem = deviceAuth.serverCaPem.takeIf { !it.isNullOrBlank() }
            val profiles = IronmeshPreferences
                .getFolderSyncConfigs(applicationContext)
                .filter { it.enabled }

            if (profiles.isEmpty()) {
                clearOutageRetry()
                return@withContext Result.success()
            }

            val networkDecisions = FolderSyncNetworkGate.evaluateProfiles(applicationContext, profiles)
            val eligibleProfiles = networkDecisions
                .filter { evaluation -> evaluation.decision.allowed }
                .map { evaluation -> evaluation.profile }
            val skippedProfiles = networkDecisions
                .filterNot { evaluation -> evaluation.decision.allowed }

            skippedProfiles.forEach { evaluation ->
                Log.i(
                    TAG,
                    "skipping one-shot sync profile=${evaluation.profile.id} reason=${evaluation.decision.reason}",
                )
            }

            if (eligibleProfiles.isEmpty()) {
                // A WorkManager network constraint cannot express all per-profile policy (for
                // example, disallowing roaming). Keep the persisted circuit armed without
                // immediately running the same blocked attempt again.
                rearmOutageRetry(deferDueAttempt = true)
                Log.i(TAG, "one-shot sync skipped because no enabled profile matches the current network policy")
                return@withContext Result.success()
            }

            if (!outageRetryStore.allowsAttempt(FolderSyncRetryTrigger.PERIODIC_WORK)) {
                val retryState = outageRetryStore.state()
                rearmOutageRetry(retryState)
                Log.i(
                    TAG,
                    "one-shot sync held by persisted endpoint backoff until ${retryState.nextRetryAtEpochMs}",
                )
                return@withContext Result.success()
            }

            val failures = mutableListOf<String>()

            for (profile in eligibleProfiles) {
                FolderSyncExecutionCoordinator.updateOneShotProfile(profile.label)
                runCatching {
                    syncProfile(connectionInput, serverCaPem, clientIdentityJson, profile)
                }.onFailure { error ->
                    failures += "${profile.label}: ${error.message ?: "unknown"}"
                    Log.e(TAG, "folder sync failed for profile=${profile.id}", error)
                }
            }

            if (failures.isEmpty()) {
                clearOutageRetry()
                Result.success()
            } else {
                val retryState = outageRetryStore.recordFailure()
                FolderSyncOutageRetryScheduler.schedule(applicationContext, retryState)
                Log.i(
                    TAG,
                    "one-shot sync failure recorded; next eligible retry is after ${retryState.nextRetryAtEpochMs}",
                )
                // The foreground service and this periodic safety net share the persisted policy.
                // Returning retry() would add an independent WorkManager exponential loop.
                Result.success()
            }
        } finally {
            FolderSyncExecutionCoordinator.finishOneShot()
        }
    }

    private suspend fun syncProfile(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
        profile: FolderSyncConfig,
    ) {
        val localFolder = File(profile.localFolder)
        val storageDiagnostics = FolderSyncStorageDiagnosticsHelper.collect(
            contentResolver = applicationContext.contentResolver,
            localFolder = profile.localFolder,
            explicitTreeUri = profile.localFolderTreeUri,
        )
        Log.i(
            TAG,
            "running one-shot sync profile=${profile.id} prefix=${profile.prefix.ifBlank { "<root>" }} localFolder=${profile.localFolder} exists=${localFolder.exists()} isDirectory=${localFolder.isDirectory} canRead=${localFolder.canRead()} rawSampleChildren=${storageDiagnostics.rawSampleChildren} treeUriSource=${storageDiagnostics.treeUriSource} treeUri=${storageDiagnostics.treeUri ?: "<none>"} safSampleChildren=${storageDiagnostics.safSampleChildren}",
        )
        File(profile.localFolder).mkdirs()
        repository.runFolderSyncOnce(
            connectionInput = connectionInput,
            localFolder = profile.localFolder,
            localFolderTreeUri = profile.localFolderTreeUri,
            prefix = profile.prefix.ifBlank { null },
            depth = profile.depth,
            serverCaPem = serverCaPem,
            clientIdentityJson = clientIdentityJson,
        )

        Log.i(TAG, "synced profile=${profile.id} via rust runtime")
    }

    private fun clearOutageRetry() {
        outageRetryStore.clear()
        FolderSyncOutageRetryScheduler.cancel(applicationContext)
    }

    /** Preserves the durable outage wake-up when this worker deliberately skips an attempt. */
    private fun rearmOutageRetry(
        state: FolderSyncOutageRetryState = outageRetryStore.state(),
        deferDueAttempt: Boolean = false,
    ) {
        val scheduledState = if (deferDueAttempt) outageRetryStore.deferDueAttempt() else state
        if (scheduledState.failureCount > 0) {
            FolderSyncOutageRetryScheduler.schedule(applicationContext, scheduledState)
        }
    }

    private companion object {
        private const val TAG = "FolderSyncWorker"
        private const val OUTPUT_ERROR = "error"
    }
}
