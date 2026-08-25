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

private data class FolderSyncStatusPollSnapshot(
    val status: FolderSyncServiceStatus?,
    val waitingMessage: String?,
    val hasAllowedProfiles: Boolean,
    val retryState: FolderSyncOutageRetryState,
)

class FolderSyncForegroundService : Service() {

    private val repository = IronmeshRepository()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val reconcileMutex = Mutex()
    private var statusJob: Job? = null
    private var lastLoggedStatusLine: String? = null
    private var lastDesiredSignature: String? = null
    private var waitingSummary: String? = null
    @Volatile
    private var hasAllowedProfiles = false
    private var outageRetryArmed = false
    private var outageRetryWakeupScheduled = false
    private var networkCallbackRegistered = false
    private lateinit var outageRetryStore: FolderSyncOutageRetryStore
    private lateinit var networkChangeNotifier: ConflatedNetworkChangeNotifier
    private lateinit var networkPolicyChangeNotifier: ConflatedNetworkChangeNotifier
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            notifyManagedClientNetworkAvailable()
        }

        override fun onLost(network: Network) {
            notifyNetworkPolicyChanged("network lost")
        }

        override fun onCapabilitiesChanged(
            network: Network,
            networkCapabilities: NetworkCapabilities,
        ) {
            notifyNetworkPolicyChanged("network capabilities changed")
        }
    }

    override fun onCreate() {
        super.onCreate()
        serviceRunning = true
        synchronized(localChangeLock) {
            lastLocalChangeElapsedMsByTreeUri.clear()
        }
        FolderSyncExecutionCoordinator.markContinuousServiceActive()
        outageRetryStore = FolderSyncOutageRetryStore(applicationContext)
        val persistedRetryState = outageRetryStore.state()
        outageRetryArmed = persistedRetryState.failureCount > 0
        schedulePersistedRetryWakeup(persistedRetryState)
        ensureNotificationChannel()
        networkChangeNotifier = ConflatedNetworkChangeNotifier(
            scope = scope,
            onNetworkChange = ::processManagedClientNetworkChange,
            onFailure = { reason, error ->
                Log.w(TAG, "network change processing failed ($reason): ${error.message}")
            },
        )
        networkPolicyChangeNotifier = ConflatedNetworkChangeNotifier(
            scope = scope,
            onNetworkChange = ::processNetworkPolicyChange,
            onFailure = { reason, error ->
                Log.w(TAG, "network policy processing failed ($reason): ${error.message}")
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
                requestReconcile(
                    reason = "sync configuration changed",
                    trigger = FolderSyncRetryTrigger.CONFIGURATION_CHANGED,
                )
                return START_STICKY
            }
            ACTION_RESTORE -> {
                requestReconcile(
                    reason = "sync runtime restored",
                    trigger = FolderSyncRetryTrigger.SERVICE_START,
                )
                return START_STICKY
            }
            ACTION_BACKOFF_TIMER -> {
                requestReconcile(
                    reason = "outage backoff expired",
                    trigger = FolderSyncRetryTrigger.BACKOFF_TIMER,
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
        if (::networkPolicyChangeNotifier.isInitialized) {
            networkPolicyChangeNotifier.close()
        }
        unregisterNetworkCallback()
        repository.stopAllContinuousFolderSync()
        FolderSyncExecutionCoordinator.releaseContinuousService()
        scope.cancel()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun clearRetryState() {
        val hasPersistedRetryState = outageRetryStore.state().failureCount > 0
        if (!outageRetryArmed && !hasPersistedRetryState && !outageRetryWakeupScheduled) {
            return
        }
        outageRetryArmed = false
        if (hasPersistedRetryState) {
            outageRetryStore.clear()
        }
        cancelRetryWakeup()
    }

    private fun syncNow() {
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
        if (state.failureCount == 0) {
            cancelRetryWakeup(force = true)
            return
        }
        FolderSyncOutageRetryScheduler.schedule(applicationContext, state)
        outageRetryWakeupScheduled = true
    }

    private fun cancelRetryWakeup(force: Boolean = false) {
        if (!force && !outageRetryWakeupScheduled) {
            return
        }
        outageRetryWakeupScheduled = false
        FolderSyncOutageRetryScheduler.cancel(applicationContext)
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
                val snapshot = reconcileMutex.withLock {
                    val status = withContext(Dispatchers.IO) {
                        runCatching { repository.getContinuousFolderSyncStatus() }.getOrNull()
                    }
                    val activeProfileCount = status?.activeProfileCount ?: 0L
                    val waitingMessage = waitingSummary
                    val profilesAreAllowed = hasAllowedProfiles
                    when {
                        !waitingMessage.isNullOrBlank() && !profilesAreAllowed && activeProfileCount == 0L -> {
                            cancelRetryWakeup()
                        }
                        (status?.errorProfileCount ?: 0L) > 0L -> {
                            armOutageRetry(currentErrorMessage(status))
                        }
                        (status?.runningProfileCount ?: 0L) > 0L -> {
                            clearRetryState()
                        }
                    }
                    FolderSyncStatusPollSnapshot(
                        status = status,
                        waitingMessage = waitingMessage,
                        hasAllowedProfiles = profilesAreAllowed,
                        retryState = outageRetryStore.state(),
                    )
                }
                val status = snapshot.status
                val waitingMessage = snapshot.waitingMessage
                val activeProfileCount = status?.activeProfileCount ?: 0L
                val (title, detail) = when {
                    !waitingMessage.isNullOrBlank() && !snapshot.hasAllowedProfiles && activeProfileCount == 0L -> {
                        "Waiting for allowed network" to waitingMessage
                    }
                    (status?.errorProfileCount ?: 0L) > 0L -> {
                        "BerryKeep sync paused" to buildRetryMessage(
                            currentErrorMessage(status),
                            snapshot.retryState,
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
        scope.launch {
            reconcileMutex.withLock {
                withContext(Dispatchers.IO) {
                    repository.stopAllContinuousFolderSync()
                    waitingSummary = null
                    hasAllowedProfiles = false
                    lastDesiredSignature = null
                    clearRetryState()
                }
            }
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
            FolderSyncExecutionCoordinator.awaitOneShotCompletion()
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
        if (
            trigger == FolderSyncRetryTrigger.MANUAL_SYNC ||
            trigger == FolderSyncRetryTrigger.CONFIGURATION_CHANGED
        ) {
            clearRetryState()
            lastDesiredSignature = null
        }
        val retryState = outageRetryStore.state()
        if (!FolderSyncOutageRetryPolicy.allowsAttempt(retryState, trigger, System.currentTimeMillis())) {
            val state = retryState
            if (hasAllowedProfiles) {
                schedulePersistedRetryWakeup(state)
            }
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

        if (
            FolderSyncOutageRetryPolicy.shouldForceReconcileAfterAllowedAttempt(
                retryState,
                outageRetryArmed,
            )
        ) {
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

    private fun notifyNetworkPolicyChanged(reason: String) {
        if (::networkPolicyChangeNotifier.isInitialized) {
            networkPolicyChangeNotifier.submit(reason)
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
        reconcileAfterNetworkPolicyEvaluation(
            reason = reason,
            trigger = FolderSyncRetryTrigger.NETWORK_AVAILABLE,
        )
    }

    private suspend fun processNetworkPolicyChange(reason: String) {
        reconcileAfterNetworkPolicyEvaluation(
            reason = reason,
            trigger = FolderSyncRetryTrigger.NETWORK_POLICY_CHANGED,
        )
    }

    /**
     * Refreshes the policy snapshot before handling an available-network event. A default-network
     * callback is allowed to arrive without a separate capabilities callback, so using the stale
     * offline value here could otherwise leave a persisted endpoint wake-up cancelled forever.
     */
    private suspend fun reconcileAfterNetworkPolicyEvaluation(
        reason: String,
        trigger: FolderSyncRetryTrigger,
    ) {
        FolderSyncExecutionCoordinator.awaitOneShotCompletion()
        val started = reconcileMutex.withLock {
            enforceNetworkPolicyLocked()
            reconcileRequestLocked(
                reason = reason,
                trigger = trigger,
            )
        }
        if (started) {
            startStatusLoop()
        }
    }

    /**
     * Applies changed roaming, SSID, and transport permissions even when an endpoint circuit is
     * cooling down. It stops only newly disallowed profiles and does not issue network traffic.
     */
    private suspend fun enforceNetworkPolicyLocked() {
        withContext(Dispatchers.IO) {
            val enabledProfiles = IronmeshPreferences
                .getFolderSyncConfigs(applicationContext)
                .filter { profile -> profile.enabled }
            val evaluations = FolderSyncNetworkGate.evaluateProfiles(applicationContext, enabledProfiles)
            val allowedProfiles = evaluations
                .filter { evaluation -> evaluation.decision.allowed }
                .map { evaluation -> evaluation.profile }
            val blockedProfiles = evaluations.filterNot { evaluation -> evaluation.decision.allowed }

            blockedProfiles.forEach { evaluation ->
                repository.stopContinuousFolderSync(evaluation.profile.id)
                Log.i(
                    TAG,
                    "stopped continuous sync profile=${evaluation.profile.id} after network policy change: ${evaluation.decision.reason}",
                )
            }

            hasAllowedProfiles = allowedProfiles.isNotEmpty()
            waitingSummary = blockedProfiles.firstOrNull()?.let { evaluation ->
                buildWaitingSummary(
                    blockedProfileCount = blockedProfiles.size,
                    profileLabel = evaluation.profile.label,
                    reason = evaluation.decision.reason,
                )
            }
            if (allowedProfiles.isEmpty()) {
                cancelRetryWakeup()
            }
        }
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
        private const val ACTION_RESTORE = "io.ironmesh.android.action.FOLDER_SYNC_RESTORE"
        private const val ACTION_SYNC_NOW = "io.ironmesh.android.action.FOLDER_SYNC_NOW"
        private const val ACTION_BACKOFF_TIMER = "io.ironmesh.android.action.FOLDER_SYNC_BACKOFF_TIMER"
        private const val ACTION_APP_FOREGROUNDED = "io.ironmesh.android.action.FOLDER_SYNC_APP_FOREGROUNDED"
        private const val ACTION_LOCAL_FOLDER_CHANGED = "io.ironmesh.android.action.FOLDER_SYNC_LOCAL_FOLDER_CHANGED"
        private const val ACTION_STOP = "io.ironmesh.android.action.FOLDER_SYNC_STOP"
        private const val LOCAL_CHANGE_DEBOUNCE_MS = 5_000L
        private val localChangeLock = Any()
        private val triggerScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        @Volatile
        private var serviceRunning = false
        private val lastLocalChangeElapsedMsByTreeUri = mutableMapOf<String, Long>()

        fun syncConfigChanged(context: Context) {
            startOwnedService(context, ACTION_REFRESH)
        }

        /** Restores the service after app/process startup without resetting endpoint backoff. */
        fun syncRuntimeRestored(context: Context) {
            startOwnedService(context, ACTION_RESTORE)
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
            val appContext = context.applicationContext
            triggerScope.launch {
                if (IronmeshPreferences.getFolderSyncConfigs(appContext).none { profile -> profile.enabled }) {
                    return@launch
                }
                runCatching { startOwnedService(appContext, ACTION_APP_FOREGROUNDED) }
                    .onFailure { error ->
                        Log.w(TAG, "failed to signal app foreground to sync service: ${error.message}")
                    }
            }
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
            val elapsedMs = SystemClock.elapsedRealtime()
            synchronized(localChangeLock) {
                val previousElapsedMs = lastLocalChangeElapsedMsByTreeUri[treeUriString]
                if (
                    previousElapsedMs != null &&
                    elapsedMs - previousElapsedMs < LOCAL_CHANGE_DEBOUNCE_MS
                ) {
                    return
                }
                lastLocalChangeElapsedMsByTreeUri[treeUriString] = elapsedMs
            }
            val appContext = context.applicationContext
            triggerScope.launch {
                val hasMatchingEnabledProfile = IronmeshPreferences
                    .getFolderSyncConfigs(appContext)
                    .any { profile ->
                        profile.enabled && profile.localFolderTreeUri == treeUriString
                    }
                if (!hasMatchingEnabledProfile || !serviceRunning) {
                    synchronized(localChangeLock) {
                        if (lastLocalChangeElapsedMsByTreeUri[treeUriString] == elapsedMs) {
                            lastLocalChangeElapsedMsByTreeUri.remove(treeUriString)
                        }
                    }
                    return@launch
                }
                // This is delivered only to an already-running foreground service. It avoids
                // trying to start a foreground service from a background ContentObserver.
                runCatching {
                    appContext.startService(
                        Intent(appContext, FolderSyncForegroundService::class.java).apply {
                            action = ACTION_LOCAL_FOLDER_CHANGED
                        },
                    )
                }.onFailure { error ->
                    Log.w(TAG, "failed to signal local folder change to sync service: ${error.message}")
                }
            }
        }

        /**
         * Delivers a due retry only to an already-running foreground service.
         *
         * Android 12+ can reject a foreground-service start from WorkManager while the app is in
         * the background. When no service is alive, [FolderSyncOutageRetryWorker] instead queues
         * a constrained [FolderSyncWorker], which is safe for background execution.
         */
        fun signalScheduledRetryIfRunning(context: Context): Boolean {
            if (!serviceRunning) {
                return false
            }
            val appContext = context.applicationContext
            return runCatching {
                appContext.startService(
                    Intent(appContext, FolderSyncForegroundService::class.java).apply {
                        action = ACTION_BACKOFF_TIMER
                    },
                )
                true
            }.onFailure { error ->
                Log.w(TAG, "failed to signal sync service for outage retry: ${error.message}")
            }.getOrDefault(false)
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
