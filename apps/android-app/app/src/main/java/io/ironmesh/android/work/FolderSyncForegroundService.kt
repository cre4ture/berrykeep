package io.ironmesh.android.work

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.IBinder
import android.os.SystemClock
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import io.ironmesh.android.data.DeviceIdentityStorageException
import io.ironmesh.android.data.FolderSyncStorageDiagnosticsHelper
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.IronmeshPreferences
import io.ironmesh.android.data.IronmeshRepository
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File

class FolderSyncForegroundService : Service() {

    private val repository = IronmeshRepository()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val reconcileMutex = Mutex()
    private var statusJob: Job? = null
    private var retryJob: Job? = null
    private var lastLoggedStatusLine: String? = null
    private var lastDesiredSignature: String? = null
    private var waitingSummary: String? = null
    @Volatile
    private var hasAllowedProfiles = false
    private var outageRetryArmed = false
    private var networkCallbackRegistered = false
    private lateinit var outageRetryStore: FolderSyncOutageRetryStore
    private lateinit var networkChangeNotifier: ConflatedNetworkChangeNotifier
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            notifyManagedClientNetworkAvailable()
        }

        override fun onLost(network: Network) {
            Log.i(TAG, "network lost; retaining the current outage retry state")
        }

        override fun onCapabilitiesChanged(
            network: Network,
            networkCapabilities: NetworkCapabilities,
        ) {
            Log.i(TAG, "network capabilities changed; no sync retry is triggered")
        }
    }

    override fun onCreate() {
        super.onCreate()
        serviceRunning = true
        synchronized(localChangeLock) {
            lastLocalChangeElapsedMs = null
        }
        FolderSyncExecutionCoordinator.markContinuousServiceActive()
        outageRetryStore = FolderSyncOutageRetryStore(applicationContext)
        schedulePersistedRetryWakeup(outageRetryStore.state())
        ensureNotificationChannel()
        networkChangeNotifier = ConflatedNetworkChangeNotifier(
            scope = scope,
            onNetworkChange = ::processManagedClientNetworkChange,
            onFailure = { reason, error ->
                Log.w(TAG, "network change processing failed ($reason): ${error.message}")
            },
        )
        startForeground(
            NOTIFICATION_ID,
            buildNotification("Starting continuous sync", "Preparing folder sync runtime"),
        )
        registerNetworkCallback()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        FolderSyncExecutionCoordinator.markContinuousServiceActive()
        when (intent?.action) {
            ACTION_STOP -> {
                stopContinuousSyncAndSelf()
                return START_NOT_STICKY
            }
            ACTION_SYNC_NOW -> {
                syncNow()
                return START_STICKY
            }
            ACTION_APP_FOREGROUNDED -> {
                requestReconcile(
                    reason = "app foregrounded",
                    trigger = FolderSyncRetryTrigger.APP_FOREGROUNDED,
                )
                return START_STICKY
            }
            ACTION_LOCAL_FOLDER_CHANGED -> {
                requestReconcile(
                    reason = "local folder changed",
                    trigger = FolderSyncRetryTrigger.LOCAL_FOLDER_CHANGED,
                )
                return START_STICKY
            }
            ACTION_REFRESH -> {
                clearRetryState()
                requestReconcile(
                    reason = "sync configuration changed",
                    trigger = FolderSyncRetryTrigger.CONFIGURATION_CHANGED,
                )
                return START_STICKY
            }
            else -> {
                requestReconcile(
                    reason = "service start",
                    trigger = FolderSyncRetryTrigger.SERVICE_START,
                )
                return START_STICKY
            }
        }
    }

    override fun onDestroy() {
        serviceRunning = false
        statusJob?.cancel()
        cancelRetryWakeup()
        if (::networkChangeNotifier.isInitialized) {
            networkChangeNotifier.close()
        }
        unregisterNetworkCallback()
        repository.stopAllContinuousFolderSync()
        FolderSyncExecutionCoordinator.releaseContinuousService()
        scope.cancel()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun clearRetryState() {
        outageRetryArmed = false
        outageRetryStore.clear()
        cancelRetryWakeup()
    }

    private fun syncNow() {
        clearRetryState()
        lastDesiredSignature = null
        requestReconcile(
            reason = "manual sync",
            trigger = FolderSyncRetryTrigger.MANUAL_SYNC,
        )
    }

    private fun armOutageRetry(reason: String) {
        if (outageRetryArmed) {
            return
        }

        outageRetryArmed = true
        val state = outageRetryStore.recordFailure()
        schedulePersistedRetryWakeup(state)
        updateNotification("BerryKeep sync paused", buildRetryMessage(reason, state))
    }

    private fun schedulePersistedRetryWakeup(state: FolderSyncOutageRetryState) {
        cancelRetryWakeup()
        val delayMs = state.nextRetryAtEpochMs - System.currentTimeMillis()
        if (state.failureCount == 0 || delayMs <= 0L) {
            return
        }
        retryJob = scope.launch {
            delay(delayMs)
            retryJob = null
            requestReconcile(
                reason = "outage backoff expired",
                trigger = FolderSyncRetryTrigger.BACKOFF_TIMER,
            )
        }
    }

    private fun cancelRetryWakeup() {
        retryJob?.cancel()
        retryJob = null
    }

    private fun buildRetryMessage(reason: String, state: FolderSyncOutageRetryState): String {
        val normalizedReason = summarizeReason(reason)
        val retryAt = formatRetryAt(state.nextRetryAtEpochMs)
        return "Retrying only after $retryAt and an allowed sync event because $normalizedReason"
    }

    private fun formatRetryDelay(delayMs: Long): String {
        val totalSeconds = (delayMs / 1000L).coerceAtLeast(1L)
        return if (totalSeconds < 60L) {
            "${totalSeconds}s"
        } else {
            val minutes = totalSeconds / 60L
            val seconds = totalSeconds % 60L
            if (seconds == 0L) {
                "${minutes}m"
            } else {
                "${minutes}m ${seconds}s"
            }
        }
    }

    private fun formatRetryAt(nextRetryAtEpochMs: Long): String {
        val remainingMs = (nextRetryAtEpochMs - System.currentTimeMillis()).coerceAtLeast(0L)
        return "${formatRetryDelay(remainingMs)} backoff"
    }

    private fun currentErrorMessage(status: FolderSyncServiceStatus?): String {
        return status?.profiles
            ?.firstOrNull { profile -> profile.state == "error" }
            ?.lastError
            ?.takeIf { message -> message.isNotBlank() }
            ?: status?.currentActivity?.takeIf { activity -> activity.isNotBlank() }
            ?: status?.serviceMessage?.takeIf { message -> message.isNotBlank() }
            ?: "failed to start sync"
    }

    private fun summarizeReason(reason: String): String {
        val firstLine = reason
            .lineSequence()
            .map { line -> line.trim() }
            .firstOrNull { line -> line.isNotEmpty() }
            .orEmpty()
        val normalized = firstLine.ifBlank { "connection error" }
        return if (normalized.length <= 180) {
            normalized
        } else {
            normalized.take(177) + "..."
        }
    }

    private suspend fun reconcileProfilesLocked(): Boolean {
        FolderSyncExecutionCoordinator.awaitOneShotCompletion()
        return withContext(Dispatchers.IO) {
            val deviceAuth = IronmeshPreferences.getDeviceAuthState(applicationContext)
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            val serverCaPem = deviceAuth.serverCaPem.takeIf { !it.isNullOrBlank() }
            val profiles = IronmeshPreferences
                .getFolderSyncConfigs(applicationContext)
                .filter { it.enabled }

            if (profiles.isEmpty()) {
                repository.stopAllContinuousFolderSync()
                waitingSummary = null
                hasAllowedProfiles = false
                lastDesiredSignature = null
                clearRetryState()
                withContext(Dispatchers.Main) {
                    stopForeground(STOP_FOREGROUND_REMOVE)
                    stopSelf()
                }
                return@withContext false
            }

            if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
                clearRetryState()
                hasAllowedProfiles = false
                applyDesiredState(
                    desiredSignature = "",
                    desiredProfiles = emptyList(),
                    connectionInput = connectionInput,
                    serverCaPem = serverCaPem,
                    clientIdentityJson = clientIdentityJson,
                )
                waitingSummary = "Enroll this device before continuous sync can run"
                updateNotification("BerryKeep sync paused", requireNotNull(waitingSummary))
                return@withContext true
            }

            val networkDecisions = FolderSyncNetworkGate.evaluateProfiles(applicationContext, profiles)
            val allowedProfiles = networkDecisions
                .filter { evaluation -> evaluation.decision.allowed }
                .map { evaluation -> evaluation.profile }
            val blockedProfiles = networkDecisions
                .filterNot { evaluation -> evaluation.decision.allowed }

            blockedProfiles.forEach { evaluation ->
                Log.i(
                    TAG,
                    "holding continuous sync profile=${evaluation.profile.id} reason=${evaluation.decision.reason}",
                )
            }

            waitingSummary = blockedProfiles.firstOrNull()?.let { evaluation ->
                buildWaitingSummary(
                    blockedProfileCount = blockedProfiles.size,
                    profileLabel = evaluation.profile.label,
                    reason = evaluation.decision.reason,
                )
            }
            hasAllowedProfiles = allowedProfiles.isNotEmpty()

            val desiredSignature = buildDesiredSignature(
                connectionInput = connectionInput,
                serverCaPem = serverCaPem,
                clientIdentityJson = clientIdentityJson,
                profiles = allowedProfiles,
            )
            applyDesiredState(
                desiredSignature = desiredSignature,
                desiredProfiles = allowedProfiles,
                connectionInput = connectionInput,
                serverCaPem = serverCaPem,
                clientIdentityJson = clientIdentityJson,
            )

            if (allowedProfiles.isEmpty()) {
                cancelRetryWakeup()
                updateNotification(
                    "Waiting for allowed network",
                    waitingSummary ?: "No enabled sync profile is allowed on the current network",
                )
            }

            true
        }
    }

    private fun startStatusLoop() {
        if (statusJob?.isActive == true) {
            return
        }

        statusJob = scope.launch {
            while (isActive) {
                val status = withContext(Dispatchers.IO) {
                    runCatching { repository.getContinuousFolderSyncStatus() }.getOrNull()
                }
                val activeProfileCount = status?.activeProfileCount ?: 0L
                val waitingMessage = waitingSummary
                when {
                    !waitingMessage.isNullOrBlank() && !hasAllowedProfiles && activeProfileCount == 0L -> {
                        cancelRetryWakeup()
                    }
                    (status?.errorProfileCount ?: 0L) > 0L -> {
                        armOutageRetry(currentErrorMessage(status))
                    }
                    activeProfileCount > 0L -> {
                        clearRetryState()
                    }
                }
                val (title, detail) = when {
                    !waitingMessage.isNullOrBlank() && !hasAllowedProfiles && activeProfileCount == 0L -> {
                        "Waiting for allowed network" to waitingMessage
                    }
                    (status?.errorProfileCount ?: 0L) > 0L -> {
                        "BerryKeep sync paused" to buildRetryMessage(
                            currentErrorMessage(status),
                            outageRetryStore.state(),
                        )
                    }
                    else -> {
                        val contentText = status?.serviceMessage ?: "Continuous sync is starting"
                        val notificationTitle = when (status?.serviceState) {
                            "error" -> "BerryKeep sync issue"
                            "syncing" -> "BerryKeep syncing ${status.syncingProfileCount}/${status.activeProfileCount}"
                            "running" -> "BerryKeep sync active"
                            else -> "BerryKeep sync idle"
                        }
                        val notificationDetail = status?.currentActivity
                            ?.takeIf { it.isNotBlank() }
                            ?: status?.activeSummary
                                ?.takeIf { it.isNotBlank() }
                            ?: contentText
                        notificationTitle to notificationDetail
                    }
                }
                val logLine = status?.profiles
                    ?.takeIf { it.isNotEmpty() }
                    ?.joinToString(" | ") { profile ->
                        listOf(
                            profile.label,
                            profile.state,
                            profile.phase.takeIf { it.isNotBlank() },
                            profile.activity.takeIf { it.isNotBlank() },
                            profile.message.takeIf { it.isNotBlank() },
                        ).joinToString(":")
                    }
                    ?: detail
                if (logLine != lastLoggedStatusLine) {
                    Log.i(TAG, "continuous sync status: $logLine")
                    lastLoggedStatusLine = logLine
                }
                updateNotification(title, detail)
                delay(1_000)
            }
        }
    }

    private fun stopContinuousSyncAndSelf() {
        scope.launch(Dispatchers.IO) {
            repository.stopAllContinuousFolderSync()
            waitingSummary = null
            hasAllowedProfiles = false
            lastDesiredSignature = null
            clearRetryState()
            withContext(Dispatchers.Main) {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    private suspend fun applyDesiredState(
        desiredSignature: String,
        desiredProfiles: List<io.ironmesh.android.data.FolderSyncConfig>,
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ) {
        if (desiredSignature == lastDesiredSignature) {
            return
        }

        repository.stopAllContinuousFolderSync()

        for (profile in desiredProfiles) {
            val localFolder = File(profile.localFolder)
            val storageDiagnostics = FolderSyncStorageDiagnosticsHelper.collect(
                contentResolver = applicationContext.contentResolver,
                localFolder = profile.localFolder,
                explicitTreeUri = profile.localFolderTreeUri,
            )
            Log.i(
                TAG,
                "starting continuous sync profile=${profile.id} label=${profile.label} prefix=${profile.prefix.ifBlank { "<root>" }} localFolder=${profile.localFolder} exists=${localFolder.exists()} isDirectory=${localFolder.isDirectory} canRead=${localFolder.canRead()} rawSampleChildren=${storageDiagnostics.rawSampleChildren} treeUriSource=${storageDiagnostics.treeUriSource} treeUri=${storageDiagnostics.treeUri ?: "<none>"} safSampleChildren=${storageDiagnostics.safSampleChildren}",
            )
            repository.startContinuousFolderSync(
                profileId = profile.id,
                label = profile.label,
                connectionInput = connectionInput,
                localFolder = profile.localFolder,
                localFolderTreeUri = profile.localFolderTreeUri,
                prefix = profile.prefix.ifBlank { null },
                depth = profile.depth,
                serverCaPem = serverCaPem,
                clientIdentityJson = clientIdentityJson,
            )
        }

        lastDesiredSignature = desiredSignature
    }

    private fun requestReconcile(
        reason: String,
        trigger: FolderSyncRetryTrigger,
    ) {
        scope.launch {
            val started = reconcileMutex.withLock {
                reconcileRequestLocked(reason, trigger)
            }
            if (started) {
                startStatusLoop()
            }
        }
    }

    /**
     * Runs under [reconcileMutex] so every pending explicit request observes the retry state left
     * by the request before it. This retains manual sync and configuration changes while still
     * suppressing a burst of automatic retry events inside one outage window.
     */
    private suspend fun reconcileRequestLocked(
        reason: String,
        trigger: FolderSyncRetryTrigger,
    ): Boolean {
        if (!outageRetryStore.allowsAttempt(trigger)) {
            val state = outageRetryStore.state()
            Log.i(
                TAG,
                "holding $trigger until outage backoff expires at ${state.nextRetryAtEpochMs}",
            )
            updateNotification(
                "BerryKeep sync paused",
                buildRetryMessage("previous connection error", state),
            )
            return false
        }

        if (outageRetryStore.state().failureCount > 0) {
            lastDesiredSignature = null
        }
        outageRetryArmed = false
        return try {
            Log.i(TAG, "reconciling continuous sync: $reason ($trigger)")
            reconcileProfilesLocked()
        } catch (error: DeviceIdentityStorageException) {
            val message = error.message
                ?: "Protected device identity is unavailable; enroll again."
            clearRetryState()
            repository.stopAllContinuousFolderSync()
            waitingSummary = message
            updateNotification("Ironmesh sync paused", message)
            false
        } catch (error: Exception) {
            val retryReason = error.message ?: "Failed to start sync"
            armOutageRetry(retryReason)
            false
        }
    }

    private fun notifyManagedClientNetworkAvailable() {
        if (::networkChangeNotifier.isInitialized) {
            networkChangeNotifier.submit("network available")
        }
    }

    private suspend fun processManagedClientNetworkChange(reason: String) {
        try {
            withContext(Dispatchers.IO) {
                val deviceAuth = IronmeshPreferences.getDeviceAuthState(applicationContext)
                val connectionInput = deviceAuth.connectionBootstrapJson()
                val clientIdentityJson = deviceAuth.toClientIdentityJson()
                if (connectionInput.isNotBlank() && !clientIdentityJson.isNullOrBlank()) {
                    repository.notifyNetworkChanged(
                        connectionInput = connectionInput,
                        serverCaPem = deviceAuth.serverCaPem.takeIf { !it.isNullOrBlank() },
                        clientIdentityJson = clientIdentityJson,
                    )
                }
            }
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            Log.w(TAG, "managed client network hint failed ($reason): ${error.message}")
        }
        requestReconcile(
            reason = reason,
            trigger = FolderSyncRetryTrigger.NETWORK_AVAILABLE,
        )
    }

    private fun registerNetworkCallback() {
        if (networkCallbackRegistered) {
            return
        }
        val connectivityManager = getSystemService(ConnectivityManager::class.java) ?: return
        runCatching {
            connectivityManager.registerDefaultNetworkCallback(networkCallback)
            networkCallbackRegistered = true
        }.onFailure { error ->
            Log.w(TAG, "failed to register network callback: ${error.message}")
        }
    }

    private fun unregisterNetworkCallback() {
        if (!networkCallbackRegistered) {
            return
        }
        val connectivityManager = getSystemService(ConnectivityManager::class.java) ?: return
        runCatching {
            connectivityManager.unregisterNetworkCallback(networkCallback)
        }
        networkCallbackRegistered = false
    }

    private fun buildDesiredSignature(
        connectionInput: String,
        serverCaPem: String?,
        clientIdentityJson: String?,
        profiles: List<io.ironmesh.android.data.FolderSyncConfig>,
    ): String {
        return buildString {
            append(connectionInput.trim())
            append('|')
            append(serverCaPem.orEmpty())
            append('|')
            append(clientIdentityJson.orEmpty())
            profiles
                .sortedBy { profile -> profile.id }
                .forEach { profile ->
                    append('|')
                    append(profile.id)
                    append(':')
                    append(profile.hashCode())
                }
        }
    }

    private fun buildWaitingSummary(
        blockedProfileCount: Int,
        profileLabel: String,
        reason: String,
    ): String {
        return if (blockedProfileCount <= 1) {
            "$profileLabel is waiting: $reason"
        } else {
            "$blockedProfileCount sync profiles are waiting. First block: $profileLabel: $reason"
        }
    }

    private fun updateNotification(title: String, text: String) {
        val notificationManager = getSystemService(NotificationManager::class.java)
        notificationManager.notify(NOTIFICATION_ID, buildNotification(title, text))
    }

    private fun buildNotification(title: String, text: String) =
        NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .build()

    private fun ensureNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }
        val notificationManager = getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(
            CHANNEL_ID,
            "BerryKeep Sync",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Continuous folder synchronization"
        }
        notificationManager.createNotificationChannel(channel)
    }

    companion object {
        private const val TAG = "FolderSyncService"
        private const val CHANNEL_ID = "ironmesh-folder-sync"
        private const val NOTIFICATION_ID = 4001
        private const val ACTION_REFRESH = "io.ironmesh.android.action.FOLDER_SYNC_REFRESH"
        private const val ACTION_SYNC_NOW = "io.ironmesh.android.action.FOLDER_SYNC_NOW"
        private const val ACTION_APP_FOREGROUNDED = "io.ironmesh.android.action.FOLDER_SYNC_APP_FOREGROUNDED"
        private const val ACTION_LOCAL_FOLDER_CHANGED = "io.ironmesh.android.action.FOLDER_SYNC_LOCAL_FOLDER_CHANGED"
        private const val ACTION_STOP = "io.ironmesh.android.action.FOLDER_SYNC_STOP"
        private const val LOCAL_CHANGE_DEBOUNCE_MS = 5_000L
        private val localChangeLock = Any()
        @Volatile
        private var serviceRunning = false
        private var lastLocalChangeElapsedMs: Long? = null

        fun syncConfigChanged(context: Context) {
            startOwnedService(context, ACTION_REFRESH)
        }

        fun stop(context: Context) {
            FolderSyncExecutionCoordinator.cancelAllContinuousStartRequests()
            context.stopService(Intent(context, FolderSyncForegroundService::class.java))
        }

        fun syncNow(context: Context) {
            startOwnedService(context, ACTION_SYNC_NOW)
        }

        /** A visible app is an explicit, user-initiated opportunity to retry a due outage. */
        fun appForegrounded(context: Context) {
            if (IronmeshPreferences.getFolderSyncConfigs(context).none { profile -> profile.enabled }) {
                return
            }
            startOwnedService(context, ACTION_APP_FOREGROUNDED)
        }

        /**
         * A local folder mutation is an explicit opportunity to retry a due outage. It is still
         * gated by the persisted circuit, so bursts of camera/SAF notifications cause no network
         * work while an endpoint backoff is active.
         */
        fun localFolderChanged(context: Context, treeUriString: String) {
            if (!serviceRunning) {
                return
            }
            val hasMatchingEnabledProfile = IronmeshPreferences
                .getFolderSyncConfigs(context)
                .any { profile ->
                    profile.enabled && profile.localFolderTreeUri == treeUriString
                }
            if (!hasMatchingEnabledProfile) {
                return
            }
            val elapsedMs = SystemClock.elapsedRealtime()
            synchronized(localChangeLock) {
                val previousElapsedMs = lastLocalChangeElapsedMs
                if (
                    previousElapsedMs != null &&
                    elapsedMs - previousElapsedMs < LOCAL_CHANGE_DEBOUNCE_MS
                ) {
                    return
                }
                lastLocalChangeElapsedMs = elapsedMs
            }
            // This is delivered only to an already-running foreground service. It avoids trying
            // to start a foreground service from a background ContentObserver callback.
            context.startService(
                Intent(context, FolderSyncForegroundService::class.java).apply {
                    action = ACTION_LOCAL_FOLDER_CHANGED
                },
            )
        }

        private fun startOwnedService(context: Context, action: String) {
            FolderSyncExecutionCoordinator.requestContinuousStart()
            try {
                ContextCompat.startForegroundService(
                    context,
                    Intent(context, FolderSyncForegroundService::class.java).apply {
                        this.action = action
                    },
                )
            } catch (error: RuntimeException) {
                FolderSyncExecutionCoordinator.cancelContinuousStartRequest()
                throw error
            }
        }

    }
}
